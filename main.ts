// The forge's trial lifecycle: start the Go core, announce ourselves to the
// grading environment, then wait quietly until the suite finishes.
//
// The environment hands us its address through MAKEFUN_ENV_URL. The dance is
// three moves: launch ./forge (it prints FORGE_PORT=<port> on stdout), tell
// the environment which port it is, and keep asking whether the trial is over
// until the answer turns to yes. Nothing else happens here — every git, REST,
// GraphQL and MCP answer comes from the Go core in src/.

import { spawn, type ChildProcess } from 'node:child_process';
import { chmod } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// How long to wait before asking again, and how long each ask may hang.
const FIRST_WAIT_MS = 250;
const LONGEST_WAIT_MS = 2000;
const ASK_TIMEOUT_MS = 10000;

// How long we keep trying to announce ourselves before giving up.
const ANNOUNCE_WINDOW_MS = 15000;

// The exit code once the environment reports the trial as finished.
const EXIT_DONE = 0;
const EXIT_FAILED = 1;

function nap(milliseconds: number): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, milliseconds);
  });
}

function envOrigin(): string {
  const raw = process.env.MAKEFUN_ENV_URL ?? '';
  const trimmed = raw.trim().replace(/\/+$/, '');
  if (trimmed === '') {
    throw new Error('MAKEFUN_ENV_URL is not set: the grading environment never introduced itself');
  }
  return trimmed;
}

function withPath(origin: string, path: string): string {
  return `${origin}${path.startsWith('/') ? path : `/${path}`}`;
}

function forgeBinaryPath(): string {
  const here = dirname(fileURLToPath(import.meta.url));
  return join(here, 'forge');
}

async function startForge(): Promise<{ child: ChildProcess; port: number }> {
  const binary = forgeBinaryPath();
  // The binary may have arrived from a git push with its executable bit
  // stripped; restore it before launching.
  await chmod(binary, 0o755).catch(() => undefined);
  const child = spawn(binary, [], { stdio: ['ignore', 'pipe', 'inherit'] });
  const port = await new Promise<number>((resolve, reject) => {
    let seen = '';
    const onData = (chunk: Buffer): void => {
      seen += chunk.toString('utf8');
      const match = seen.match(/FORGE_PORT=(\d+)/);
      if (match !== null) {
        child.stdout?.off('data', onData);
        resolve(Number(match[1]));
      }
    };
    child.stdout?.on('data', onData);
    child.once('error', (err) => {
      child.stdout?.off('data', onData);
      reject(new Error(`could not launch the forge core: ${err.message}`));
    });
    child.once('exit', (code) => {
      child.stdout?.off('data', onData);
      reject(new Error(`the forge core exited before announcing a port (code ${code})`));
    });
  });
  return { child, port };
}

async function announce(origin: string, port: number): Promise<void> {
  const started = Date.now();
  let pause = 100;
  for (;;) {
    let reply: Response;
    try {
      reply = await fetch(withPath(origin, '/register'), {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ port }),
        signal: AbortSignal.timeout(ASK_TIMEOUT_MS),
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      if (Date.now() - started > ANNOUNCE_WINDOW_MS) {
        throw new Error(`could not reach the environment to register: ${message}`);
      }
      await nap(pause);
      pause = Math.min(pause * 2, 1000);
      continue;
    }
    if (reply.status === 200) return;
    if (reply.status === 409) return; // somebody already announced us; the first one stands
    if (Date.now() - started > ANNOUNCE_WINDOW_MS) {
      throw new Error(`the environment refused the registration with HTTP ${reply.status}`);
    }
    await reply.arrayBuffer().catch(() => undefined);
    await nap(pause);
    pause = Math.min(pause * 2, 1000);
  }
}

async function trialFinished(origin: string): Promise<boolean> {
  let reply: Response;
  try {
    reply = await fetch(withPath(origin, '/done'), {
      method: 'GET',
      headers: { accept: 'application/json' },
      signal: AbortSignal.timeout(ASK_TIMEOUT_MS),
    });
  } catch {
    return false; // a dropped ask is not an answer; ask again next round
  }
  if (!reply.ok) {
    await reply.arrayBuffer().catch(() => undefined);
    return false;
  }
  let payload: unknown;
  try {
    payload = await reply.json();
  } catch {
    return false;
  }
  return (
    typeof payload === 'object' &&
    payload !== null &&
    (payload as Record<string, unknown>).done === true
  );
}

async function waitForFinish(origin: string): Promise<void> {
  let pause = FIRST_WAIT_MS;
  for (;;) {
    if (await trialFinished(origin)) return;
    await nap(pause);
    pause = Math.min(pause * 2, LONGEST_WAIT_MS);
  }
}

async function run(): Promise<number> {
  const origin = envOrigin();
  const { child, port } = await startForge();

  const stop = (): void => {
    child.kill('SIGTERM');
    setTimeout(() => process.exit(EXIT_DONE), 1000).unref();
  };
  process.on('SIGTERM', stop);
  process.on('SIGINT', stop);
  child.once('exit', () => {
    process.stderr.write('forge: the core exited early\n');
    process.exit(EXIT_FAILED);
  });

  await announce(origin, port);
  await waitForFinish(origin);
  child.kill('SIGTERM');
  return EXIT_DONE;
}

run()
  .then((code) => process.exit(code))
  .catch((err: unknown) => {
    process.stderr.write(`forge lifecycle failed: ${err instanceof Error ? err.message : String(err)}\n`);
    process.exit(EXIT_FAILED);
  });
