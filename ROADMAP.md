# Roadmap

This is **intent, not a commitment or a schedule.** Priorities move with what
users report, so nothing here is a dated promise. The record of what has already
shipped is in [`CHANGELOG.md`](CHANGELOG.md) and
[`docs/releases/`](docs/releases/); what each version level (patch / minor /
major) is allowed to change is in
[`docs/branch-tag-discipline.md`](docs/branch-tag-discipline.md).

**Compatibility promise.** Anything that changes the on-disk layout or the
engine's behavior is declared **per lobe** and is **off by default** — no
declaration, no change. Upgrading does not alter what you already have running.

## Next — the 1.1.x line

- **Code contributions open** — we are **targeting the last week of August
  2026** (about 30 days after the 2026-07-30 launch); a target, not a guarantee,
  and if the date moves it will be announced in
  [`CONTRIBUTING.md`](CONTRIBUTING.md). On that day the contributor agreement is
  published, the automated check on pull requests is switched on, and code PRs
  begin going against `dev`.
- **Logical export (`dump` / `load`)** — a data-out path independent of the
  on-disk format. Today snapshots are physical, and a format change between minor
  versions requires re-ingestion from source (see
  [`docs/releases/v1.0.0.md`](docs/releases/v1.0.0.md) → Compatibility); a logical
  export/import removes that. This is the most important item here for anyone with
  data on disk.
- **A cardinality cap and time budget for `FIND` and `PULL`** — today both return
  every matching / linked record with no default row cap and no wall-clock budget
  (declared as a limitation under *What xyzDB is not* in the
  [README](README.md)). A default cap plus a per-query budget close it.
- **Per-file SPDX headers** across the tree.
- **`NEAREST` stops degrading on a residual filter over an undeclared field.**
  Today it scores the whole bucket and hydrates until it reaches `k`; it will
  switch to filtering first and scoring only the survivors when that is cheaper.
- **The `NEAREST` response reports the fact, not an inference.** It will carry
  `examined` / `candidates` / `found` counts instead of a bare boolean, and the
  budget signal is renamed to `budget_stopped`. This is a change to the response
  shape — anyone writing a client needs it.
- **Aggregate ghosts truly maintained.** `count` and `sum` move from scanning the
  bucket to an O(1) read.
- **A query served by a possibly-stale ghost says so in the response.**
- **`NEAREST` served from a ghost.**
- **Documentation.** A hierarchical-retrieval recipe (collection summaries first,
  then the top-k items) for a very large corpus in a single bucket; and the
  datasheet will separate two numbers that are conflated today — storage capacity
  vs. a comfortable bucket size — with the note that a large payload costs disk,
  not retrieval latency, because the vector travels in its own lane.

## Later — no dates

Cross-AZ replication / leader-follower / multi-node clustering, encryption at rest
(AES-256-GCM), cross-lobe transactions, a PostgreSQL wire-protocol shim, tiered
storage beyond memory — a *future* design, distinct from the RAM/SSD/HDD
heat-allocator multi-tier mode that was measured and retired before 1.0
([`docs/releases/CHANGELOG-pre-1.0.md`](docs/releases/CHANGELOG-pre-1.0.md)), not a
redo of what failed — and a billions-of-records scale tier. Multi-tenancy
isolation stays cgroup-level (one xyzDB process per tenant), not intra-process
namespaces.

- **Satellite re-packing.** The satellite axis itself **shipped in 1.1.0**
  (`SATELLITE BY`); what is not built is moving records that already exist. A lobe
  must be empty when the axis is declared, because existing rows would stay in the
  default sub-bucket where a bounded query never reaches them. Re-packing them is a
  data-movement feature, not a key-format change — the slot has been in the key
  since 1.0.
