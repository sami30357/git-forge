# github-core-clone-01 — build_v4 report

## Summary

build_v4 is a copy of build_v3 (Go static core) with one focused change set:
**the push path is de-allocated** — a delta-free fast path in `readPack`,
a `fmt.Sprintf`-free `hashObject`, a pre-sized inflater, byte-level command
parsing, a single-buffer report build, and a direct-parent fast path in
`isDescendant`. It keeps **120/120 conformance** in every run. Best local
score on this VM is **998**, median **~994** — statistically identical to
the v3 copy on the same VM (best 999–1000, median ~997). **The change is real
in micro-benchmarks but sub-noise at bench level, because the remaining wall
time lives in the benchmark client, not the server.** 967 was NOT reached
locally, and the expected official score is **~955 (±7)** — below the 967
bar, so per Robin's rule this build should NOT be submitted on local
evidence alone.

## Build

`~/workspace/makefun/github-core-clone-01/build_v4/` — `main.ts` (unchanged
from v2/v3, lifecycle only), `forge` (3.3 MB Go static binary, rebuilt from
`src/` with `CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath
-ldflags='-s -w'`), `src/*.go`, `LICENSE`, `README.md`, `REPORT_V2.md`,
`REPORT_V3.md`, `package.json`, `tsconfig.json`, `.makefun/owner.json`
(`{"wallet": "CnAneEKB4XShihhxLMnkBQ3CmQcUN573kNRsvg86dGir"}`).

No commit, no GitHub touch, no submit — parent does that. The starter's
`main.ts` was never read.

## Part 1 — Full analysis of v1, v2, v3 (what each version actually did)

### Per-workload times (local runner, `sdk/run.ts --perf`, median runs)

| version | arch | push | clone | read | write | perf | score |
|---------|------|------|-------|------|-------|------|-------|
| v1 (Node) | one Node process, no cache | 672 | 282 | 2187 | 285 | 123 | 923 |
| v2 (Go) | raw-TCP Go core + caches + pooled zlib | ~505 | ~218 | ~1867 | ~246 | ~137 | ~937 |
| v3 (Go) | v2 + GOMAXPROCS(1) + GC off + fast JSON | ~450 | ~180 | ~1620 | ~200 | ~146 | ~945 |
| v3 copy, **this VM** | same binary, new VM | ~215 | ~61 | ~550 | ~105 | ~197 | ~997 |
| v1 (Node), **this VM** | same binary, new VM | ~310 | ~97 | ~615 | ~95 | ~191 | ~991 |

Points: `round(clamp(25 − 12.5·log2(t/T_ref), 0, 50))`,
T_ref push 1640.82 / clone 266.94 / read 2759.72 / write 348.97 ms.

### What each change actually gained (old VM numbers)

- **v1 → v2 (Node → Go raw TCP + caches + pooled zlib): +14 median perf
  (123 → 137), +21 best.** Push 672→505 ms (generation-keyed
  advertisement/fetch caches + pooled zlib: `writePack` 1.7 ms → 48 µs,
  `readPack` ~110 µs → ~60 µs), clone 282→218 ms (fetch cache: 49 of 50
  clone fetches are cache hits), read 2187→1867 ms (raw TCP + cached issue
  JSON), write 285→246 ms. This was the big architectural win; everything
  after it is single-digit points.
- **v2 → v3 (GOMAXPROCS(1) + GC off + fast-path JSON): +8–9 median
  (137 → ~146).** Push improved most (505→~450 ms). The fast JSON parser
  was worth only ~0–1 pt (stdlib parse of a tiny body is ~3 µs; the fast
  path saves ~0.8 µs). GOMAXPROCS(2) tested: wash, reverted. `nice` tested:
  no effect, not adopted.
- **v3 → v4 (this report): ~0 at bench level.** See Part 2.

### Where the remaining time goes (measured on this VM)

Server handler pure CPU (Go micro-benchmarks, 200k iters): GET issue
**1.1 µs**, POST issue **8.7 µs**, receive-pack **51.6 µs**
(`readPack` 21 µs of it), clone upload-pack ~9 µs. Live handler times
(server-side timers during a bench run): GET issue 1–2 µs, POST issue
13 µs, receive-pack 74–115 µs, upload-pack 9–10 µs.

Bench wall time per op on this VM: push ~1035 µs (2 requests), clone
~1360 µs (6 requests, 50 concurrent), read ~264 µs, write ~540 µs.
**The server is ~1–8% of bench wall time; the client is ~92–99%.**
Raw Go client (no undici): read 71 µs/req, write 78 µs/req. Undici
(Node fetch): GET issue ~209–304 µs, POST issue ~417–649 µs.

### The VM changed — the old "local ceiling" is void

The VM was replaced mid-day (uptime 3 min when v4 work started, 4 CPUs
visible, load ~0.8). The old VM charged ~30 µs/syscall and ~900 µs per
undici GET; this VM charges ~12 µs/syscall and ~250 µs per undici GET.
**v2/v3's "local ceiling ~952" was a property of the old VM, not the
entry.** On this VM the same v3 binary scores 995–1000 and v1 scores 991.
Local absolute scores therefore cannot be compared across VMs — only
same-VM, same-window comparisons are meaningful.

### The POST ≈ 2× GET gap is client-side (proven three ways)

Undici POST costs ~2× undici GET (479 vs 209 µs p50) with a Go client
showing no gap (78 vs 71 µs). The gap persists against a trivial
net/http server (679 vs 323 µs), is body-size independent, and the
server receives both in a single Read parsed in 2 µs. It is undici/Node
request-body handling, and **no server can remove it**. The write
workload (200 sequential POSTs) carries ~210 µs/op of fixed client cost.

### Ideas carried forward vs dropped

- **Carried forward:** Go raw-TCP core, GOMAXPROCS(1), GC off, all
  generation-keyed caches, cached issue JSON, fast-path JSON, pooled
  zlib, stack response headers, single `writev`.
- **Dropped / rejected:** v0-only answers and bench-repo detection
  (disqualification risk, per RULES.txt); epoll rewrite (unproven, high
  risk, gains unclear — the floor is 2 syscalls/request, which is already
  the minimum for HTTP/1.1); GOMAXPROCS(2) (measured wash); `nice`
  (no effect); response-size trimming (client cost is fixed, not
  size-dependent); hand-rolled `isoOf` (0.2 µs — already fast).

## Part 2 — What v4 changed and the measured effect of each change

All in `src/` (a copy of v3; v3's files untouched):

1. **`readPack` delta-free fast path (`gitobj.go`)** — the biggest
   remaining server cost was `readPack` (21 µs pure CPU, 51.6 µs for the
   whole receive-pack handler). The old code built 4 maps, a `raw` slice
   of heap structs, and a recursive resolve closure for every pack —
   even though every pack the suite's client sends is delta-free (plain
   blob/tree/commit). The new `readPackFlat` makes one pass with a single
   result map; the moment it meets a type-6/7 delta entry it falls back
   to the unchanged general path, so delta packs behave exactly as
   before. Verified by test: flat path taken for the bench pack.
   Micro-benchmark: ReadPack 21.2 → 21.3 µs (no change — the maps were
   only ~2 µs; inflate dominates). **Verdict: correct but no bench gain.**
2. **`hashObject` without `fmt.Sprintf` (`gitobj.go`)** — stack header
   buffer, pooled `sha1` hasher, stack hex encode. Micro-benchmark:
   2.62 → 2.34 µs (-11%). **Verdict: real, tiny.**
3. **Pre-sized inflater (`gitobj.go`)** — `inflate` now takes the
   header's size and reads exactly that much (`ReadFull` + EOF probe)
   instead of `ReadAll`'s grow-by-doubling. Same errors for corrupt
   packs. **Verdict: real, tiny.**
4. **Byte-level command parsing + single-buffer report (`git.go`)** —
   receive-pack commands parsed as bytes (no `strings.Split` /
   `TrimSuffix` / intermediate string); the sideband report assembled in
   one buffer instead of a `[][]byte` + two `concatBytes` passes.
   **Verdict: real, tiny.**
5. **`isDescendant` direct-parent fast path (`treeops.go`)** — a
   single-commit push's parent IS the ancestor; answer with one lookup
   and no map/queue, falling back to the BFS otherwise.
   **Verdict: real, tiny.**

Micro-benchmarks (200k iters, GC-on, ±15% noise): HandleReceivePack
51.6 → 59.6 µs (noise — no improvement measurable), HandlePostIssue
8.7 → 7.9 µs, HandleGetIssue 1.1 → 0.7 µs, HashObject 2.62 → 2.34 µs,
ReadPack 21.2 → 21.3 µs. **The inflate (Huffman table setup per tiny
zlib stream) is ~26% of the handler and cannot be beaten with Go's
`flate`; that is the remaining server cost and it is ~13 µs per push.**

## Results — all full runs, 120/120 conformance everywhere

| binary | runs (score) | best | median |
|--------|--------------|------|--------|
| v3 copy (this VM) | 996, 999, 995, 997, 999, 996, 998, 991, 998 | 999–1000 | ~997 |
| **v4** | 994, 993, 990, 996, 993, 998 | **998** | **~994** |
| v1 Node (this VM) | 991, 991 | 991 | 991 |

Head-to-head, same window, alternating: baseline 991/998, v4 993/998.
Per-workload v4 (median of 6 runs): push ~237 ms, clone ~77 ms,
read ~633 ms, write ~106 ms. **v4 ≈ v3 within VM noise (±5–10 pts
run-to-run, which exceeds any remaining optimization gain).**

## Expected official score and the 967 question

Three anchors, all official:

- v1 official: **934** (perf 134, p50 493185 µs). v1 on this VM: 991
  (p50 ~205000 µs) → grading machine ≈ 2.41× slower (geomean) than
  this VM. Applying that factor to v4's local geomean (~186 ms) gives
  official ~948 (best-run ~948, median ~943).
- Rank 1 (vinzsenzo, Go): **966** (perf 166, p50 322015 µs). If its
  server is near-zero, the grading client floor is ~322 ms geomean.
  Our server adds only ~2.3 ms geomean locally; at any plausible
  server scaling (2–4×) that is 5–9 ms officially → official
  ~327–331 ms → score ~965.
- @sami07 Go entry official: **952** (perf 152, p50 392263 µs) — a Go
  static core like ours.

Reconciled: **expected official ~950–965, most likely ~955–960**.
967 is at the top of that range and is NOT established — it requires
our server to be essentially as fast as rank 1's on the grading
machine AND the client floor to be at its lowest plausible value.
**Per Robin's rule (submit only if 967+ looks likely), this build
should NOT be submitted.** If he wants to spend a submission anyway,
v4 is (tied for) the best entry available: ~+14 local points over the
submitted v1 on the old VM's scale, with identical 120/120 conformance.

What could change that assessment: nothing measurable from here. The
gap to rank 1 lives in the grading machine's client, which no server
change can touch (proven three ways in Part 1).

## Safety / hygiene

- GC off: RSS after a full run ~41 MB (limit 1024 MB) — no memory risk.
- Fast paths fall back to the general code on anything unusual (delta
  packs, unknown types, corrupt packs); 120/120 in every run since they
  landed. No behaviour change for any conforming input.
- The starter's `main.ts` was never read. No keypair/credential files
  touched. No commit, no GitHub, no submit.
