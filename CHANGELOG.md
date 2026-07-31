# Changelog

All notable changes to xyzDB are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioning follows [Semantic Versioning](https://semver.org/).

> **The public history of xyzDB begins at 1.0.** Per-version release notes live in [`docs/releases/`](docs/releases/). The condensed development history from 0.1 to 1.0 is archived in [`docs/releases/CHANGELOG-pre-1.0.md`](docs/releases/CHANGELOG-pre-1.0.md) — its commit, branch, and file references belong to the private repo and do not resolve here. For the current design, read [`docs/architecture.md`](docs/architecture.md) and [`OPERATIONS.md`](OPERATIONS.md); this changelog tracks 1.0 forward.

## [Unreleased]

### Added
- **Sub-gravity axis (`SATELLITE BY <field> IN "lobe"`).** A third foundational
  axis, sibling to gravity and vector: names the single field whose value
  sub-buckets a gravity bucket via the `sat` axis of the spatial key. The write
  path places each record in satellite `hash16(field)`; a `SCAN … WHERE gravity
  AND satellite_field` (and the same shape feeding `AGGREGATE count()`) scans
  only that satellite sub-range instead of the whole bucket. It is a **pure
  optimisation** — same rows, same order as the parent scan — because the read
  path re-applies the field predicate as an anti-collision residual (the 16-bit
  hash collides by design). One axis per lobe; declared on an empty lobe; a
  `SET` that changes the field re-places the record (`ON CONFLICT UPDATE` stays
  in place, like gravity). Records missing the field share satellite 0, so the
  axis pays only when the field is near-universal in the lobe. See
  `docs/xytalk-spec.md` §2.2.2.
- `NEAREST` responses truncated by the latency airbag (`--nearest-budget-ms`)
  now carry a `budget_stop` object (`examined` / `candidates` / `found`) — the
  counts at the cut, turning the `has_more` inference into a fact. Present ONLY
  on that truncation frame; every other `PaginatedRecords` (cursor pages, SCAN
  caps) stays byte-identical, so `has_more`-based clients are unaffected.

### Fixed
- `PUT ... ON CONFLICT UPDATE` (upsert) now notifies ghosts and updates the
  record cache, so covering/aggregate ghosts and cached reads reflect an upsert
  without a `REFRESH` (previously stale until refreshed).

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
