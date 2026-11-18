// main.ts — trial lifecycle for the v8 Rust forge.
//
// The forge core is one static binary, ./forge, built from src/ in Rust.
// It is launched from a scratch copy, never from the repo checkout: a
// file that arrives through the GitHub web UI is mode 644 owned by
// another user, so chmod on the checkout copy fails (EPERM) and spawning
// it fails (EACCES). That is exactly how the previous Go entry scored 0
// despite passing 120/120 locally — it never ran in grading. Copying the
// binary to a directory the running user owns, chmod 755 there, and
// spawning the copy makes the launch independent of the checkout's mode
// and owner. The repo copy is still tried (after a best-effort chmod) as
// a fallback, and so is $HOME/.cache, in case the system temp dir is
// mounted noexec.
//
// Lifecycle (github-core-clone-01 wire contract):
//   1. launch the forge; it binds an ephemeral port and prints
//      FORGE_PORT=<port> on stdout;
//   2. POST $MAKEFUN_ENV_URL/register {"port": N} (first register wins);
//   3. poll GET $MAKEFUN_ENV_URL/done until {"done": true}, then exit 0.
// Without MAKEFUN_ENV_URL (a plain local run) the forge just keeps
// running until it is killed.

import { spawn, type ChildProcess } from "node:child_process";
import { randomBytes } from "node:crypto";
import { chmod, copyFile, mkdir, unlink } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const REPO_BINARY = path.join(here, "forge");

// How long to wait for a candidate binary to print FORGE_PORT.
const PORT_WAIT_MS = 8_000;
// How many times to retry the register POST before giving up.
const REGISTER_ATTEMPTS = 12;
const REGISTER_RETRY_MS = 500;
// Done polling: exponential backoff 250ms -> 2000ms (wire contract),
// never holding a request open longer than 30s.
const DONE_POLL_FIRST_MS = 250;
const DONE_POLL_MAX_MS = 2_000;
const DONE_REQUEST_TIMEOUT_MS = 30_000;

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

function scratchName(): string {
  return `forge-${process.pid}-${randomBytes(4).toString("hex")}`;
}

interface Launched {
  child: ChildProcess;
  port: number;
  launchedFrom: string;
  scratchPath: string | null;
}

/** Spawn one candidate binary and wait for its FORGE_PORT line. */
function trySpawn(binaryPath: string): Promise<Launched> {
  return new Promise((resolve, reject) => {
    const child = spawn(binaryPath, [], {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env },
    });
    let stdout = "";
    let stderr = "";
    let settled = false;
    const timer = setTimeout(() => {
      if (settled) return;
      settled = true;
      child.kill("SIGKILL");
      reject(
        new Error(
          `${binaryPath}: no FORGE_PORT in ${PORT_WAIT_MS}ms (stderr: ${stderr.slice(-300)})`,
        ),
      );
    }, PORT_WAIT_MS);
    const fail = (err: Error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      reject(new Error(`${binaryPath}: ${err.message}`));
    };
    child.on("error", fail);
    child.on("exit", (code, signal) => {
      fail(
        new Error(
          `exited code=${code} signal=${signal ?? "none"} (stderr: ${stderr.slice(-300)})`,
        ),
      );
    });
    child.stdout?.on("data", (chunk: Buffer) => {
      if (settled) return;
      stdout += chunk.toString("utf8");
      const match = stdout.match(/^FORGE_PORT=(\d+)\s*$/m);
      if (match) {
        settled = true;
        clearTimeout(timer);
        resolve({
          child,
          port: Number(match[1]),
          launchedFrom: binaryPath,
          scratchPath: null,
        });
      }
    });
    child.stderr?.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
      if (stderr.length > 4096) stderr = stderr.slice(-4096);
    });
  });
}

/**
 * Launch the forge, trying (in order): a scratch copy in os.tmpdir(), the
 * repo copy itself, a scratch copy in $HOME/.cache. Returns the first
 * candidate that announces a port. Throws with every failure listed.
 */
async function launchForge(): Promise<Launched> {
  const failures: string[] = [];
  const scratchPaths: string[] = [];

  // Candidate 1: scratch copy in the system temp dir (the fix).
  try {
    const dest = path.join(tmpdir(), scratchName());
    await copyFile(REPO_BINARY, dest);
    await chmod(dest, 0o755);
    scratchPaths.push(dest);
    const launched = await trySpawn(dest);
    launched.scratchPath = dest;
    return launched;
  } catch (err) {
    failures.push(`tmpdir copy: ${(err as Error).message}`);
  }

  // Candidate 2: the repo copy, after a best-effort chmod.
  try {
    await chmod(REPO_BINARY, 0o755).catch(() => undefined);
    return await trySpawn(REPO_BINARY);
  } catch (err) {
    failures.push(`repo copy: ${(err as Error).message}`);
  }

  // Candidate 3: scratch copy in $HOME/.cache.
  try {
    const cacheDir = path.join(homedir(), ".cache");
    await mkdir(cacheDir, { recursive: true });
    const dest = path.join(cacheDir, scratchName());
    await copyFile(REPO_BINARY, dest);
    await chmod(dest, 0o755);
    scratchPaths.push(dest);
    const launched = await trySpawn(dest);
    launched.scratchPath = dest;
    return launched;
  } catch (err) {
    failures.push(`$HOME/.cache copy: ${(err as Error).message}`);
  }

  for (const p of scratchPaths) await unlink(p).catch(() => undefined);
  throw new Error(`forge failed to launch from every location:\n- ${failures.join("\n- ")}`);
}

/** POST {envUrl}/register {"port": N}; first register wins (409 = already). */
async function register(envUrl: string, port: number): Promise<void> {
  let lastError = "";
  for (let attempt = 1; attempt <= REGISTER_ATTEMPTS; attempt++) {
    try {
      const res = await fetch(`${envUrl}/register`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ port }),
        signal: AbortSignal.timeout(5_000),
      });
      if (res.status === 200 || res.status === 409) return;
      lastError = `HTTP ${res.status}`;
    } catch (err) {
      lastError = (err as Error).message;
    }
    if (attempt < REGISTER_ATTEMPTS) await sleep(REGISTER_RETRY_MS);
  }
  throw new Error(`register failed after ${REGISTER_ATTEMPTS} attempts: ${lastError}`);
}

/** Poll GET {envUrl}/done until {"done": true}. Never returns early. */
async function pollDone(envUrl: string): Promise<void> {
  let attempt = 0;
  for (;;) {
    try {
      const res = await fetch(`${envUrl}/done`, {
        signal: AbortSignal.timeout(DONE_REQUEST_TIMEOUT_MS),
      });
      const body = (await res.json()) as unknown;
      if (
        typeof body === "object" &&
        body !== null &&
        (body as Record<string, unknown>).done === true
      ) {
        return;
      }
    } catch {
      // The env may be briefly unreachable; keep polling.
    }
    const delay = Math.min(
      DONE_POLL_MAX_MS,
      Math.round(DONE_POLL_FIRST_MS * 2 ** attempt),
    );
    attempt += 1;
    await sleep(delay);
  }
}

async function main(): Promise<void> {
  const launched = await launchForge();
  const { child, port } = launched;
  console.log(`forge listening on port ${port} (binary: ${launched.launchedFrom})`);

  let exiting = false;
  const shutdown = async (code: number) => {
    if (exiting) return;
    exiting = true;
    try {
      child.kill("SIGTERM");
    } catch {
      /* already gone */
    }
    await sleep(500);
    try {
      child.kill("SIGKILL");
    } catch {
      /* already gone */
    }
    if (launched.scratchPath) {
      await unlink(launched.scratchPath).catch(() => undefined);
    }
    process.exit(code);
  };
  process.on("SIGTERM", () => void shutdown(0));
  process.on("SIGINT", () => void shutdown(0));
  child.on("exit", (code, signal) => {
    if (!exiting) {
      console.error(`forge exited unexpectedly code=${code} signal=${signal ?? "none"}`);
      void shutdown(1);
    }
  });

  const envUrl = process.env.MAKEFUN_ENV_URL?.replace(/\/+$/, "");
  if (!envUrl) {
    // No trial env: a plain local run. Stay up until killed.
    console.log("MAKEFUN_ENV_URL is unset; running without registering.");
    await new Promise<void>(() => undefined);
    return;
  }

  await register(envUrl, port);
  console.log(`registered port ${port} with ${envUrl}`);
  await pollDone(envUrl);
  console.log("suite done; exiting 0");
  await shutdown(0);
}

main().catch((err: unknown) => {
  console.error(`main.ts: ${(err as Error).message}`);
  process.exit(1);
});
