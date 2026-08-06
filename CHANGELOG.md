# Changelog

All notable changes to xyzDB are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows [Semantic Versioning](https://semver.org/).

> **The public history of xyzDB begins at 1.0.** Per-version release notes live in [`docs/releases/`](docs/releases/). The condensed development history from 0.1 to 1.0 is archived in [`docs/releases/CHANGELOG-pre-1.0.md`](docs/releases/CHANGELOG-pre-1.0.md) — its commit, branch, and file references belong to the private repo and do not resolve here. For the current design, read [`docs/architecture.md`](docs/architecture.md) and [`OPERATIONS.md`](OPERATIONS.md); this changelog tracks 1.0 forward.

## [Unreleased]

## [1.1.1] — the doors the last fix missed

Patch: **no behaviour change, no on-disk format change, no wire change.** The one
new response field is additive JSON. A 1.1.0 data directory opens unchanged and a
1.1.0 client keeps working.

### Fixed

- **Two more anchor checks distrust a post-recovery bloom.** 1.1.0 armoured the
  duplicate-anchor check on the single-`PUT` path: after an unclean restart, a
  bloom-gated "absent" is re-confirmed without the bloom before it is believed. The
  identical decision was left bloom-gated in **`PUT BATCH`** and in **`AUTOANCHOR
  APPLY`** — one defect class, three doors, one closed. A false negative in the
  batch path writes a second record under a `UNIQUE` anchor; in the populate path
  it re-indexes a value that already has an entry, silently repointing the anchor
  at a different record while reporting the write as "indexed". Both now share the
  single-`PUT` confirmation, its counter and its warning, through one function
  (`ops::put::anchor_dict_get`) that every caller must go through — the previous
  shape was three copies of a decision, which is how two of them stayed wrong.
  Each door has a forged-bloom test **and a negative control that asserts the
  defect with the armouring off**, because a guard that cannot be shown to be
  load-bearing is decoration.
- **The ghost writes counter too.** `load_total_writes` was the last bloom-gated
  point read of the set. Its cost is the mildest — a false "absent" resets a
  promotion counter, so a ghost that had earned auto-creation earns it again — and
  it is closed anyway, because leaving one door of a class open is how the class
  survives.
- **`KNOWN-ISSUES.md` named the wrong statement.** It reported a duplicate check in
  `DECLARE ANCHOR`; there is none — `ANCHOR … UNIQUE IN` registers the anchor and
  never touches the dictionary. The bloom-gated read was in `AUTOANCHOR APPLY`, an
  operational populate step, which is a different exposure profile: an operator runs
  it deliberately, rather than every application issuing it. The correction is
  recorded in that file rather than quietly applied.
- **The spec called `LOBE` idempotent. It is not** — re-declaring an existing lobe
  is refused with `INVALID_QUERY`, and so is re-declaring an anchor. Making
  re-declaration a no-op is the better contract and is a behaviour change, so the
  sentence is corrected here and the change waits for a minor.
- **`docs/mcp-integration.md` commanded an image tag that was never published.**
  Four `docker run` lines named `xyzdb-mcp:1.1.0`; GHCR carries `1.0.1` only. They
  now name the tag that exists.

### Added

- **The engine version is queryable over the protocol.** `STATS`, HTTP `/stats` and
  the `/health` probe carry a `version` key. Additive and diagnostic: `alive` keeps
  its place in the health payload, so a checker reading only that field is
  unaffected. Until now the only way to learn which engine you were talking to was
  to run `--version` against the binary inside the container — which an operator
  holding a connection cannot do, and which is step one of the format-mismatch
  procedure in `OPERATIONS.md` §8.4.
- **A known issue for the WAL gap in derived state.** Ghost entries, rollup deltas
  and the `AUTOANCHOR APPLY` dictionary insert are written straight to a memtable,
  outside the WAL, so a crash after an acknowledged write can lose them while the
  record survives. No acknowledged record is lost and no `UNIQUE` constraint
  weakens — the asymmetry is in derived state. `REFRESH GHOST` is the repair. Filed
  rather than fixed because the fix moves a read-modify-write inside the fsync
  barrier, which is a write-path latency change a patch must not make unmeasured.
- **Per-file `SPDX-License-Identifier: BUSL-1.1`** across the tree.

### Changed

- **Docs the integration exercise found wrong or missing**: a retry-safety table
  and the idempotent-write pattern (anchored `PUT` + `ON CONFLICT UPDATE`), the
  fact that identifiers are interpolated and only *values* can be bound, that an
  upsert **merges** rather than replaces, and that a link written by the standalone
  `LINK` statement is **not visible to `PULL`** — with `GRAVITY BY` on the child
  lobe as the supported idiom. The `ROADMAP` entry calling the satellite axis
  "design in validation" now describes what actually remains (re-packing existing
  records); the axis shipped in 1.1.0.
- **Removed the dead `EngineConfig.worker_threads`.** Nothing read it, and fourteen
  test files set it — which is the argument for deleting it rather than filing it:
  a knob that looks load-bearing and is not had already fooled the people writing
  the tests.

## [1.1.0] — 2026-08-03 — sub-gravity, and detectors that speak

Narrative, migration notes and declared costs: [`docs/releases/v1.1.0.md`](docs/releases/v1.1.0.md).

Minor, not patch: new grammar, new response fields and new `STATS` / `/metrics` / `describe_lobe` surface — all additive and backward compatible. 1.1 carries its own BUSL Change Date (2029-09-01).

### Added

- **Sub-gravity axis — `SATELLITE BY <field> IN "lobe"`.** Splits one gravity bucket into sub-buckets so `SCAN`/`AGGREGATE`/`NEAREST` pinning both the gravity and satellite fields read only the matching rows. Opt-in per lobe; a pure optimisation (same rows, same order). Rules and the upsert caveat: `docs/xytalk-spec.md` §2.2.2.
- **`NEAREST` is bounded by the satellite** when the query pins that field — the exact top-k of the filtered set instead of scoring the whole bucket.
- **`budget_stop` on a truncated `NEAREST`** (`examined` / `candidates` / `found` / `strategy`): the counts at the latency-airbag cut, plus which traversal produced the partial. Present only on that frame; every other response is byte-identical. `strategy` defines two values but this release emits only `"score_order"` — the field ships ahead of the second traversal so clients key off the fact instead of assuming it. Spec §2.20.
- **Invariant guards are observable state.** `STATS` gains `invariant_guards` and `recovered_from_wal`; `/metrics` gains the matching series. Correctness signals, not capacity metrics, and they never reset. They do **not** all mean the same thing when non-zero, and the difference decides whether you report an engine bug or note a known defect being met — `OPERATIONS.md` §5 is the source for which is which.
- **The satellite axis is discoverable.** `SHOW PROFILE` reports it and the MCP `describe_lobe` tool gained a `satellite` field, so an agent can see the axis it is meant to query along.
- **`SHOW PROFILE` reports the gravity axis, and `describe_lobe` exposes it.** The profile listed `Pinned`, `Vector`, `Satellite` and `Learned` — every declaration except the primary one — so an agent reading a lobe over MCP could discover the satellite axis without discovering the bucket it subdivides. The line is additive with the same three-state shape as `Vector:`/`Satellite:`, the MCP parser tolerates an engine that does not emit it, and `gravity` is hoisted to the top level of the description beside `vector` and `satellite`.

### Fixed

- **A `UNIQUE` anchor can no longer be duplicated after an unclean restart.** A process that replayed WAL re-confirms an anchor miss without the bloom before trusting it. Has a declared cost while armed.
- **Table ids are monotonic across restarts**, so one identity can never name two different table contents.
- **`PUT … ON CONFLICT UPDATE` notifies ghosts.** An upsert skipped the ghost hook, so an aggregate ghost kept serving its pre-upsert sums and counts as current. **Ghost state is persisted, so run `REFRESH GHOST` once after upgrading** for aggregate ghosts over lobes that take upserts — the fix stops new drift, it does not repair drift already recorded.
- **`PUT … ON CONFLICT UPDATE` writes through the record cache.** A cached read could return the record as it was before the upsert — a read-your-own-write violation. The cache is in-memory, so the upgrade restart clears any staleness; no action needed.
- **A ghost-routed read returned records without the declared vector.** V5 keeps the searchable vector in its own column; the ghost's point-read did not re-attach it. Since ghosts are materialised by the engine from scan telemetry, the same query returned different fields before and after one was built, with nothing written in between — and an unfused `NEAREST` over that scan returned zero rows, which reads as "no matches". No action needed: nothing on disk is wrong, the vector was always in its column.
- **The published amd64 images no longer require AVX2.** `.cargo/config.toml` applies `target-cpu=x86-64-v3` to every x86-64 Linux build, so the amd64 engine and MCP images needed AVX2, FMA and BMI1/2: on an x86-64 without them the process died with `SIGILL` before it could log anything, and nothing in the build prevented it — the Dockerfiles' `if TARGETARCH = amd64` block was a log line, and `TARGETARCH` is empty unless the build uses buildx, so on a classic builder it printed "arm baseline" while compiling for x86_64. Both images now carry **both** builds and a small `xyzdb-launch` entrypoint picks one from the CPU's actual feature set — the whole `x86-64-v3` set, not just AVX2, since a CPU with AVX2 but no BMI2 would still fault. It prints its choice to stderr (stdout stays clean for the MCP stdio transport), falls back to the baseline if the preferred build cannot be exec'd, and refuses to start rather than guess if neither is present. This is safe because the two builds are not two answers: v2 and v3 produce byte-identical scores, gated in CI by `crates/core/tests/score_bit_identity.rs`. **No action needed and no interface change** — same tags, same arguments, same behaviour; a modern host gets the same binary it got before. Images grew ~10 MB and the amd64 build takes about twice as long. The `x86-v3` image label is gone: an image that runs on both is not a v3 image.
- **`xyzdb-server --version`.** `OPERATIONS.md` §8.4 makes it the first diagnostic step of a format-mismatch incident and the binary answered `error: unexpected argument '--version' found`.
- **`PUT BATCH … ON CONFLICT UPDATE` updates instead of inserting a duplicate.** In a batch, a record whose anchor value already existed was neither updated nor skipped — it was inserted, leaving two records under a field declared `ANCHOR … UNIQUE`. The single-statement form was always correct. The clause names the semantics, so the batch now updates too: the collision resolves to the owning LID and is applied through the same `execute_upsert` the single statement uses, which is what makes the ghost notification, the record-cache write-through and the V5 vector re-hoist happen — merging inline in the batch loop would have re-opened the upsert-ghost leak on a second path. Two records sharing an anchor value *inside* one batch now collapse the same way: the first inserts, the second merges onto it. Without the clause, a collision is still `DuplicateAnchor` for the whole batch. **Two declared consequences:** the update is in place and does **not** re-bucket, so a batch upsert that changes the gravity field or the satellite axis leaves the record in its old bucket (same as the single form, spec §2.2.2); and a batch that mixes inserts with updates is no longer all-or-nothing across both halves — the inserts commit atomically, the updates follow as separate commits. The batch response now reads "records written" rather than "records inserted", since under this clause it is both.
- **A longer pipeline no longer shrinks a `NEAREST`'s candidate set.** The fused plan was gated on the pipeline being exactly `SCAN | NEAREST`, so appending a step (`| SHAPE {id}`) dropped the query into the generic loop, where the scan materialises one 1000-record page and `NEAREST` ranks inside it. The fused plan is now chosen whenever the pipeline *starts* with those two steps.

### Changed

- **The MCP `query` and `stats` tools describe what 1.1 added.** The served xyTalk grammar gained `SATELLITE BY` — an agent could read the axis back from `describe_lobe` but had nothing telling it how to declare one — and a "partial results" section separating `has_more` + `cursor` (an ordinary next page) from `budget_stop` (a `NEAREST` cut by the latency airbag, `cursor: null`, no page to resume), including the refusal of a mutating step after one. The `stats` description now distinguishes the two correctness signals from the capacity numbers. A new test walks every `Statement` variant the parser accepts and requires the served grammar to either mention it or mark it deliberately unadvertised with a reason; the match is exhaustive, so a new statement does not compile until that decision is made.
- **A truncated `NEAREST` refuses a mutating or aggregating next step.** When the latency airbag cuts the candidate set, a following `SET`, `DELETE` or `AGGREGATE` now errors instead of running: those results cannot carry `budget_stop`, so the write or the total would silently cover only the part that was scored. Read-only steps still compose and the flag travels with them.
- **A parse error no longer hands the caller `nom`'s internal `Debug`.** `SHOW BANANAS` returned `Parsing Error: Error { input: "BANANAS", code: Tag }` — a combinator name, which says nothing about what was expected and whose shape belongs to a dependency, so a `nom` upgrade could change it under clients. One wrapper now produces `could not parse from: 'BANANAS' — check the statement's grammar in docs/xytalk-spec.md`, and it replaced **all 35** formatting sites so the leak cannot come back through one nobody rewrote. Statement-specific messages still win where they exist — a WHERE-less `DELETE` still names `PURGE`, a `FIND` with `OR` still points at `SCAN`. What is still missing is expected-token detail, tracked in `KNOWN-ISSUES.md`. The wire `code` was and remains `PARSE_ERROR`.
- **Errors and log lines state limitations instead of naming a version.** `CURSOR` with `ORDER BY` was refused with "not supported in v0.2.5.1; paginated sort lands in v0.3" — a stale version and a plan that never shipped, in an error a client reads. It now says why a sorted page cannot be resumed from a key-ordered cursor and what to do instead. Same treatment for the TLS chunked-streaming refusal, the incompatible-on-disk-format error, the ghost-metadata skip warning, the MCP public-host warning, and two `/metrics` `# HELP` strings that carried internal work-item codenames. No behaviour change; the `CURSOR`+`ORDER BY` error keeps its opening phrase, which is what the test asserts.
- **`COMPACT` / `ANALYZE` / `BULKMODE` / `MIGRATE` are permanent aliases; no retirement is planned.** The v0.2.5.1 deprecation announced removal in v0.3.0, and the server kept announcing it on every invocation through 1.1.0 — an operator reading that log plans a migration that never arrives. The recommendation is worth keeping and stays: prefer `xyzdb-cli admin <verb>` so housekeeping stays out of application query paths. The deadline is withdrawn. The statements keep parsing, the `tracing::warn` now names the preferred form without a version, and `--help` and spec §2.21 say the same. Nothing to migrate.

## [1.0.0] — 2026-07-30 (1.0 launch)

The 1.0 release of xyzDB under the Business Source License 1.1. This entry records the launch-defining work; the pre-1.0 `0.9.x` line was internal iteration and is not itemised here.

### Added

- **Wire protocol specification (`PROTOCOL.md`).** The request framing (V1–V4), the bearer-token preamble, the response envelope, the 16 MiB frame cap, and the HTTP surface multiplexed onto the same TCP port are now specified directly from the implementation, so third parties can write compatible clients. The protocol may be implemented freely, by anyone, under any license (see `NOTICE`).
- **Business Source License 1.1 licensing package.** `LICENSE` (Change Date 2029-08-01, Change License Apache-2.0), `NOTICE`, `TRADEMARKS.md`, `PERMISSIONS.md`, and `docs/license-change-dates.md`. The entire repository, including the reference client under `examples/`, is BUSL-1.1; the installable client packages (`xyzdb` on crates.io, PyPI, and npm) are Apache-2.0 and ship from a separate repository. `.ci/license-version-parity.sh` guards that the version and Change Date stay aligned across `LICENSE`, the workspace manifest, the change-dates table, and `NOTICE`.
- **Apache-2.0 client packages ship with 1.0.** `xyzdb` on crates.io, PyPI, and npm — the installable clients, published from a separate repository under Apache-2.0 so an application can depend on a client without taking the BUSL-1.1 engine license.

### Changed

- **Loopback bind by default (`xyzdb-server`).** The server binds `127.0.0.1` by default. Binding a non-loopback address without `--auth-token` is refused with a non-zero exit; `--insecure-allow-no-auth` overrides it for a trusted network. The container image still commands `0.0.0.0`, so a plain `docker run` without a token fails safe rather than exposing an open server.
- **`STATS`, `SHOW STATS` and `/metrics` follow the token.** They return the engine stats snapshot, so when `--auth-token` is set they now require authentication; the `/health` and `/ready` liveness probes stay on the unauthenticated allowlist. In one line: authentication applies to everything except the liveness probes.

### Fixed

- **Record boxes in the text protocol align to their widest line.** The `FIND`/`PULL` record box shown by `xyzdb-cli` used fixed column widths: the right border sat one column short of the content rows, and any LID or field value past the hard-coded width overflowed and broke the box. It is now sized to the widest content line, so the border always closes flush regardless of value length. Text output carries no compatibility guarantee (see `PROTOCOL.md`); this only changes on-screen rendering.
- **`xyzdb-mcp --connect` authenticates.** `--connect` now reads `XYZDB_TOKEN` and sends the bearer-token preamble, so it works against a server started with `--auth-token` instead of failing the handshake.
- **`NEAREST` degrades instead of failing when the latency budget expires mid-hydration.** A highly selective residual filter (fewer than `k` rows pass) forces `NEAREST` to descend the whole bucket in score order; when that crossed the wall-clock budget the query previously returned an error, turning a legitimate small answer into a failure. It now returns the highest-scoring passers found within budget as `PaginatedRecords { has_more: true, cursor: None }` — a prefix-correct partial. The budget stays a latency wall, never a recall wall; the unbounded scoring scan still fails fast as before. See `docs/xytalk-spec.md` §2.20.
