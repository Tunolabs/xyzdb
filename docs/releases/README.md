# Release notes

One note per **public** release of xyzDB, newest first — the full narrative behind each tag. The root [`CHANGELOG.md`](../../CHANGELOG.md) carries the per-version delta.

| Version | Date | What it shipped |
|---|---|---|
| [v1.0.0](v1.0.0.md) | 2026-07-30 | **1.0 launch under BUSL-1.1** + specified wire protocol (`PROTOCOL.md`). **Secure by default**: loopback bind, auth on `STATS`/`/metrics` (both break existing deployments). Vector discoverability (`SHOW PROFILE`/`describe_lobe`), MCP `--connect` auth, `NEAREST` budget-degrade, `Value` depth cap. **On-disk format break** — recreate 0.8.x data from source. |

The development that preceded 1.0 (versions 0.1 → 1.0) is not published as per-version notes. Its condensed changelog is archived in [`CHANGELOG-pre-1.0.md`](CHANGELOG-pre-1.0.md) as a record — the full per-version narrative stays in a private repository.
