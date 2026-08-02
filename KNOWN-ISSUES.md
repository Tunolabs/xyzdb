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
That includes the unbounded `FIND`/`PULL` result sets, the plaintext auth token, and
the absence of an in-place migration from pre-1.0 data directories.
