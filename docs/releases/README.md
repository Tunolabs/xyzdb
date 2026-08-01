# Release notes

One note per **public** release of xyzDB, newest first — the full narrative behind each tag. The root [`CHANGELOG.md`](../../CHANGELOG.md) carries the per-version delta.

| Version | Date | What it shipped |
|---|---|---|
| [v1.1.0](v1.1.0.md) | not yet tagged | **Sub-gravity (`SATELLITE BY`)** — a third foundational axis that bounds `SCAN`/`AGGREGATE`/`NEAREST` to one sub-bucket. **`budget_stop`** says what a truncated `NEAREST` partial actually is, including which traversal produced it. **A `UNIQUE` anchor can no longer be duplicated after an unclean restart** (the one item with a direct product consequence). **Invariant guards became `STATS` + `/metrics` state.** Additive and backward compatible — a 1.0.x data directory opens unchanged. |
| [v1.0.0](v1.0.0.md) | 2026-07-30 | **1.0 launch under BUSL-1.1** + specified wire protocol (`PROTOCOL.md`). **Secure by default**: loopback bind, auth on `STATS`/`/metrics` (both break existing deployments). Vector discoverability (`SHOW PROFILE`/`describe_lobe`), MCP `--connect` auth, `NEAREST` budget-degrade, `Value` depth cap. **On-disk format break** — recreate 0.8.x data from source. |

The development that preceded 1.0 (versions 0.1 → 1.0) is not published as per-version notes. Its condensed changelog is archived in [`CHANGELOG-pre-1.0.md`](CHANGELOG-pre-1.0.md) as a record — the full per-version narrative stays in a private repository.
