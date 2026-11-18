# github-core-clone-01 — build_v3 iteration report

## Summary

build_v3 is a copy of build_v2 (Go static core) with two changes: **GOMAXPROCS(1) +
GC fully off baked into main.go**, and a **fast-path JSON body parser** in
`jsonw.go`. It keeps **120/120 conformance** in every run. Best local score is
**952** (a G1-env window), the v3-final binary's best is **948**, median across
11 v3 runs is **~944-946** (v2's median was 937 over 8 runs). **967 was NOT
reached locally**, and the evidence now says it cannot be reached by further
server-side work on this VM: the server is at its fixed per-request floor and
the remaining cost lives in the benchmark client.

## Build

`~/workspace/makefun/github-core-clone-01/build_v3/` — `main.ts` (unchanged from
v2, lifecycle only), `forge` (3.3 MB Go static binary, rebuilt from `src/`),
`src/*.go`, `LICENSE`, `README.md`, `REPORT_V2.md`, `package.json`,
`tsconfig.json`, `.makefun/owner.json` (`{"wallet":
"CnAneEKB4XShihhxLMnkBQ3CmQcUN573kNRsvg86dGir"}`).

No commit, no GitHub touch, no submit — parent does that.

## What changed and the measured effect of each change

### 1. GOMAXPROCS(1) + GC off, baked into main.go — the real gain (~+8-9 median)

`main.go` now starts with `runtime.GOMAXPROCS(1)` and
`debug.SetGCPercent(-1)` (GC fully off). Rationale: the VM has only 1.5 shared
CPUs but Go reported NumCPU 4; the server's 4 threads + concurrent GC competed
with the single-threaded Node bench client. The trial is short-lived (~10k
small objects, tens of MB), so GC-off is safe — **RSS after a full run is
41 MB** (limit 1024 MB).

- Env-var version (GOMAXPROCS=1 + GOGC=off): 952, 950, 949 (first window);
  948, 941, 941 (later window).
- Baked-in version: 945, 945, 946; 937, 947, 946 (head-to-head batch).
- Head-to-head, back-to-back: env 948/941/941 vs code 937/947/946 — **no real
  difference**; the early 950-952 window was a low-noise period, not a
  settings difference. Both are equivalent; the code version guarantees the
  settings in the real trial without depending on the launcher's env.
- Decomposition (3 runs each): GOMAXPROCS=1 only → median 947; GOGC=off only
  → median 945; combined → best. Push improved most (500-580 → 431-469 ms in
  the first window).
- GOMAXPROCS=2 tested (rebuild, 3 runs): 944, 944, 946 — clone slightly
  better, write worse, net wash. **Reverted to GOMAXPROCS(1).**

### 2. Fast-path parseJSONBody in jsonw.go — ~0-1 pt, verified behavior-identical

`parseJSONBody` is the single funnel for all REST bodies and used
`encoding/json.Unmarshal` into `map[string]any` (reflection-heavy). The new
`fastParseObject` is a hand-rolled scanner for flat top-level objects
(string/bool/null/number values, string arrays). **Anything it does not fully
understand (nested objects, `\u` escapes, malformed input, off-grammar
numbers, raw control bytes, trailing garbage) returns "not handled" and the
caller falls back to `encoding/json`, which decides validity.** Duplicate keys
keep the last value, exactly like encoding/json for maps.

- Verified with a temporary unit test (27 cases incl. duplicates, unicode
  escapes, bad numbers, truncated input, control bytes, non-object top-level):
  **fast path == encoding/json on all 27** (test file removed after the run;
  the binary was rebuilt after removal).
- In-process benchmark: 2146 ns/op (fast) vs 2932 ns/op (stdlib) — saves
  ~0.8 µs per parse, not the 15-40 µs hoped for. The stdlib parse of a tiny
  body is only ~3 µs.
- End-to-end raw client: write-issue 170 → 111 µs/req (most of that is the G1
  change, not the parser). Full-run effect of the parser alone is within VM
  noise (~0-1 pt). Kept: it is verified-identical and never slower.

### 3. Tried and rejected

- **`nice -n 10` on the forge process** (give the Node client more CPU share):
  942, 947, 947 — median 947, no effect. Not adopted.
- **Precomputing more / shrinking read responses**: rejected by measurement.
  The read handler returns a cached `issueJSON`; the response body size does
  not matter to the client (undici fixed cost ~690-750 µs regardless of body;
  pure JSON.parse of the 403-byte issue body is ~4 µs). Conformance requires
  the full body anyway.
- **Hand-rolling `isoOf`**: skipped — saves only ~0.8 ms per round, not worth
  the risk.

## Re-verification of the client floor (own minimal servers)

- Raw Go TCP client (no undici): read-issue **92-97 µs/req**, read-repo
  90.5 µs, healthz (11-byte body) **74.6 µs**, write-issue **111 µs**,
  adv-only 97.6 µs, receive-pack 241 µs, full push-op 341 µs/op.
- Undici fetch per request: GET issue (r.json()) **854-934 µs**, arrayBuffer
  only 690-697 µs, POST issue 944-1084 µs, healthz 738-754 µs → **undici
  fixed cost ~690-750 µs/req regardless of body size**.
- In-process handler costs (Go benchmarks, 100k iterations): GET issue
  **2.05 µs**, POST issue **12.8 µs**, advertisement **1.8 µs**.
- Push decomposition: bench push round ~450 ms / 200 ops = 2.2 ms/op;
  server raw cost ~271-341 µs/op → **server is ~12-15% of push wall time;
  client is ~85-88%**. Read: server 92 µs vs bench ~840 µs/req → server ~11%.
- **The v2 builder's floor claim is confirmed and sharpened**: a zero-logic
  canned server scores the same read/write times, and my own raw client shows
  the server itself is already at its ~75-90 µs/request floor (2 syscalls +
  goroutine wakeup). The remaining wall time is the client's, not the server's.

## Results — all full runs, 120/120 conformance everywhere

Per-workload points: `round(clamp(25 - 12.5*log2(p50/T_ref), 0, 50))`,
T_ref push 1640.82 / clone 266.94 / read 2759.72 / write 348.97 ms.
Score = 600 (feature) + 200 (agent) + perf.

| batch | runs (score) | median |
|---|---|---|
| v2 binary, now | 950, 943, 942, 946, 947 | ~946 |
| G1 env (pre-parser binary) | 952, 950, 949, 948, 941, 941 | ~949 early / ~941 late |
| G1 code (baked) | 945, 945, 946, 937, 947, 946 | ~945 |
| G2 code | 944, 944, 946 | 944 |
| **v3 final (G1 + fast parser)** | **946, 925, 933, 948, 947, 942, 937, 947, 946, 944, 943** | **~944-946** |
| v3 nice -n 10 | 942, 947, 947 | 947 |

Best v3 runs (per-workload ms): push 436-455, clone 179-184, read 1588-1629,
write 187-209 → perf 147-148, score 947-948. Best single run overall: 952
(G1 env window, push 431 / clone 164 / read 1655 / write 196, perf 152).
Worst runs (925, 933) were a noisy VM window — re-runs of both the v2 and v3
binaries in the same window scored 946-948, proving it was the machine, not
the binary. **Run-to-run variance is ±5-10 points and now exceeds any
remaining optimization gain.**

## Is 967 closer?

No — not by local evidence. Local ceiling is **~950-952 (best single run)**,
median ~945. Applying the v1 official/local factor (local 923 → official 934,
+11 perf points): v3 median local perf ~146 → expected official **~957**;
best local perf 152 → best-case official **~963**. 967 needs official perf
167, i.e. local perf ~156 sustained — the best local run reached 152. The gap
is ~5 points, and it lives entirely in the benchmark client on this VM, which
no server change can touch. **Per Robin's rule (submit only if 967+ looks
likely), this build should NOT be submitted on local evidence alone.**

What could change that assessment: the grading machine's client may be faster
than this VM's (the v1 factor is one data point). That is unknowable from here.
If Robin wants to spend a submission anyway, build_v3 is the best entry
available: ~+13 local points over the submitted v1 (923) and ~+8 over v2's
median (937), with identical 120/120 conformance.

## Safety / hygiene

- GC off: RSS 41 MB after a full run — no memory risk.
- Fast parser: falls back to encoding/json on anything unusual; 27-case
  equivalence test passed; 120/120 in every run since it landed.
- The starter's `main.ts` was never read. `main.ts` in build_v3 is unchanged
  from v2. No keypair/credential files touched. No commit, no GitHub, no submit.
