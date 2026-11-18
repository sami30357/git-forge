# REPORT — github-core-clone-01 forge (first submittable version)

Built 2026-11-18 for the @sami-patoary account, aiming for top 1. Entry lives in
`~/workspace/makefun/github-core-clone-01/build/`:
`main.ts` (trial lifecycle, unchanged from the starter), `app.ts` (the whole
forge, ~124 KB, stdlib only), `package.json`, `tsconfig.json`, `LICENSE`,
`README.md`, `REPORT.md` (this file). Total tree 133 KB — far under the
16 MiB / 8 MiB-per-file limits. Nothing was submitted, no GitHub repo was
touched, no keypair or credential was touched.

## What was built

A single Node process (`node:http` + `node:crypto` + `node:zlib` only) serving
all four surfaces over one in-memory store:

- **git smart HTTP** — v2 advertisement (`version 2`, `ls-refs=unborn`,
  `fetch=shallow`, `object-format=sha1`); v0 advertisement (service line,
  capabilities on the first ref line, `capabilities^{}` pseudo-ref for empty
  repos, peeled `^{}` lines for annotated tags); v2 `ls-refs` (peel, symrefs,
  unborn HEAD line, server-side ref-prefix filtering); v2 + v0 `fetch`
  (thin-pack, `deepen`, shallow/unshallow boundaries, ACK + `ready` / NAK);
  `receive-pack` (create/update/delete, fast-forward check, mixed reports,
  sideband framing, `unpack ok` + `ok`/`ng` lines). Packs are plain objects
  with a correct SHA-1 trailer; incoming packs are read with OFS/REF delta
  resolution.
- **REST v3** — root (`X-OAuth-Scopes: repo, read:org`), meta, user,
  user/repos (create 201 / duplicate 422, paginated with `Link`), repos
  (get/patch/delete 204), branches, commits, git/refs, contents (file and
  directory reads, put 201/200/409 stale-sha, delete), labels, issues
  (+comments, state/sort/pagination), pulls (+files diff, squash merge with
  three-way conflict detection → 405). Every reply is JSON with a non-empty
  `X-GitHub-Request-Id`; 401 without a token; 404 JSON for unknown routes;
  400 for malformed JSON.
- **GraphQL v4** — a small tokenizer/parser/executor for the suite's
  documents: `viewer`, `repository` (issue, issues connection with cursor
  pagination and CREATED_AT ordering, pullRequests, pullRequest with files),
  `node` (opaque base64 global ids, stable across reads), `__type`
  introspection, the five mutations (createIssue, closeIssue,
  createPullRequest, mergePullRequest SQUASH, addComment), batched documents,
  and `Cannot query field "…" on type "…"` errors.
- **MCP** — JSON-RPC 2.0 over Streamable HTTP with plain JSON answers:
  `server/discover` (supportedVersions `2026-07-28`, `2025-11-25`),
  `initialize`, `tools/list` (7 tools, byte-identical across calls),
  `tools/call` (create_repo, get_repo, create_issue, list_issues,
  create_pull_request, merge_pull_request, list_pull_requests), unknown
  tool → -32601, missing argument → -32602, bad JSON → 400 / -32700, no
  token → 401.

Determinism: timestamps from a counter off the scenario epoch, request ids
from a counter, packs built from oid-sorted objects. The viewer is the
scenario's fixed account (`adaiversen`); any other owner is a 404, never a 403.

## Local conformance (sdk/run.ts — byte-identical to the sealed suite)

Run 2026-11-18 ~11:00 +06 against a locally started server
(`MAKEFUN_SHOP_TOKEN=test-token-123`, `node --experimental-strip-types
/tmp/forge-run-local.ts`), with `--perf`:

```
passed: 120 / 120
  git 30/30 · repos 15/15 · issues 15/15 · pulls 20/20
  protocol 5/5 · graphql 20/20 · mcp 15/15
columns: feature 600 · agent 200 · perf 123 · score 923 / 1000 · working 1
```

The lifecycle was also smoke-tested end to end against a fake env:
`main.ts` bound an ephemeral port, POSTed it to `/register`, polled `/done`,
and exited 0.

## Workloads (median of 3 rounds, same run)

| workload | p50 | T_ref (reference forge) | ratio | points (max 50) |
|----------|------|------------------------|-------|-----------------|
| push  | 672 ms  | 1640.82 ms | 0.41× | 41 |
| clone | 282 ms  | 266.94 ms  | 1.06× | 24 |
| read  | 2187 ms | 2759.72 ms | 0.79× | 29 |
| write | 285 ms  | 348.97 ms  | 0.82× | 29 |
| **perf_p50_us** | **586411** | — | — | **123 / 200** |

Per-workload points: `round(clamp(25 − 12.5·log2(t/T_ref), 0, 50))`.

## Bugs found and fixed during testing

1. Two TypeScript parameter properties (`constructor(private …)`) — not
   erasable syntax under type stripping; rewritten as plain field assignments.
2. GraphQL mutation payload types (`CreateIssuePayload` etc.) were missing
   from the field table, so every mutation field errored — added.
3. The fetch closure followed commit parents past the depth boundary, so a
   depth-1 clone carried the whole history — replaced with a depth-limited
   closure (commit → tree/blobs only, no parents beyond the boundary).
4. The `have` closure followed parents through the client's shallow
   boundaries, so a deepen-2 fetch delivered nothing — the have closure now
   stops at shallow commits (git's negotiation rule), which is what makes
   deepen deliver the parent.

## Honest assessment vs the house baseline

- **Conformance is complete: 120/120, feature 600 + agent 200.** Nothing in
  the suite is left unimplemented. The score's only gap is perf (123/200).
- **The reference forge's calibration (T_ref) implies it scores ~25 points
  per workload (100 perf) if it passes everything, i.e. ~900 total.**
  This entry at **923** is ahead of that on the numbers available — but the
  house baseline is entered 4h before the deadline and its real score is
  unknown; the margin is not large.
- **Weakest workload: clone (24 pts, 1.06× T_ref).** It is also the noisiest:
  the same server scored 27 pts (p50 244 ms) on the previous run. The clone
  path is 4 requests per clone (advertisement, ls-refs, advertisement,
  fetch) plus a small depth-1 pack; the cost is mostly per-request HTTP +
  pack deflate overhead, not data volume.
- **Read (29 pts, 0.79×)** is 2,000 sequential GETs on one keep-alive
  connection; per-request cost is a Map lookup + JSON.stringify. Push
  (41 pts) and write (29 pts) are comfortably ahead of the reference.
- **What would still improve it:** (a) clone: cache the advertisement /
  ls-refs bytes per repo generation and skip re-serializing the pack for
  identical depth-1 fetches; (b) read: a fast path that skips the generic
  router for `GET /issues/{n}` and avoids re-allocating the response object
  shape; (c) push: batch the pack-store writes. Each is a small, safe change
  — none of them is needed for correctness, and none should be attempted
  without re-running the full suite afterwards, since the replay export must
  stay byte-identical.
- **Caveats:** the local run is evidence, not a guarantee — the final grade
  runs the same checks on fresh seeded data in the trial pod. The local
  token was a test token; the trial sets `MAKEFUN_SHOP_TOKEN` from the env.
  The entry has not been submitted yet (submit, repo, bundle and claim are
  the parent agent's steps, not this build's).
