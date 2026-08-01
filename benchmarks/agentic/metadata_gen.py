#!/usr/bin/env python3
"""Deterministic synthetic metadata for S5 (hybrid filter) and S6 (composite turn).

One generator serves both scenarios (design §6.2 fleco). Seeded by turn count +
a fixed seed (same family as build_lme, INV-2), so all four engines get
byte-identical fields — no new on-disk artifact, regenerated from (n, seed).

Per-turn fields:
  topic      int in [0, N_TOPICS)  — uniform. S5b sweeps selectivity via `topic < T`
                                      (= T/N_TOPICS); a RANGE, so it cannot exercise
                                      the satellite axis, which bounds EQUALITY only.
  status     "active" | "archived" — ACTIVE_FRAC active. Q4 AGGREGATE keeps active.
  importance float in [0,1)        — Q4 AGGREGATE avg(importance).
  catN       int in [0, N)         — N in {2, 10, 100, 1000}. The EQUALITY fields the
                                      satellite axis needs (v2 Q3). See below.

WHY catN AND NOT `topic`
------------------------
Satellites bound equality (one sub-range of the key), not ranges: `topic < T` on the
axis is T sub-ranges, worse than sweeping. And with equality the selectivity is fixed
by the field's CARDINALITY, not by a threshold — so sweeping selectivity means varying
cardinality, which `topic` cannot do at a fixed N_TOPICS. Hence four independent
fields, one per point of the sweep: 1/2, 1/10, 1/100, 1/1000.

**Independent, not nested.** One axis per lobe means each cardinality point is its own
lobe with its own axis; there is nothing to nest. Hierarchical fields would only be
needed if the two-level u16 split ever lands (deferred, see the debt register).

**Uniform by design, and that is a declared limitation.** Uniformity makes selectivity
exactly 1/N and identical for all four engines, which is what makes the point
comparable. Real categories are skewed, and skew would produce uneven satellites —
some fat, some empty. That is a known limitation of this corpus, not something v1
measured and not something v2 claims to.

DEGENERATE CELLS — DECLARED OUT BEFORE RUNNING, NOT RATIONALISED AFTER
---------------------------------------------------------------------
The two axes multiply: rows per satellite = bucket size / cardinality.

    buckets  rows/bucket    cat2    cat10   cat100   cat1000
        500          493     247       49      [5]     [0.5]
         50        4,935   2,467      493       49       [5]
          5       49,348  24,674    4,935      493        49
          1      246,738 123,369   24,674    2,467       247

With k=10, a cell holding fewer than ~10 rows per satellite **cannot fill the top-k**.
It does not measure 0.1% selectivity; it measures "there is nothing there". The same
holds for the rivals — an HNSW over 5 candidates means nothing. The three bracketed
cells are excluded up front by `is_degenerate()`.

Where the headline lives, from the same table: **big bucket AND high cardinality** —
1 bucket x cat100/cat1000 (sweeping 246,738 against 2,467 or 247) and 5 buckets x
cat100. Those are the flagship cells; the rest of the usable band is context.
"""
import numpy as np

N_TOPICS = 1000        # topic<T ⇒ selectivity T/1000: 500→50%, 100→10%, 10→1%, 5→0.5%, 1→0.1%
ACTIVE_FRAC = 0.7
SEED = 20260701        # same seed as build_lme (INV-2)

# The S5b selectivity sweep (fractions) and the topic<T thresholds they map to.
SELECTIVITIES = [0.5, 0.1, 0.01, 0.005, 0.001]

# Q3 (S5a): equality selectivity comes from cardinality — 1/2, 1/10, 1/100, 1/1000.
CARDINALITIES = (2, 10, 100, 1000)

# A satellite holding fewer than this many rows cannot fill a top-k of 10, so the
# cell measures emptiness rather than selectivity.
MIN_ROWS_PER_SATELLITE = 10


def gen(n, seed=SEED):
    """Return the per-turn metadata, deterministic in (n, seed).

    Args:
        n: Number of turns.
        seed: Generator seed; the same family as `build_lme` (INV-2).

    Returns:
        ``{'topic', 'status', 'importance', 'cat2', 'cat10', 'cat100', 'cat1000'}``.
        Every array has length ``n``; the ``catN`` are uniform over ``[0, N)``.
    """
    rng = np.random.default_rng(seed)
    topic = rng.integers(0, N_TOPICS, size=n).astype(np.int64)
    status = np.where(rng.random(n) < ACTIVE_FRAC, "active", "archived")
    importance = rng.random(n).astype(np.float64)
    out = {"topic": topic, "status": status, "importance": importance}
    # Drawn after the three above so adding them cannot shift those streams — an
    # existing corpus keeps the same topic/status/importance for the same (n, seed).
    for card in CARDINALITIES:
        out[f"cat{card}"] = rng.integers(0, card, size=n).astype(np.int64)
    return out


def threshold(sel):
    """`topic < T` threshold for a target selectivity fraction `sel` (S5b, range form)."""
    return max(1, round(sel * N_TOPICS))


def rows_per_satellite(bucket_size, cardinality) -> float:
    """Expected rows sharing one satellite value inside one bucket."""
    return bucket_size / cardinality


def is_degenerate(bucket_size, cardinality, k=10) -> bool:
    """Whether a (bucket size x cardinality) cell is too thin to measure anything.

    Below `MIN_ROWS_PER_SATELLITE` — or below `k`, whichever binds — the satellite
    cannot fill the top-k, so the cell reports emptiness rather than selectivity, for
    every engine alike. Excluded before running rather than explained afterwards.
    """
    return rows_per_satellite(bucket_size, cardinality) < max(k, MIN_ROWS_PER_SATELLITE)
