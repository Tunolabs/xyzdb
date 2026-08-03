# Known issues — xyzDB engine

Open defects and limitations we know about, in the engine itself. Written down here
rather than left in a tracker so that a checkout carries them.

**Every entry states what it costs you and whether there is a workaround.** Nothing
here is a surprise waiting in production; if we knew about something and it is not
listed, that is a bug in this file.

Fixed issues are not repeated here — they live in
[`CHANGELOG.md`](CHANGELOG.md) and in the release notes under
[`docs/releases/`](docs/releases/). This file is only what is still true.

As of **1.1.0**.

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

**Where it is still NOT closed.** These bloom-gated point lookups still trust the
filter, and none of them has been measured:

- **The pinned-field lists** and their legacy key, loaded at boot. Same shape the
  specs had. Losing them loses which columns a read projects — not a constraint.
- The duplicate check in `DECLARE ANCHOR`, and a ghost metadata load.
- **The ghost's point-read of a source record** (`ghost/read.rs`). The sharpest of
  the three: its miss branch skips the record, so a false negative there **silently
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
- **The `UNIQUE` constraint on the write path.** The duplicate check confirms a miss
  without the bloom before trusting it (`ops/put.rs`), and that was verified under a
  total forge: a `PUT` reusing an existing key still collides.

**Reachability, precisely.** Demonstrated for the three declaration loads — that is
what the forged test showed before the fix. **Not measured** for the paths listed
above: whether a real post-recovery bloom reaches them is unknown, and the live
reproduction is flaky. Unknown is not the same as safe, and neither is written here as
though it were the other. What is no longer available is the comfortable argument that these keys are old
and already compacted and therefore safe. It did not hold where it was checked.

**The balance, in one place.**

| | Status |
|---|---|
| Root cause | **not diagnosed**, no live candidate |
| Duplicate-anchor check (write path) | **closed** — confirms a miss without the bloom |
| Gravity / vector / satellite loads | **closed** by range scan — reachability **demonstrated** first |
| Pinned-field lists | **closed** by range scan — same defect, lighter consequence |
| `UNIQUE` anchor declarations | **never exposed** — `anchors.bin` lives outside the keyspace |
| `DECLARE ANCHOR` duplicate check | vulnerable by inspection, **not measured** |
| Ghost metadata load | vulnerable by inspection, **not measured** |
| Ghost source-record read | vulnerable by inspection, **not measured** — and the reason is known (above) |

**What it costs you, and the workaround.** The window is an unclean restart. A
graceful shutdown followed by a clean open does not open it. If a process did come up
from a dirty restart, `xyzdb_recovered_from_wal` in `STATS` and `/metrics` reports it
— treat that as a degraded mode, and prefer a clean restart before trusting a lobe's
declarations.

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
migration from pre-1.0 data directories.

One limitation lives elsewhere and is pointed at rather than moved: the auth token is
stored **in plaintext on disk**, stated in
[`docs/usage/reference.md`](docs/usage/reference.md) §Security beside the flag that
reads it. The README's security bullet covers the authorization model — that there is
none — which is a different limitation; an earlier version of this file sent readers
there for the plaintext one and they would not have found it. TLS 1.3 protects the
token **in transit** (`--tls-cert` / `--tls-key`); at rest it is a file on your disk.
