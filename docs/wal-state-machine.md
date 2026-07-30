# WAL state machine

Reference for every path that writes, persists, rotates, or truncates
the xyzDB Write-Ahead Log. This document answers two questions a
contributor should be able to resolve without reading the code:

1. What states can a write occupy between `WriteBatch::commit()` and
   surviving a crash + reopen?
2. What operation transitions between states, under what
   preconditions and with what postconditions?

This document is the static reference for the durability contract — the
invariant, its callers, and the guards that enforce it — not the forensic
history of how the cluster was found and closed.

## 1. States

A write committed to xyzDB moves through the following states. Each
state is defined by its **durability property** (does a crash in
this state lose the write?) and its **visibility property** (can a
reader see it?).

| State | Durable on crash? | Visible to readers? | Transition out |
|---|---|---|---|
| `Pending` | No | No | sync thread fsync |
| `Acked` | Yes, via WAL replay | Yes, via memtable | `Tree::seal_active` |
| `InSealedMemtable` | Yes, via WAL replay | Yes, via memtable | `Tree::flush_sealed` |
| `InSST` | Yes, independent of WAL | Yes, via SSTable | `Tree::maybe_compact` (D1-neutral) |
| `PostCompact` | Yes, independent of WAL | Yes, via deeper SSTable | `Tree::maybe_compact` (loops) |
| `Pruned` | N/A — write must already be `InSST` | Yes, via SSTable | terminal |
| `Truncated` | N/A — write must already be `InSST` | Yes, via SSTable | terminal |

`Pruned` and `Truncated` are not states the write itself moves
through; they are states of the **WAL storage** that previously
recorded the write. The write itself remains in `InSST` or
`PostCompact`. Both mean "this write is no longer recoverable from the
WAL" — safe only when every Acked write the WAL recorded is already
`InSST` — but they are reached by two different mechanisms:

- **`Pruned`** is the **steady-state** path. The WAL is segment-based
  (an active `journal.wal` plus archived `journal.<max_seqno>.wal`
  segments); the production `turba-wal-pruner` deletes an archived
  segment once every entry it holds is manifest-durable. Only the
  deleted segment's writes become `Pruned`; the active segment and any
  not-yet-durable tail are never touched.
- **`Truncated`** is the **exceptional** path. `JournalWriter::rotate`
  zeroes the *entire* WAL (active segment plus every archived segment)
  and is invoked only from the quiescent `major_compact` /
  `execute_compact` compaction paths.

### State diagram

```
                   ┌─── WAL replay recovers ────┐
                   │                            │
                   v                            │
  commit() ──► Pending ──► Acked ──► InSealedMemtable ──► InSST ──► PostCompact
               │           │         │                    ^
               │           │         │                    │
               │           │         │                    └─ SSTable file +
               │           │         │                       manifest entry
               │           │         │                       independent of WAL
               │           │         │
               │           │         └─ seal_active: memtable moved
               │           │            from active to sealed (still
               │           │            in RAM, still WAL-backed)
               │           │
               │           └─ writer returned Ok; sync thread
               │              fsynced the WAL bytes
               │
               └─ bytes in BufWriter, not fsynced, not visible

  ╰─────────────────── D1 invariant domain ────────────────────╯
          from the first Acked write
          to when every Acked write is InSST

  D1 is testable inside this domain. Outside:
    - Before Acked: no durability claim has been made.
    - After InSST: the WAL segment recording it is free to be
      pruned (steady state) or the whole WAL rotated (compaction).
```

The **D1 invariant domain** is the interval where the invariant is
both non-trivial and testable. Before `Acked`, no durability promise
has been given to the client (the write hasn't returned `Ok`).
After every Acked write is `InSST`, the WAL is free to be truncated.
Inside the domain, the invariant requires that any operation which
discards the WAL (rotate, janitor, etc.) first establish that every
Acked write has transitioned to `InSST`. This is what Finding 8/10
violated; it is what the rustdoc on `JournalWriter::rotate`
documents as precondition.

## 2. Transitions

Each transition lists: trigger (the function that drives it),
precondition (what must be true on entry), postcondition (what the
transition guarantees on exit), and code location.

### `Pending → Acked`

- **Trigger**: group-commit sync thread (`turba-wal-sync`), `crates/turba-engine/src/engine.rs`.
- **Precondition**: `pending_epoch > synced_epoch` and the journal
  mutex is acquirable.
- **Postcondition**: `j.sync()` returned `Ok`; `synced_epoch` is
  advanced to `pending_epoch`; waiting writers are released via
  `Condvar::notify_all`. If `j.sync()` returned `Err`, `synced_epoch`
  is NOT advanced and writers remain blocked until the next retry
  (Finding 9 secondary fix).
- Writer side: `WriteBatch::commit` (`engine.rs`) enrolls into
  `pending_epoch`, then blocks on the condvar until
  `synced_epoch >= epoch`. The loop condition — not a timeout —
  is what makes `Ok` imply durability (Finding 9 primary fix).

### `Acked → InSealedMemtable`

- **Trigger**: `Tree::seal_active` (`tree/mod.rs`).
- **Precondition**: none on durability. Caller may invoke
  unconditionally; the function is a no-op if the active memtable is
  empty.
- **Postcondition**: the previously-active memtable is now in
  `sv.sealed`; a fresh empty memtable is installed as active.
  Durability of already-Acked writes is unchanged — they remain
  WAL-backed until the sealed memtable is flushed.

### `InSealedMemtable → InSST`

- **Trigger**: `Tree::flush_sealed` (`tree/mod.rs`).
- **Precondition**: `sv.sealed` is non-empty (otherwise no-op).
- **Postcondition**: for each sealed memtable, an SSTable file is
  written via `flush::flush_memtable` and opened as a new L0 handle;
  `sv.sealed` is trimmed; in non-BULKMODE, `persist_manifest` is
  called (`tree/mod.rs`); `flushed_seqno.fetch_max(max_flushed)`
  advances at `tree/mod.rs`. A successful `persist_manifest` is
  also what advances `manifest_durable_seqno` — the sentinel the WAL
  pruner keys on (§5). See §6 for the BULKMODE nuance.

### `InSST → PostCompact`

- **Trigger**: `Tree::maybe_compact` (background worker) or
  `Tree::major_compact` (manual, invoked by `Engine::major_compact`
  and `execute_compact`).
- **Precondition**: none on durability.
- **Postcondition**: input SSTables are merged into new SSTables at
  the target level; Version is updated; manifest is persisted; old
  SSTables are deleted. D1-neutral: every Acked write is still in
  some SSTable, just at a different level.

### Active → Archived segment (WAL roll)

- **Trigger**: `JournalWriter::maybe_roll`
  (`crates/turba-engine/src/journal/writer.rs`), called after a successful
  `sync()` / `write_batch` fsync once the active segment reaches
  `segment_max_bytes` (default 64 MiB).
- **Precondition**: the buffer is flushed + synced (the active
  `journal.wal` on disk is a complete segment).
- **Postcondition**: `journal.wal` is renamed to
  `journal.<max_seqno>.wal` and recorded in `segments`; a fresh empty
  active `journal.wal` starts. **D1-neutral**: no write changes
  durability state; the same bytes are simply in a differently-named
  file. This is the mechanism that lets `prune` reclaim WAL space at
  segment granularity without ever touching the active tail.

### `Acked → Pruned` (steady-state WAL reclamation)

- **Trigger**: `JournalWriter::prune(watermark)`
  (`crates/turba-engine/src/journal/writer.rs`), driven by the production
  `turba-wal-pruner` thread (`crates/turba-engine/src/engine.rs`) on a
  ~1 s cadence.
- **Precondition (D1)**: the pruner passes
  `watermark = min(manifest_durable_seqno)` across all trees
  (`wal_prune_watermark`, `crates/turba-engine/src/engine.rs`) — **never
  `flushed_seqno`** (the BULKMODE trap, §6). A caught-up keyspace
  contributes `u64::MAX` so idle keyspaces (e.g. `ghosts`) never pin
  the watermark.
- **Postcondition**: every archived segment whose `max_seqno ≤
  watermark` is deleted (its writes become `Pruned`); the active
  segment and any archived segment holding a seqno `> watermark` are
  kept. Lossless and delete-only — a segment holding a non-durable
  entry is never eligible, because the keyspace that received it pins
  the watermark below it. Bounded-growth backstop: if a lagging
  keyspace pins the watermark and the total WAL crosses `wal_max`, the
  pruner runs `checkpoint_flush_and_prune` (flush every tree, persist
  manifests, then prune) rather than `rotate`, so a concurrent
  writer's not-yet-durable tail is never truncated.

### `Acked → Truncated` (the dangerous transition)

- **Trigger**: `JournalWriter::rotate`
  (`crates/turba-engine/src/journal/writer.rs`). **Exceptional path** —
  used only by quiescent compaction, never by steady-state
  reclamation (that is `prune`, above).
- **Precondition (D1)**: every Acked write is already `InSST`. In
  practice this means: on every `Tree`, `seal_active()` has been
  called AND the resulting sealed memtables have been drained by
  `flush_sealed()` BEFORE `rotate()` is invoked.
- **Runtime enforcement**: the precondition is now checked at run
  time, not merely documented in rustdoc / covered by tests.
  `TurbaEngine::rotate_journal` (`crates/turba-engine/src/engine.rs`)
  inspects every WAL-backed keyspace (spatial, identity, dictionary,
  vectors) and refuses with `Error::WalRotatePrecondition` if any
  still holds unflushed acked writes — a caller that skips a
  `seal_active()` / `flush_sealed()` (as the compact-skips-vectors bug
  did) is rejected instead of silently truncating.
- **Postcondition**: the WAL file is truncated to 0 bytes and every
  archived segment is dropped (they are stale under the precondition).
  Future crash recovery cannot replay from it. Undoing this transition
  is impossible.
- **Callers**:
  - `Engine::major_compact` / `checkpoint_and_rotate` (path A, fixed
    in v0.2.3) — calls `seal_active()` + `major_compact()` which
    invokes `flush_sealed()` on every tree before `rotate()`.
  - `execute_compact` (path B, fixed in v0.2.3) — same structure
    for spatial/identity/dictionary/vectors.
  - **`turba-wal-pruner` does NOT call `rotate`** — it is the
    rotate-free reclaimer, using `prune(watermark)` (and, at the
    `wal_max` backstop, `checkpoint_flush_and_prune`). Listed here so
    the rotate-caller audit stays complete: the pruner is deliberately
    absent from it.
  - WAL janitor (disabled in v0.2.3) — the *former* automatic
    rotate path; ran `rotate()` on a `min(flushed_seqno)` watermark
    without seal/flush. Now gated behind the `durability-test-hooks`
    feature for regression testing only, superseded in production by
    the `prune`-based pruner above.

## 3. Composite operations

Operations exposed to the caller; each decomposes into transitions
from §2.

| Operation | Decomposition | D1 guarantee |
|---|---|---|
| `WriteBatch::commit` (Durable) | Pending → Acked | Ack implies sync (Finding 9 primary fix). |
| `WriteBatch::commit` (BULKMODE) | memtable insert, WAL write **skipped** | Ack is best-effort by design. Crash = re-run; trade-off documented in the `/// # Durability` rustdoc. |
| `Engine::major_compact` (path A) | `seal_active` + `major_compact` (loops flush_sealed + compact) + `rotate` | Establishes D1 before rotate. |
| `execute_compact` (path B) | `seal_active` + `major_compact` per keyspace (spatial/identity/dictionary/vectors) + `rotate` | Same as path A. |
| `turba-wal-pruner` (steady state) | segment `prune(min(manifest_durable_seqno))` | Lossless: deletes only manifest-durable archived segments; active tail untouched, no `rotate`. The default, automatic WAL bound. |
| `turba-wal-pruner` (`wal_max` backstop) | `checkpoint_flush_and_prune` = flush every tree + persist manifests + `prune` | Establishes durability, then prunes. Never rotates a live WAL, so a concurrent writer's tail survives. |
| `execute_autoanchor_apply` | direct dictionary insert + `seal_active` + `flush_sealed` (fixed in v0.2.3.1) | Ack implies InSST. |
| `persist_pinned` (PIN / UNPIN) | direct dictionary insert + `seal_active` + `flush_sealed` (fixed in v0.2.3.1) | Ack implies InSST. |
| `Engine::shutdown` | stop sync thread + `seal_active` per tree + drain bg workers + `journal.sync` | Every acked write ends in InSST or in the WAL file for the next replay. |
| `Drop for TurbaEngine` | delegates to `shutdown` path | Same as shutdown. Bypassed by tests via `mem::forget`. |
| WAL janitor (gated, superseded) | `rotate` on `min(flushed_seqno)` watermark, **no** seal/flush | D1 violation by construction. Not in production (replaced by `turba-wal-pruner`); survives only behind `durability-test-hooks` for the Finding 10 regression test (`#[should_panic]`). |

## 4. Invariant D1

> **Invariant D1.** Every caller of `JournalWriter::rotate()` — and
> every operation that advances a durability sentinel treated as a
> promise to a client — must establish, before the call, the
> precondition **"all writes acknowledged to callers are in SSTables,
> not in active memtables, sealed-but-unflushed memtables, or WAL-only
> state"**.

Every known caller of `rotate()` has a dedicated regression test that
exercises an adversarial ordering (an active-memtable write present at
rotate time, then a crash), enforcing this at the test level.
Adding a fourth caller without a corresponding row in that table is
a violation of the cluster's response plan.

## 5. Sentinels

Four durability-related atomic counters:

- `synced_epoch` (`GroupSync`): advanced by the sync thread after
  `j.sync() == Ok`. Consumed by writers via the condvar loop. The
  writer's Ack happens only after `synced_epoch >= writer_epoch`, so
  `synced_epoch` is a strict durability promise.
- `pending_epoch` (`GroupSync`): incremented by each writer at
  enrollment, **before** the sync. Counts batches queued for sync;
  NOT a durability promise. The writer blocks on `synced_epoch`, not
  `pending_epoch`. Advance-before-sync here is intentional.
- `flushed_seqno` (`Tree`): advanced in `flush_sealed`
  (`tree/mod.rs`) after the SSTable file is written but **before**
  `persist_manifest` — in BULKMODE the manifest is skipped entirely, so
  `flushed_seqno` can lead the on-disk manifest (nuance in §6). It is
  therefore **NOT** a WAL-prune input: `turba-wal-pruner` explicitly
  refuses `flushed_seqno` (`crates/turba-engine/src/engine.rs`,
  `wal_prune_watermark`). Consumed only by diagnostic readers
  (`/stats`, reap-cycle log) and by the feature-gated legacy janitor.
- `manifest_durable_seqno` (`Tree`): advanced in `flush_sealed`
  (`tree/mod.rs`) **only after `persist_manifest` succeeds** —
  the manifest that references the just-written SSTables is itself on
  disk. This is the sentinel the production WAL pruner keys on:
  `min(manifest_durable_seqno)` across all trees is the prune
  watermark, so an archived segment is dropped only when every entry
  it holds is provably recoverable from a durable manifest, not merely
  flushed. Reading it via `manifest_durable_seqno()` (`tree/mod.rs`).

An internal audit confirms every advance is either
gated on I/O `Ok` or is intentionally pre-sync because the sentinel
is not a durability promise.

## 6. Failure modes and anti-patterns

Each shape below has been observed historically in this codebase; the
corresponding fix commit is listed. A commit introducing any of these
shapes again should be rejected in code review.

### rotate() without seal (Finding 8 shape)

- **Shape**: `rotate()` called while an active memtable holds
  acknowledged writes; the writes disappear with the WAL truncation.
- **Fixed in**: v0.2.3 (path A and path B).
- **Guard**: rustdoc on `JournalWriter::rotate` states the D1
  precondition; regression tests per caller.

### Ack before sync (Finding 9 primary shape)

- **Shape**: writer returns `Ok` to the client while its batch is
  still `Pending` (WAL not fsynced).
- **Fixed in**: v0.2.3 (primary — writer loops on condvar until
  `synced_epoch >= epoch`).
- **Guard**: `WriteBatch::commit` rustdoc documents the sync barrier;
  regression test `finding_9_writer_blocks_until_synced_epoch`.

### Sentinel advance on failed I/O (Finding 9 secondary shape)

- **Shape**: `synced_epoch` bumped in a path that does not verify
  `j.sync() == Ok`, waking writers on unsynced data.
- **Fixed in**: v0.2.3 (secondary — match on `j.sync()` result).
- **Guard**: sync-thread code explicitly matches on `Ok(())`; audit
  in §5.

### Janitor rotate on sentinel watermark (Finding 10 shape)

- **Shape**: background thread rotates the WAL on a watermark
  (`min(flushed_seqno)`) without calling `seal_active()`; active
  memtable writes whose seqno exceeds the watermark at rotate instant
  are lost.
- **Fixed in**: v0.2.3 (janitor disabled in production builds).
- **Guard**: `#[cfg(feature = "durability-test-hooks")]` on the
  janitor spawn; `finding_10_wal_janitor_rotate_does_not_lose_active_memtable_writes`
  regression test (`#[should_panic]`, documents the bug class).

### Ack before InSST on direct Tree::insert (audit item 5 shape)

- **Shape**: caller of a PIN/UNPIN/AUTOANCHOR-style operational
  command invokes `Tree::insert` directly (bypassing
  `WriteBatch::commit` and hence the group-commit barrier), then
  returns `Ok` to the client. The write is in the active memtable
  but neither `Acked`-via-WAL nor `InSST`. Crash before the next
  natural flush loses the write despite the ack.
- **Fixed in**: v0.2.3.1 (`execute_autoanchor_apply` and `persist_pinned`, both seal/flush pre-ack).
- **Guard**: an internal writer-ack audit enumerates
  every direct `Tree::insert` ack site; future commits adding a new
  one must extend it and include the seal/flush pre-ack
  pattern.

### Sentinel leads manifest in BULKMODE (item 6 nuance — reconciled)

- **Shape**: `flushed_seqno` advances in `flush_sealed` while
  `persist_manifest` is skipped (BULKMODE path). The sentinel leads
  the on-disk manifest; on crash + reopen, the manifest wins.
- **Status**: **reconciled, not a bug.** The always-on WAL bound
  (`turba-wal-pruner`) never keys on `flushed_seqno`. The
  manifest-aware accessor called for in the original "future guard"
  now exists: `manifest_durable_seqno` (`tree/mod.rs`) advances
  **only** after `persist_manifest` succeeds, and `wal_prune_watermark`
  (`crates/turba-engine/src/engine.rs`) takes `min(manifest_durable_seqno)`
  — so in BULKMODE, where the manifest is deferred, the watermark does
  not advance and the pruner keeps the corresponding WAL segments. The
  segment holding a not-yet-manifest-durable entry is therefore never
  eligible for prune, which is precisely how the trap is avoided by
  construction. D1 is not violated — `flushed_seqno` is not returned to
  clients as an ack and is not a prune input.
- **Residual**: any *future* cross-recovery durability promise
  (replication, a follower reading a live seqno) must likewise key on
  `manifest_durable_seqno`, never `flushed_seqno`.

## 7. Compliance quick-reference

For the implementer of a new feature that touches durability state,
the questions to answer in code review:

1. **Does my path introduce a new ack to the client?** If yes, which
   transition establishes `Acked` or `InSST` before my `Ok`? Add a
   row to the writer ack audit table.
2. **Does my path advance a durability sentinel?** If yes, is the
   advance gated on the underlying I/O returning `Ok`? Add a row to
   the sync/replay advancement audit table.
3. **Does my path call `rotate()` or equivalent WAL truncation?** If
   yes, what establishes the D1 precondition before the call? Add a
   regression test per the D1 caller coverage map.
4. **Does my path skip any step documented in §6 as a failure mode?**
   If yes, justify why the skip is safe in your context, or replace
   with the documented guard.

If any of these four questions cannot be answered definitively, the
path is not ready to merge.
