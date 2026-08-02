# Known issues — agentic benchmark

What we know about this harness and the numbers it produced, written down where it
travels with the code it describes.

**Scope is stated on every entry, and it matters as much as the defect.** Three of
the four below do not touch the published matrix at all; without saying so, a list
of defects reads as a retreat from results that still stand.

**The published matrix** was produced on AWS with `xyzdb:0.9.7-x86v3`. The
benchmark page says so, and also says that Mac/OrbStack runs of the same matrix are
direction only. Anything in `results/` carrying another image stamp is a direction
run, not the matrix.

A v2 of this benchmark is in progress. The last entry is what reordered it.

---

## 1 · Rival images were not pinned by digest

**What.** The matrix ran the rivals at their `:latest` images, and the per-cell
stamp recorded `xyzdb_image` precisely and the rivals not at all. So the exact rival
builds behind a published cell cannot be recovered from the result files.

**Scope: reproducibility, not correctness. The numbers stand.** Every engine ran the
same corpus, the same queries and the same tiers; what is missing is the ability to
re-create the rivals' side byte-for-byte later.

**Already declared.** The page states "rivals at their latest images", so this is a
known gap rather than an omission — but a stated gap is still a gap.

**Fix for v2.** Digest pins with a human-readable version beside them, and a stamp
carrying all four engine versions instead of one.

---

## 2 · The binary that produced the matrix is not in this history

**What.** The matrix ran `xyzdb:0.9.7-x86v3`. This repository's `main` is a single
squashed commit at `xyzDB 1.0`, so no tree corresponding to 0.9.7 exists here. The
image is the only artefact naming it.

**Scope: the exact binary is not reconstructible. The behaviour is.** What makes the
matrix's headline result work — the memtable ceiling derived from
`--memory-budget-mb`, which is why the tight tier fits at all — is documented on the
page and is still the behaviour in 1.1.0. So the claim can be re-verified even
though the binary cannot be rebuilt, and v2 will re-verify it.

**What NOT to conclude.** This does not make the published numbers wrong. It makes
them unrepeatable against that exact build, which is a different and smaller
statement.

---

## 3 · Nine harness defects found while building v2 — none touches the published matrix

Found in one sweep, all in the instrument rather than in any engine. Five live in
scripts written for v2 that did not exist when the matrix ran; four are in
infrastructure that also did not exist then.

| # | Defect | Where |
|---|---|---|
| 1 | `docker exec` + a fresh `psql` per timed repeat — process startup reported as query latency | v2 direction scripts |
| 2 | `pg_total_relation_size` on a partitioned parent returns 0 for a table with rows | v2 |
| 3 | `EXPLAIN ANALYZE` parsed the relation as `loops=1)` — the trailing token, not the name | v2 |
| 4 | `--out` written inside the container's own `/tmp`, so results vanished with it | v2 infra |
| 5 | A port default without the offset pointed at the developer's own postgres | v2 infra |
| 6 | `default="127.0.0.1"` inside a container is the container itself, and refuses connections exactly like a dead engine | v2 infra |
| 7 | `cargo fmt`/`clippy` not run locally, so CI caught formatting the tests | v2 infra |
| 8 | A hardcoded qdrant collection name (`bench` vs the `mem` the adapter creates) — in two copies | v2 |
| 9 | Rivals loaded with `scoped=False`, measuring pg's flat arm while labelling it the partitioned one | v2 |

**Scope: none of the nine reaches the published matrix.** The page names
`run_envelope_full.sh` as its harness; that script drives `measure_s1.py` …
`measure_s6.py`, and none of the six contains `psql`, `docker exec`, or a subprocess
in a timed path. They measure through `adapters.py`, whose `PgAdapter` opens one
persistent `psycopg2` connection in its constructor. Defect 1 in particular — the
one that would have biased *against* pg — exists only in the v2 direction scripts.

**Two are worth keeping as method rather than as history.** A default that looks
reasonable and, in the wrong environment, answers with something that reads as
another thing entirely (5, 6) is the same family as an unpinned `:latest`. The fix
that worked was not validating the value but **printing the effective one** before
touching an engine.

---

## 4 · The finding that reordered v2: three engines, one mechanism

Not a defect. The result.

In the regime the matrix measures, **all three engines converge on the same
mechanism — bound the candidate set, then scan it exactly** — and they arrive there
by different routes:

- **pgvector** prunes to one partition leaf and its planner chooses `Seq Scan` +
  `Sort`. Measured with `EXPLAIN ANALYZE` taken both before and after the per-leaf
  HNSW exists: the plan is identical, and at three of four leaf sizes the query is
  *slower* with the index. **Range measured: up to 25,000 rows per leaf at 1024
  dimensions.** Outside that range this says nothing.
- **qdrant** drops below its own `full_scan_threshold` (10,000 points, its default,
  left untouched) and scans exactly.
- **xyzDB** bounds by declaration and scans exactly, with no planner and no
  threshold.

So it is not "pg fails to use its index". **The pg planner concludes what xyzDB
assumes**, and in this regime approximate search does not pay.

### The declared limit, which is what makes the rest credible

Above roughly 25,000 bounded rows with a wide filter, the graph wins, and not by a
little. Measured on 123,490 bounded vectors: **qdrant 13.31 ms against xyzDB
190.16 ms — 14× — with recall 1.0 on both.** That is the regime HNSW exists for, and
it is where xyzDB has the most to lose.

**Consequence for v2.** A grid whose cells all sit below the crossing compares
implementation constants rather than mechanisms. v2 moves its measured cells to
where the bounded set lands between ~25k and ~250k, so there are three different
mechanisms to compare instead of one — and publishes the cell above the crossing
without decoration.
