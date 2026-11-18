# REPORT_V8 — full Rust rewrite of the github-core-clone-01 forge

Date: 2026-11-18. Status: **complete, tested to the last-chance standard.
NOT submitted, NOT committed, GitHub untouched** (parent handles that only
on the user's go-ahead).

## (a) What was built and how it differs from the Go core (v1–v7)

`~/workspace/makefun/github-core-clone-01/build_v8/` — a **full fresh
rewrite in Rust, std only, zero dependencies**, ~8,000 lines across 13
modules. No Go code reused; the Go core was studied for behaviour only.

| Area | Go core (v6) | Rust v8 |
|---|---|---|
| Language / runtime | Go, GOMAXPROCS(1), GC off | Rust, static musl binary, no runtime |
| Concurrency | single global mutex serialises everything | **RCU / lock-free reads**: immutable `Arc<RepoState>` snapshots; mutation forks the snapshot and swaps the pointer; readers never lock |
| HTTP parsing | byte-at-a-time | **SWAR 8-byte-at-a-time scan** for `\r\n\r\n`; `write_vectored` responses |
| Hot paths | zero-alloc where cached | issue JSON precomputed at creation and served from cache; **fetch cached per snapshot keyed by raw request body** (50 identical concurrent clones compute once, hit 49 times) |
| Threading | single-threaded event loop | thread-per-connection |
| Everything else | — | SHA-1, zlib inflate/deflate, JSON (Go-compatible writer), base64, git pack read/write with OFS/REF deltas, merge-base/diff/merge, REST v3, GraphQL v4, MCP — all reimplemented from the wire contract |

Launch: `main.ts` (fresh, 263 lines, written from the wire contract in
`program/env/src/wire.ts` — the starter's main.ts was never read).

## (b) Local scores — interleaved A/B vs the Go core, fresh server per run

Kit runner `sdk/run.ts --perf`, token `abtest`. **All 16 runs 120/120.**

Rust v8 (musl, launched via main.ts):

| round | score | push ms | clone ms | read ms | write ms | p50 µs |
|---|---|---|---|---|---|---|
| 1 | 1000 | 237.5 | 58.6 | 476.1 | 78.5 | 151,004 |
| 2 | 997 | 198.0 | 63.6 | 525.4 | 105.2 | 162,433 |
| 3 | 997 | 202.3 | 68.0 | 494.4 | 102.5 | 162,526 |
| 4 | 1000 | 214.8 | 52.7 | 522.8 | 80.6 | 147,796 |
| 5 | 999 | 212.9 | 62.4 | 561.7 | 94.3 | 162,864 |
| 6 | 994 | 202.6 | 74.7 | 745.5 | 101.5 | 183,939 |
| 7 | 994 | 210.1 | 56.1 | 547.4 | 118.5 | 166,250 |
| 8 | 1000 | 226.2 | 49.6 | 549.6 | 73.7 | 145,974 |
| **median** | **997.5** | **211.5** | **60.5** | **536.4** | **97.9** | **162,480** |

Go v6:

| round | score | push ms | clone ms | read ms | write ms | p50 µs |
|---|---|---|---|---|---|---|
| 1 | 1000 | 206.0 | 66.9 | 501.1 | 76.3 | 151,490 |
| 2 | 1000 | 256.6 | 57.8 | 593.2 | 88.8 | 167,160 |
| 3 | 999 | 245.0 | 71.9 | 585.6 | 74.0 | 166,197 |
| 4 | 1000 | 226.0 | 56.8 | 536.9 | 84.8 | 155,492 |
| 5 | 994 | 235.5 | 73.1 | 564.5 | 109.9 | 180,760 |
| 6 | 998 | 230.6 | 73.4 | 661.9 | 73.6 | 169,468 |
| 7 | 999 | 238.0 | 64.8 | 545.0 | 91.4 | 166,496 |
| 8 | 991 | 195.7 | 51.9 | 512.1 | 146.9 | 166,209 |
| **median** | **999** | **233.1** | **65.9** | **554.8** | **86.8** | **166,353** |

Head-to-head (medians): Rust faster on push (~9%), clone (~8%, the
lock-free reads help a little), read (~3%); slower on write (~13% — but
both ranges overlap heavily, Rust 74–119 ms vs Go 74–147 ms, so this is
noise-dominated); aggregate p50 **162,480 µs vs 166,353 µs (~2.3% faster)**.
Score medians 997.5 vs 999 — a statistical tie at the local ceiling; both
hit local 1000.

## (c) Expected OFFICIAL score — and the honest verdict on 967+/1000

**Expected official: ~954 (range 952–957), rank ~27.** Same as the Go
core's estimate. The ~2% local edge is inside grading-machine variance and
does not move the estimate.

- **967+ is NOT reached** by this rewrite either.
- **1000 official remains impossible** per the v5–v7 client-floor proof:
  it needs read ≤690 ms = ≤345 µs/GET officially, while the grading
  client's own per-GET cost is ~660 µs. No server can move that.
- Rank 1's 966 is partly a favourable grading run (machine effect); matching
  its technique (SIMD parsing, zero-alloc, lock-free reads — all done here)
  gains ~2% locally, not the ~13 points to 967.

**What v8 actually changes:** it will RUN in grading. v6 scored 0 (rank
7916) because it never launched. The current final entry is that 0. v8 at
~954 would be ~rank 27 — just outside the top-20 prize zone, but a real
score instead of a zero. That, not a higher number, is the last-chance value
of this build. If the user wants a prize-zone rank, no server-side build in
v1–v8 has shown a path there; the binding constraint is the grading client,
not the server.

## Launch fix + fresh-clone end-to-end test (the exact path where v6 died)

`main.ts` launch order: (1) copy `./forge` to `os.tmpdir()`, chmod 755,
spawn the copy; (2) repo copy after best-effort chmod; (3) `$HOME/.cache`
copy. It waits for `FORGE_PORT=<port>` on stdout (8 s per candidate) and
throws listing every failure if all three fail. On exit it SIGTERMs the
forge, then unlinks the scratch copy.

Fresh-clone test, 2026-11-18 ~13:13 +06 — **PASSED**:
- Checkout `/tmp/freshclone` (copy of the submission contents), root-owned,
  `forge` mode **644** — simulating a GitHub web-UI upload.
- Entry run as user **`nobody` (uid 65534)**.
- `main.ts` copied the binary to `/tmp/forge-17764-54b95289`
  (**uid 65534, mode 755**) and spawned it from there.
- Fake env (wire contract: POST /register, GET /done, real kit suite):
  register OK, `/healthz` ×2 OK, kit **120/120, local score 1000**
  (push 234.0, clone 67.1, read 563.3, write 80.4), `/done` → true,
  **entry exited 0**. ENV_EXIT=0, ENTRY_EXIT=0.
- Direct-spawning the mode-644 binary fails with EACCES even as root —
  reproducing the v6 failure mode exactly, and confirming the tmpdir copy
  is what saves it.
- SIGTERM path verified separately: node exits 0, scratch copy unlinked,
  forge child dead.

Two harness bugs found and fixed during testing (both mine, neither in the
submission): the A/B script captured a subshell pid instead of node's, so
early Rust runs leaked idle processes (their numbers are unaffected — idle
processes take no CPU, and round 4 with 3 leaks present was the fastest);
a `pkill -f` pattern matched my own shell. The clean 3-round re-run with
the fixed harness reproduces the same numbers, leak check 0.

## (d) Build path

```sh
cd ~/workspace/makefun/github-core-clone-01/build_v8
cargo build --release --offline --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/forge ./forge
```

Binary: `forge`, ELF x86-64, **static-pie linked** (musl, no interpreter
or library dependency), 1,114,920 bytes, stripped,
BuildID `f0e0dd9e4557a650c7d783e300cdbb47550e74e9`. Toolchain rustc/cargo
1.100.0; musl target verified installed. The gnu build also passes 120/120
but the musl static binary is the one in the repo.

Run: `MAKEFUN_SHOP_TOKEN=<token> node --experimental-strip-types main.ts`
(registers with `$MAKEFUN_ENV_URL`, polls `/done`, exits 0; without the env
var it just serves until killed).

## Submission checklist (for the parent — DO NOT submit without the user's go-ahead)

Include: `main.ts`, `package.json`, `tsconfig.json`, `README.md`,
`LICENSE`, `.gitignore`, `.makefun/owner.json`, `forge` (musl static;
mode 644 in the repo is fine — the launch fix makes the mode irrelevant),
`src/*.rs` (13 files), `Cargo.toml`, `Cargo.lock`, `REPORT_V8.md`.
**Exclude: `target/`** (covered by `.gitignore`).
Quota: 1 submit left today (resets 2026-11-19T00:00Z = 06:00 Dhaka Nov 19);
deadline Nov 21 06:30Z. "All okay" per the user's rule: 120/120 ✓, launch
verified end-to-end from a fresh clone as a non-root user ✓, expected
official score reported above ✓.

## Levers measured (steer 3)

- Lock-free RCU reads: clone 65.9 → 60.5 ms median (~8%), the one real
  technique gap vs rank 1 — helps, but only ~1–2 official points.
- SIMD-style SWAR header parsing + write_vectored: part of the push/read
  edge (~3–9%), no score change at the ceiling.
- Native rewrite (Rust vs Go): ~2% aggregate — the server was already
  1–8% of wall time; the grading client dominates.
- Socket options / CPU affinity / busy-spin: re-tested in v5–v7, noise or
  worse; not re-adopted. io_uring: EPERM-blocked (v7).
- Startup/precompute: issue-JSON caching and fetch caching adopted;
  remaining startup cost is ~10 ms and not scored.
