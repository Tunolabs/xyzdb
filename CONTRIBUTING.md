# Contributing to xyzDB

## Status — code contributions are not open yet

**xyzDB is not accepting pull requests at this time.** xyzDB is
source-available under the Business Source License 1.1 and is also licensed
commercially, so accepting code requires a contributor agreement (CLA) to be in
place first. That agreement is being drafted; we are **targeting the last week
of August 2026** (about 30 days after the 2026-07-30 launch) to open pull
requests. This is a target, not a guarantee — if the date moves, it will be
announced here. On that day the contributor agreement is published in this
repository, the automated check on pull requests is switched on, and code PRs
begin going against `dev`.

**Welcome right now:** issues, bug reports, and benchmark reproductions —
please open an issue. Do **not** open a pull request with code changes yet: it
cannot be merged until the contributor agreement is published, and we do not
want you to spend effort that cannot land.

The rest of this document describes the checks that will apply to code
contributions once they open. It is published in advance so the requirements
are no surprise.

## Scope of this document

xyzDB is authored and maintained by Iván Moreno Mendoza. This file documents the
non-negotiable checks a contributor must run through before proposing a change,
with pointers to the authoritative references. It does not cover onboarding,
style, or roadmap — those live elsewhere.

## Durability compliance checklist

Any change that touches the write path, the WAL, memtables, SSTables,
compaction, or any function documented with a `/// # Durability` rustdoc section
must answer these four questions in the PR description or the code review:

1. **Does this path introduce a new `Ok` returned to a caller after a write?**
   If yes: state explicitly in the PR description which transition establishes
   `Acked` or `InSST` before the `Ok`.

2. **Does this path advance a durability sentinel** (`synced_epoch`,
   `pending_epoch`, `flushed_seqno`, or any successor)? If yes: is the advance
   gated on the underlying I/O returning `Ok`? Show the gate in the PR
   description.

3. **Does this path call `JournalWriter::rotate()` or equivalent WAL
   truncation?** If yes: what establishes the D1 precondition (every
   acknowledged write is in an SSTable) before the call? Add a regression test
   covering this caller alongside the existing durability tests in
   `crates/turba-engine/tests/`.

4. **Does this path match any failure mode documented in
   `docs/wal-state-machine.md` §6?** If yes: justify why the skip is safe in
   this context, or replace with the documented guard.

If any of these four questions cannot be answered definitively, the change is
not ready to merge.

The authoritative reference is
[`docs/wal-state-machine.md`](docs/wal-state-machine.md), which catalogues the
states, transitions, sentinels, and historically-observed bug shapes. Read §7
of that document before proposing a durability-touching change.

## Branch targets

Pull requests go against `dev`, never against `main`. `main` is always the latest
release and moves only when a release is cut — a fast-forward carrying that
release's tag. See [`docs/branch-tag-discipline.md`](docs/branch-tag-discipline.md)
for the full branch and tag model.

## Commit and PR conventions

- **Conventional Commits** for every commit: `feat`, `fix`, `docs`, `refactor`,
  `test`, `chore`, `perf`, `build`, `ci`, `style`. Subject ≤ 72 chars,
  imperative, no trailing period. Body wraps at 72 cols and explains WHY.
- **Commits atomic per logical step**, not per session. If a commit fixes a bug
  and documents the fix, one commit; if it fixes two independent bugs, two
  commits.

## Tests

- `cargo test --workspace` must pass before merge.
- `cargo test -p turba-engine --features durability-test-hooks` must pass; this
  gate exercises the durability regression tests that lock previously-observed
  write-path and crash-recovery bugs.
- `cargo build --workspace` must succeed.
- Integration-level crash recovery: `crates/turba-engine/tests/crash_recovery.rs` uses
  fork + SIGKILL and is the strictest durability check. Run it locally for any
  change that touches the write path.
- A few performance assertions do **not** run by default because they measure
  wall-clock time, which is meaningless on a shared/contended CI runner. Enable
  them with `XYZDB_PERF_GATES=1 cargo test ...` on a quiet machine; they are
  expected to pass only where there is no contention. Without the variable the
  measurement still runs and prints its number — only the assertion is skipped.

## Formatting & linting

CI runs these same checks on every push and pull request (see
[`.github/workflows/ci.yml`](.github/workflows/ci.yml)); run them locally first,
using the same invocations:

- `cargo fmt --all --check` — formatting is reproducible across toolchains:
  `rustfmt.toml` pins `style_edition = "2024"` under the toolchain pinned in
  `rust-toolchain.toml` (Rust 1.96). Reformat with `cargo fmt --all`.
- `cargo clippy --workspace` — must exit 0 (**zero errors**). This is not the
  same as zero warnings. Policy (from `[workspace.lints.clippy]`, re-declared
  per crate where needed): `undocumented_unsafe_blocks` and `missing_safety_doc`
  are **deny** — these are hard errors and must stay at zero; `unwrap_used` and
  `expect_used` are **warn** — production code carries a known, tracked set of
  these, and your change must not increase the count; the cosmetic
  `doc_lazy_continuation` and `doc_overindented_list_items` are **allow**.
- `cargo test --workspace` — see [Tests](#tests) above.
- `bash .ci/license-version-parity.sh` — must exit 0. Validates that `LICENSE`,
  the root `Cargo.toml`, the change-dates table, and `NOTICE` all agree on the
  version and Change Date. A change that touches `LICENSE` or the workspace
  version fails here until all four match.
- `bash .ci/profile-parity.sh` — must exit 0. Guards the storage-profile
  definitions against drift. A change to a storage profile fails here until the
  profiles line up.

CI also runs `cargo test -p turba-engine --features durability-test-hooks` (see
[Tests](#tests)) and `rustup show` to install the pinned toolchain before any of
the above.

## References

- [`docs/wal-state-machine.md`](docs/wal-state-machine.md) — WAL state machine,
  invariant D1, failure catalogue, compliance checklist.
- [`CHANGELOG.md`](CHANGELOG.md) — keep-a-changelog format; add your change under
  the `## [Unreleased]` section. The maintainer assigns it to a release when the
  release is cut.
