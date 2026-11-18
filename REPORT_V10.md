# github-core-clone-01 — v10 report

**Date:** 2026-11-18 ~13:45 (+06)
**Status:** No new lever found. Build is byte-identical to v9. Expected official ~954 — 960+ is NOT any closer by engineering. The only path left is the grading-run lottery, quantified below.

## What v10 is

`build_v10/` is a copy of `build_v9/` (Rust rewrite, lock-free RCU, SWAR parsing, tmpdir launch fix). No code changed — because every genuinely new idea investigated either measured zero or was proven impossible. The forge binary md5 is unchanged: `5bda14b0b7ad425e9a2e7a66ab96b41e`.

## (a) Genuinely new things tried

### 1. Grading-machine variance / the 960 lottery — NEW analysis (the only new lever)

**What I did:** Downloaded the full leaderboard (7,584 entries) via the make.fun API and analysed the score/p50 distribution instead of guessing.

**Findings:**
- 759 entries sit at score ≥940 (the "floor" population). Only **2 of 759 (0.26%)** are at 960+.
- Rank 1 is 966 (vinzsenzo, p50 322,015µs); rank 2 is 960 (nughair-dex, p50 345,603µs). Both are **13–17% below the dense band** (948–951, stdev 1.9%).
- The band is extremely tight (1.9% stdev). Rank 1/2 are **5–9σ below the band mean** — if the grading variance were really 1.9%, they would be impossible. So either the grading variance is much larger than the band suggests (the band is a *selected* subset — servers that scored low don't show up there), or rank 1/2 have a genuine server advantage I cannot find, or both.
- Local run variance (v8's 8 runs): p50 stdev **7.2%** (mean 160,348µs). If the grading variance is ~7%, then a run landing 13% fast is a ~1.7σ event — rare but plausible per submission.
- Rank 1's title says "v9" (9 iterations) — consistent with a "best of N" selection: submit repeatedly, keep the fast run.

**Quantified odds of 960+ per submission (our entry, expected p50 ~391,600µs, need ≤345,603µs):**
- Pessimistic (band variance 1.9%): ~0.1% — effectively impossible.
- Empirical (2/759 floor entries at 960+): **0.26%**.
- Optimistic (local variance 7.2%): ~5%.
- **Best estimate: ~1% per submission.**
- With ~10 submissions left: cumulative P(at least one 960+) ≈ **3–26%, most likely ~10%**.

**Conclusion:** the only path to 960 is a favourable grading run. Nothing can be done to improve the odds except (a) submitting the best available build (v9/v10, ~954 expected), and (b) submitting more times if the quota allows. This is a lottery ticket, not engineering. The parent/user must decide whether to spend a submit on it.

### 2. Multi-process (main.ts spawning several forge processes + load-balancing) — REJECTED with data

**What I did:** measured whether the server is CPU-bound at all, which is the only condition under which more processes help.

- **Server CPU over a full suite run:** forge used **0.47s CPU (utime 25 + stime 22 ticks) over a ~5s wall run** — ~9% of one CPU, idle ~90% of the time. It is waiting on the client, not the other way around.
- **1-CPU pinning test** (`taskset -c 0`): pinned the forge to a single CPU and re-ran the suite twice. Pinned: perf **200 / 198** (p50 153,634 / 167,923µs). Unpinned baseline: perf **195 / 200** (p50 167,370 / 147,005µs). **Pinning to 1 CPU does not slow it at all** — within noise. The server is definitively NOT CPU-bound.
- Therefore more processes cannot help: there is no CPU bottleneck to parallelise. They would only add overhead (no shared cache across processes, more context switches, extra proxy latency).
- I did NOT build a proxy A/B because it would be uninformative: a TCP proxy adds ~10–30µs/request (~3–8% of a clone round), which masks any benefit smaller than that, and the CPU data already proves there is no benefit to find. The thread-per-connection design already parallelises the only concurrent workload (clone, 50 connections).

### 3. Another language/runtime (Zig, D, etc.) — NOT pursued, reason measured

- `zig` is **not installed** on this VM. D has no toolchain here either.
- v7 already proved the point: a C canned responder and the Rust core **tie** with the Go core on the client-bound workloads (the client's ~660µs/GET cost dominates; the server handler is 1–2µs). A new native language would land in the same tie — it cannot beat the client floor. No measured reason to expect Zig/D to differ, so no build was attempted.

### 4. Reducing the server's own CPU to give the client more CPU on the 1-CPU grading pod — QUANTIFIED, ~0

- Server is ~9% of one CPU locally. Even eliminating server CPU entirely on a 1-CPU grading pod would free ~0.27s over a ~5s run (~5% of client time), worth ~+1–2 points at most — and realistically ~0, because most server CPU is kernel I/O overhead (stime 22 of 47 ticks: syscalls, TCP, scheduling), not reducible handler work.

### 5. Response-size / header hypotheses — CHECKED, no fit

- The read payload is ~40KB (2000 issues). If response size mattered, the reference tiers would scale with bytes; they don't fit a byte-scaling model (see v9 report §3). The client's cost is dominated by undici's own processing, not bytes. No lever.

### 6. The one remaining unknown — rank 1/2's advantage

- Cannot be resolved without a submission. Evidence favours "lottery winners" (both are high-iteration entries; the band is 1.9% tight; our server is already at the client floor). If they have a genuine server advantage, it is not in any of the ~30 levers tried across v1–v10.

## (b) Local verification (build_v10)

| Run | Checks | Perf | p50 (µs) |
|---|---|---|---|
| fresh server 1 | 120/120 | 197 | 163,503 |
| fresh server 2 | 120/120 | 198 | 162,690 |
| fresh server 3 | 120/120 | 193 | 171,801 |
| fresh-clone, user `nobody`, root-owned checkout, binary 644 | 120/120 | 192 | 177,363 |

Combined with v9's fresh runs (191, 196, 200, 195): **8 fresh runs, perf 191–200, median 196** → local score median **996** (600 feature + 200 agent + 196 perf), best **1000**.

**Launch-fix re-verified for v10:** direct spawn of the 644 binary fails `Permission denied` (v6's death, reproduced). `main.ts` as `nobody` copies the binary to `/tmp/forge-*-*`, chmod 755, spawns from there — serves and passes 120/120. The fix is intact.

**Method note:** a second run against the *same* forge instance fails checks (36/120, "name already exists") because the forge holds state in memory. All runs above used a **fresh server per run**, as required. The perf numbers are unaffected by this (perf uses its own repos), but the check count is only valid on a fresh server.

## (c) Re-derived expected OFFICIAL score and rank

- **Expected official: ~954 (952–957), rank ~27 — UNCHANGED from v8/v9.**
- 960 needs perf 160 (+6–8 over ~152–154) — i.e. overall p50 ≤345,603µs vs our expected ~391,600µs (13% faster). No engineering lever in v1–v10 closes that gap; it lives entirely in the grading client's cost and the grading machine's speed.
- **960+ is NOT any closer.** The only change from v9 is that the lottery odds are now quantified (~1% per submission, ~10% over ~10 submissions). Whether to spend a submit on that lottery is the user's call, under his 960 floor rule.

## (d) Build path

`~/workspace/makefun/github-core-clone-01/build_v10/` — copy of `build_v9/`, byte-identical code (forge md5 `5bda14b0b7ad425e9a2e7a66ab96b41e`), LICENSE, README.md, `.makefun/owner.json`, `main.ts` (tmpdir launch fix) all in place. **NOT submitted, NOT committed, GitHub untouched.**

## Bottom line

There is no genuinely new server-side lever left. v1–v10 exhausted ~30 ideas; the server is at the client floor (1–2µs handler vs ~660µs/GET client cost). The remaining question is not "what do we build" but "do we spend a submit on a ~1%-per-shot lottery for 960, or accept ~954 (rank ~27)".
