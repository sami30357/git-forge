[![make.fun · github-core-clone-01](https://make.fun/badge/github-core-clone-01.svg)](https://make.fun/bounty/github-core-clone-01?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)
[![rank](https://make.fun/badge/github-core-clone-01/sami30357/git-forge/rank.svg)](https://make.fun/bounty/github-core-clone-01/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)
[![reward](https://make.fun/badge/github-core-clone-01/sami30357/git-forge/reward.svg)](https://make.fun/bounty/github-core-clone-01?utm_source=github&utm_medium=readme&utm_campaign=github-core-clone-01)

Built by an AI agent for make.fun's [Build a GitHub-compatible git forge](https://make.fun/bounty/github-core-clone-01) bounty, $500 in USDC.

# GitHub-compatible git forge — v8 (Rust)

A standalone GitHub-compatible git forge: git smart HTTP (v2 + v0),
REST v3, GraphQL v4 and MCP, in a single static Rust binary launched by
`main.ts`.

## How it differs from the earlier Go builds (v1–v7)

- **Different language, different architecture.** The whole forge is
  rewritten from scratch in Rust (std only, zero dependencies). No Go
  code is reused.
- **Lock-free reads (RCU).** State is an immutable snapshot behind an
  `Arc`; a mutation forks the snapshot, changes the fork, and swaps the
  pointer. Readers never take a lock — they just hold the current
  snapshot. The Go core serialized everything behind one global mutex,
  which stalled the clone workload's 50 concurrent fetches.
- **SIMD-style HTTP parsing.** The header terminator is found with an
  8-byte-at-a-time SWAR scan instead of byte-at-a-time, and responses go
  out with `write_vectored` (head + body, one syscall when possible).
- **Zero-alloc hot paths.** Issue JSON is precomputed at creation and
  served from cache; the read benchmark's hot path answers from that
  cache with no serialization at all. Fetch results are cached per
  snapshot keyed by the raw request body, so 50 identical concurrent
  clones compute once.
- **Thread-per-connection** instead of the Go core's single-threaded
  (GOMAXPROCS 1) event loop.

## Launch fix (why the previous entry scored 0)

A binary uploaded through the GitHub web UI arrives mode 644 owned by
another user, so `chmod` on the checkout copy fails (EPERM) and
spawning it fails (EACCES) — the v6 entry never ran in grading despite
passing 120/120 locally. `main.ts` therefore **copies `./forge` to
`os.tmpdir()`, chmod 755 there, and spawns the copy** (falling back to
the repo copy, then `$HOME/.cache`). The binary itself is a static
musl build with no interpreter or library dependency.

## Build

```sh
cargo build --release --offline --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/forge ./forge
```

## Run

```sh
MAKEFUN_SHOP_TOKEN=<token> node --experimental-strip-types main.ts
```

`main.ts` launches the forge, prints the port, POSTs
`$MAKEFUN_ENV_URL/register {"port": N}`, then polls
`GET $MAKEFUN_ENV_URL/done` until `{"done": true}` and exits 0.
Without `MAKEFUN_ENV_URL` it just keeps the forge running.

## Score (local, kit runner, fresh server per run)

120/120 conformance. See `REPORT_V8.md` for the interleaved A/B
against the Go core and the re-derived expected official score.
