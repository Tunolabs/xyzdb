# Known issues — xyzDB engine

**Audience**: anyone deciding whether a behaviour they hit is a defect, and anyone
about to depend on one of these paths.
**Surface**: the engine's own behaviour — query results, write semantics, recovery.
Not operational tuning (`OPERATIONS.md`) and not language definition
(`docs/xytalk-spec.md`), though an entry here overrides either when they disagree.

Open defects and limitations we know about, in the engine itself. Written down here
rather than left in a tracker so that a checkout carries them.

**Every entry states what it costs you and whether there is a workaround.** Nothing
here is a surprise waiting in production; if we knew about something and it is not
listed, that is a bug in this file.

Fixed issues are not repeated here — they live in
[`CHANGELOG.md`](CHANGELOG.md) and in the release notes under
[`docs/releases/`](docs/releases/). This file is only what is still true.

As of **1.1.1**.

---

## Correctness

### A bloom filter can report a key absent that is present, after an unclean restart

**What.** An SSTable written during crash recovery can carry a bloom filter that
disagrees with its own data. Every **point** lookup is bloom-gated: the filter is
consulted first, and a "not present" answer skips the read. So a key that exists can
be reported missing, and each caller then does whatever it does when a key is
genuinely absent.

**Root cause: NOT diagnosed.** Three mechanisms remain candidates — a bloom built
with zero bits (which matches everything, so it cannot cause this), a torn bloom on
disk, and a seqno/replay interaction. None has been confirmed, and the reproduction
is flaky. We are not going to name a cause we have not established.

**Where it is now closed.** Two paths, for two different reasons.

The duplicate-anchor check on the write path (`ops/put.rs`) confirms a miss
**without** the bloom before trusting it. That is where a false negative would break
`UNIQUE`.

The declaration loads at boot — gravity, vector and satellite — no longer use a point
lookup at all. They read their reserved prefix with a **range scan**, which never
consults the bloom, because the filter answers point questions. That removes the
exposure rather than teaching the caller to distrust the filter, and it replaces one
lookup per lobe with a single pass. A range scan applies the same MVCC snapshot as a
point read and excludes tombstones, so a retired declaration stays retired.

**This one was demonstrated, not argued.** With the bloom forged to answer "absent"
for every key — the bit array zeroed while `num_bits` stays non-zero — the old
loaders brought the lobe up reporting `Vector: (none)`. The axis was gone, and
nothing said so. `tests/spec_load_bloom_false_negative.rs` asserts both halves: that
the forge really does blind lookups, and that the axes survive it.

**A correction to this file, made in 1.1.1.** Until 1.1.1 the list below named a
duplicate check in `DECLARE ANCHOR`. There is no such check: `ANCHOR … UNIQUE IN`
registers the anchor and persists the registry, and never touches the dictionary
(`engine/verbs.rs`, `execute_anchor`). The bloom-gated read that was meant is in
**`AUTOANCHOR APPLY`**, the operational statement that back-fills the dictionary
for records written before the anchor existed. The defect was real and is now
closed; what was wrong was the label, and the label mattered — it pointed readers
at a declarative statement any application issues, when the exposure is a populate
step an operator runs by hand. Recorded rather than quietly corrected, because the
mis-attribution is the kind of thing this file exists to not do.

**Where it is still NOT closed.** One bloom-gated point lookup still trusts the
filter:

- **The ghost's point-read of a source record** (`ghost/read.rs`). The sharpest of
  the set: its miss branch skips the record, so a false negative there **silently
  drops rows from a result** rather than raising anything. And ghosts are materialised
  by the engine itself, so that exposure can appear without anyone writing a query
  differently.

  Vulnerable **by inspection**; reachability could not be measured yet, and the
  reason is worth stating rather than filing as "unknown": the forge that works for
  the declaration loads does not reach this path, because the reopen replays the
  journal and serves the records from the memtable, which has no bloom to blind. A
  reachability proof needs records living **only** in an SSTable, which means a close
  that rotates the journal. The attempt is in
  `tests/ghost_read_bloom_false_negative.rs`, `#[ignore]`d with that condition and
  with both discriminators already armed.

**What was checked and is NOT exposed.** Listed because a defect file that names only
the bad parts is easy to misread: "not mentioned" looks exactly like "nobody looked".

- **`UNIQUE` anchor declarations.** They do not live in the keyspace at all —
  `AnchorRegistry` is loaded from its own file under `meta/` (`engine/boot.rs`), so no
  bloom, no point lookup, no range scan. A false negative cannot reach them.
- **The `UNIQUE` constraint on every write path.** All three anchor lookups — single
  `PUT`, `PUT BATCH`, `AUTOANCHOR APPLY` — confirm a miss without the bloom before
  trusting it, and each is pinned by a forged-bloom test with its own negative
  control (`tests/anchor_bloom_false_negative.rs`). They share one function
  (`ops::put::anchor_dict_get`) on purpose: 1.1.0 armoured the single-`PUT` door and
  left the other two open, one class through three doors, so the confirmation is now
  somewhere a fourth caller cannot avoid.
- **Ghost metadata.** Loaded by range scan (`ghost/persist.rs`), never a point get.
  Only its writes counter was bloom-gated, and 1.1.1 closed that too.

**Reachability, precisely.** Demonstrated for the three declaration loads and for all
three anchor paths — that is what the forged tests show. **Not measured** for the one
path still open: whether a real post-recovery bloom reaches the ghost source-record
read is unknown, and the live reproduction is flaky. Unknown is not the same as safe,
and neither is written here as though it were the other. What is no longer available
is the comfortable argument that these keys are old and already compacted and
therefore safe. It did not hold where it was checked.

**The balance, in one place.**

| | Status |
|---|---|
| Root cause | **not diagnosed**, no live candidate |
| Duplicate-anchor check, single `PUT` | **closed** — confirms a miss without the bloom |
| Anchor collision check, `PUT BATCH` | **closed in 1.1.1** — same confirmation, same counter, forged test + negative control |
| `AUTOANCHOR APPLY` duplicate check | **closed in 1.1.1** — same; this is the path this file used to call `DECLARE ANCHOR` |
| Gravity / vector / satellite loads | **closed** by range scan — reachability **demonstrated** first |
| Pinned-field lists | **closed** by range scan — same defect, lighter consequence |
| `UNIQUE` anchor declarations | **never exposed** — `anchors.bin` lives outside the keyspace |
| Ghost metadata load | **never exposed** — range scan; its writes counter **closed in 1.1.1** |
| Ghost source-record read | vulnerable by inspection, **not measured** — and the reason is known (above) |

**What it costs you, and the workaround.** The window is an unclean restart. A
graceful shutdown followed by a clean open does not open it. If a process did come up
from a dirty restart, `xyzdb_recovered_from_wal` in `STATS` and `/metrics` reports it
— treat that as a degraded mode, and prefer a clean restart before trusting a lobe's
declarations.

---

### Ghost deltas and the `AUTOANCHOR APPLY` back-fill are written outside the WAL

**What.** Every multi-keyspace mutation goes through one `WriteBatch` and one WAL
entry, so a `PUT`'s record, its identity mapping, its vector column and its anchor
entries are atomic across a crash: all or none. Three writes do **not** travel that
way. The ghost-entry hook and the rollup deltas it maintains (`ghost/notify.rs`) and
the dictionary insert in `AUTOANCHOR APPLY` (`engine/verbs.rs`) go straight into a
memtable through `Tree::insert`, which assigns a sequence number and touches no
journal.

**What it costs you.** A crash between an acknowledged write and the next flush of
`ghosts` / `dictionary` loses the derived state while the record survives. After
recovery a ghost under-counts or misses the row, and an `AUTOANCHOR APPLY` that
reported *N indexed* may have persisted fewer. Nothing repairs it automatically and
nothing reports it: the ghost answers, it just answers from a state that is short.

**What it does NOT cost you.** No acknowledged record is lost, and no `UNIQUE`
constraint is weakened — those live in the WAL-backed batch. This is a durability
asymmetry in derived state, not in your data.

**Workaround.** `REFRESH GHOST "<name>"` rebuilds a ghost from the lobe, which is
the same remedy the 1.1.0 upsert-drift fix prescribes; re-running `AUTOANCHOR APPLY`
is idempotent by the duplicate check above. If a process reports
`xyzdb_recovered_from_wal = 1`, treat declared ghosts over write-heavy lobes as
suspect until refreshed.

**Why it is filed rather than fixed.** Carrying these as batch items in the commit
WAL entry is the fix — they are ordinary keyspace puts and the journal already frames
arbitrary keyspaces — but it moves the rollup read-modify-write inside the fsync
barrier, which is a write-path latency change that a patch release must not make
unmeasured. Tracked for the next minor.

---

### `PULL` can return without the requested root under concurrent load

**What.** `FIND "lobe" WHERE code = "C-1" | PULL 1` returns the linked children but
not the root record that was matched. A client that expects the root first receives a
child instead.

**When.** Observed only with a second, memory-heavy workload running concurrently in
the same process (a ~40 MB ingest of 10,000 records with 4 KB payloads). It has never
been reproduced with that load absent.

**Mechanism: NOT established.** We can reproduce the conditions, not explain the
cause, and we are not going to guess in public. The lobe has anchors declared, so the
lookup goes through the anchor path.

**What it costs you.** A pipeline that assumes the root is present can silently
operate on a child. If you use `FIND | PULL` under concurrent write load, check the
record type of what comes back rather than assuming position.

---

## Contract — a partial that does not announce itself

### Full-lobe `NEAREST` truncates silently; the gravity-bounded path does not

**What.** A `NEAREST` bounded by a gravity predicate reports truncation: it returns
`has_more` with a `budget_stop` object carrying the counts at the cut. A `NEAREST`
over a whole lobe with no gravity scope does not. Its candidate window is the `LIMIT`
(hard ceiling `SCAN_LIMIT_HARD_MAX = 10_000`), and exceeding it produces `status: ok`
with k rows, **no flag and no error**.

**Demonstration** — same lobe, same query, only the `LIMIT` differs:

```
no scope, LIMIT   100 -> top-3 ['g2404', 'g4304', 'g554']
no scope, LIMIT  1000 -> top-3 ['g1403', 'g4151', 'g1252']
no scope, LIMIT  5000 -> top-3 ['g4229', 'g1403', 'g3442']
```

Three different answers to the same question. With a gravity scope and a bucket of
12,000 rows the answer is identical at every `LIMIT` — the cap is lifted there.

**This is not a semantics bug.** `LIMIT` bounds the scan and `NEAREST` ranks what the
scan produced, which is what the pipeline says. But to someone asking "the closest
one overall", the effect is a partial answer that reads as complete.

**What it costs you, and the workaround.** Scope your `NEAREST` with a gravity
predicate and it announces truncation correctly. If you must search a whole lobe,
paginate with `CURSOR` and merge top-k client-side — the top-k of a union of top-k is
exact.

**Either exit closes this:** lift the cap on the full-lobe path, or make that partial
raise its flag like the bounded one does. What cannot stay is one path that warns and
one that does not.

---

## Grammar — an error that does not say what was expected

### A bad argument to a recognised verb gets a generic parse error, not an expected-token one

**What.** When the *verb* is unknown, the error names it: `FROBNICATE "x"` gives
`Unknown command: 'FROBNICATE'`. Many statements also have specific messages, and they
are good ones — a WHERE-less `DELETE` names `PURGE`, a `FIND` with `OR` points at
`SCAN`, `ORDER BY` without `LIMIT` says so. But when the verb parses and its *argument*
does not, and no specific message exists for that spot, you get a generic one:

```text
SHOW BANANAS   →  could not parse from: 'BANANAS' — check the statement's grammar in docs/xytalk-spec.md
SHOW           →  statement ends where more input was expected — check the statement's grammar in docs/xytalk-spec.md
```

It says *where* parsing stopped, never *what was expected there*.

**What it costs you.** For an unfamiliar statement you compare against the grammar in
`docs/xytalk-spec.md` rather than being told the missing token. The wire `code` is
`PARSE_ERROR` either way (`PROTOCOL.md` §8), so a client keying off `code` is
unaffected.

**Already closed, and worth stating so it is not re-reported.** These errors used to
render `nom`'s `Debug` — `Parsing Error: Error { input: "BANANAS", code: Tag }` — which
named a combinator, told the caller nothing, and would have changed shape on a `nom`
upgrade. Fixed in 1.1.0: one wrapper (`parse_failure` in `xytalk-parser`) produces the
messages above, and it replaced **all 35** formatting sites, so the leak cannot return
through one nobody rewrote. Statement-specific messages still win wherever they exist.

**Why the rest is filed rather than fixed.** Expected-token messages mean giving every
argument parser its own — a pass over the parser, not a patch. Nothing silently
succeeds and no result is wrong.

---

## Grammar — a clause that is silently ignored

### Trailing tokens after a pipeline step are discarded

**What.** A pipeline step is parsed until its own grammar is satisfied and **whatever
follows on that segment is dropped without an error**:

```
… | NEAREST(emb, q, 10, cosine)           -> 10 rows
… | NEAREST(emb, q, 10, cosine) LIMIT 10  -> 10 rows   (identical)
… | NEAREST(emb, q, 10, cosine) LIMIT 3   -> 10 rows   <-- asked for 3
… | NEAREST(emb, q, 10, cosine) BANANA    -> 10 rows   <-- invented token
```

`LIMIT 3` returning 10 rows and an invented token being accepted show the discard is
general, not the `LIMIT` being absorbed into `k`.

**What it costs you.** You write a constraint, the engine answers as though it were
not there, and nothing tells you. There is no way to notice except by counting rows.

**Why it is not fixed yet.** The fix (requiring each pipeline step to consume its
whole segment) **changes behaviour**: every query carrying a stray token goes from
working to failing at parse time. That is a breaking change and needs a minor with a
warning period before it, not a patch.

**Workaround.** Put constraints where the grammar takes them — `LIMIT` on the `SCAN`,
`k` inside `NEAREST`, `| TAKE n` as a pipeline step — and treat a clause you are not
sure about as ignored until you have counted the rows it returns.

---

## Observability

### `/ready` reports not-ready on an idle engine under `--durability durable`

**What.** The readiness heuristic asks whether `last_successful_sync_ts_ms` is
within 5 s of now. That timestamp advances only when the sync thread completes a
real **fsync**, so it tracks *writes*, not liveness. An engine serving reads with
no writes pending goes stale in five seconds and answers
`{"ready": false, "reason": "sync_thread heartbeat stale"}` — while the sync
thread is alive and ticking. Measured: `{"ready": true}` immediately after a
write, not-ready five seconds later, on the same idle process.

**What it costs you.** A readiness probe wired straight to `/ready` deroutes a
healthy instance between write bursts. On an idle deployment it never reports
ready at all, and since the probe is what admits traffic, nothing arrives to make
it ready — the instance cannot bootstrap out of the state on its own.

**Reproduced on 1.1.0 and 1.1.1 identically**; it is not a regression, and it has
been documented as an operational caveat in
[`OPERATIONS.md`](OPERATIONS.md) §4 (with the recommended probe pattern) for
longer than it has been listed here. Listing it now because that file's standard
applies to this one: a known behaviour that is not written down here is a bug in
this file.

**Workaround.** Both are in `OPERATIONS.md` §4: pair the probe with
`xyzdb_sync_thread_heartbeat_total` and treat an advancing heartbeat as ready, or
raise the failure threshold enough to absorb an idle window. `--durability
batched` / `async` are unaffected (the timestamp stays 0 and the back-compat
clause covers it).

**Why it is not fixed here.** The engine already distinguishes the two signals —
`last_successful_sync_ts_ms` (fsyncs) and `heartbeat_count` (every tick of the
sync loop, idle included, `turba-engine/src/engine.rs`) — and `readiness_response`
(`crates/server/src/connection.rs`) reads the first while its own doc comment says
it means to test the second. So the plumbing exists and the wire is on the wrong
terminal. **Two candidate fixes, and they are not equivalent:** exposing a
per-tick `last_heartbeat_ts_ms` and reading that is the smaller change, but it
would make `/ready` blind to a sync thread that is alive and whose fsyncs are
*failing* — the condition the probe exists to catch. The heuristic in
`OPERATIONS.md` §4 — `(pending_epoch > synced_epoch) AND (now - last_sync_ts >
5 s)` — keeps that detection and only removes the idle false-positive, at the cost
of exposing the two epochs first. The second is the right one; it is a behaviour
change to a probe, which is why it belongs in a minor and not in this patch.

### The fused `SCAN | NEAREST` path emits no scan telemetry

**What.** Pattern telemetry — which feeds `SHOW SCAN STATS` and the automatic ghost
detector — is recorded on the ordinary scan path only. The fused vector path never
touches it, so a `NEAREST` served by the fast path is invisible to both.

This was already true for a two-step pipeline. 1.1.0 widened it: the fused plan is now
chosen whenever a pipeline *starts* with `SCAN | NEAREST`, so longer pipelines are
invisible too.

**What it costs you.** Two things, neither of them a wrong answer:

1. A hot three-step pattern can never be promoted to a ghost. That is usually right —
   the fused path is already the fast one — **but if the hot step is the `AGGREGATE`**,
   a precomputed ghost would help and will never be detected. Nothing reports the
   missed optimisation; it simply does not happen.
2. `SHOW SCAN STATS` shows fewer patterns than it used to, with no explanation in the
   output.

**Open design question.** Whether the fused path should emit telemetry anyway is not
obvious: telemetry exists to decide whether to build a ghost, and for the dominant
case the answer is already no — so emitting it would invite ghosts that do not help.
A middle path is to emit only when the pipeline has a tail.

---

## Test infrastructure

### `durability-test-hooks` substitutes the production WAL pruner

**What.** Building with the `durability-test-hooks` feature replaces the production
WAL pruner with an older janitor. The durability suite therefore exercises a
reclamation path that is not the one that ships.

**What it costs you.** Nothing at runtime — the feature is off in every release
build. It costs confidence in that suite: a green run there does not fully evidence
the shipped pruner.

---

## Declared limitations

Design limits — things xyzDB does not do, as opposed to things it does wrong — are
listed in the README under
[**What xyzDB is not**](README.md#what-xyzdb-is-not), and are not repeated here.
That includes the unbounded `FIND`/`PULL` result sets and the absence of an in-place
migration across an on-disk format change. Both are bullets in that section — checked,
because this file has already sent readers to a section that did not contain what was
promised (see the plaintext-token note below), and a pointer to the wrong place is worse
than no pointer: the reader concludes the limitation is undocumented.

One limitation lives elsewhere and is pointed at rather than moved: the auth token is
stored **in plaintext on disk**, stated in
[`docs/usage/reference.md`](docs/usage/reference.md) §Security beside the flag that
reads it. The README's security bullet covers the authorization model — that there is
none — which is a different limitation; an earlier version of this file sent readers
there for the plaintext one and they would not have found it. TLS 1.3 protects the
token **in transit** (`--tls-cert` / `--tls-key`); at rest it is a file on your disk.
