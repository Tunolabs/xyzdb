# Changelog

All notable changes to xyzDB are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows [Semantic Versioning](https://semver.org/).

> **The public history of xyzDB begins at 1.0.** Per-version release notes live in [`docs/releases/`](docs/releases/). The condensed development history from 0.1 to 1.0 is archived in [`docs/releases/CHANGELOG-pre-1.0.md`](docs/releases/CHANGELOG-pre-1.0.md) — its commit, branch, and file references belong to the private repo and do not resolve here. For the current design, read [`docs/architecture.md`](docs/architecture.md) and [`OPERATIONS.md`](OPERATIONS.md); this changelog tracks 1.0 forward.

## [Unreleased]

## [1.1.0] — sub-gravity, and detectors that speak

Narrative, migration notes and declared costs: [`docs/releases/v1.1.0.md`](docs/releases/v1.1.0.md).

Minor, not patch: new grammar, new response fields and new `STATS` / `/metrics` / `describe_lobe` surface — all additive and backward compatible. 1.1 carries its own BUSL Change Date (2029-09-01).

### Added

- **Sub-gravity axis — `SATELLITE BY <field> IN "lobe"`.** Splits one gravity bucket into sub-buckets so `SCAN`/`AGGREGATE`/`NEAREST` pinning both the gravity and satellite fields read only the matching rows. Opt-in per lobe; a pure optimisation (same rows, same order). Rules and the upsert caveat: `docs/xytalk-spec.md` §2.2.2.
- **`NEAREST` is bounded by the satellite** when the query pins that field — the exact top-k of the filtered set instead of scoring the whole bucket.
- **`budget_stop` on a truncated `NEAREST`** (`examined` / `candidates` / `found` / `strategy`): the counts at the latency-airbag cut, plus which traversal produced the partial. Present only on that frame; every other response is byte-identical. `strategy` defines two values but this release emits only `"score_order"` — the field ships ahead of the second traversal so clients key off the fact instead of assuming it. Spec §2.20.
- **Invariant guards are observable state.** `STATS` gains `invariant_guards` and `recovered_from_wal`; `/metrics` gains the matching series. Correctness signals, not capacity metrics — any non-zero is an engine bug.
- **The satellite axis is discoverable.** `SHOW PROFILE` reports it and the MCP `describe_lobe` tool gained a `satellite` field, so an agent can see the axis it is meant to query along.
- **`SHOW PROFILE` reports the gravity axis, and `describe_lobe` exposes it.** The profile listed `Pinned`, `Vector`, `Satellite` and `Learned` — every declaration except the primary one — so an agent reading a lobe over MCP could discover the satellite axis without discovering the bucket it subdivides. The line is additive with the same three-state shape as `Vector:`/`Satellite:`, the MCP parser tolerates an engine that does not emit it, and `gravity` is hoisted to the top level of the description beside `vector` and `satellite`.

### Fixed

- **A `UNIQUE` anchor can no longer be duplicated after an unclean restart.** A process that replayed WAL re-confirms an anchor miss without the bloom before trusting it. Has a declared cost while armed.
- **Table ids are monotonic across restarts**, so one identity can never name two different table contents.
- **`PUT … ON CONFLICT UPDATE` notifies ghosts.** An upsert skipped the ghost hook, so an aggregate ghost kept serving its pre-upsert sums and counts as current. **Ghost state is persisted, so run `REFRESH GHOST` once after upgrading** for aggregate ghosts over lobes that take upserts — the fix stops new drift, it does not repair drift already recorded.
- **`PUT … ON CONFLICT UPDATE` writes through the record cache.** A cached read could return the record as it was before the upsert — a read-your-own-write violation. The cache is in-memory, so the upgrade restart clears any staleness; no action needed.
- **A ghost-routed read returned records without the declared vector.** V5 keeps the searchable vector in its own column; the ghost's point-read did not re-attach it. Since ghosts are materialised by the engine from scan telemetry, the same query returned different fields before and after one was built, with nothing written in between — and an unfused `NEAREST` over that scan returned zero rows, which reads as "no matches". No action needed: nothing on disk is wrong, the vector was always in its column.
- **A longer pipeline no longer shrinks a `NEAREST`'s candidate set.** The fused plan was gated on the pipeline being exactly `SCAN | NEAREST`, so appending a step (`| SHAPE {id}`) dropped the query into the generic loop, where the scan materialises one 1000-record page and `NEAREST` ranks inside it. The fused plan is now chosen whenever the pipeline *starts* with those two steps.

### Changed

- **A truncated `NEAREST` refuses a mutating or aggregating next step.** When the latency airbag cuts the candidate set, a following `SET`, `DELETE` or `AGGREGATE` now errors instead of running: those results cannot carry `budget_stop`, so the write or the total would silently cover only the part that was scored. Read-only steps still compose and the flag travels with them.
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
