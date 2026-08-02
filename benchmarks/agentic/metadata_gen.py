#!/usr/bin/env python3
"""Deterministic synthetic metadata for S5 (hybrid filter) and S6 (composite turn).

One generator serves both scenarios (design §6.2 fleco). Seeded by turn count +
a fixed seed (same family as build_lme, INV-2), so all four engines get
byte-identical fields — no new on-disk artifact, regenerated from (n, seed).

Per-turn fields:
  topic      int in [0, N_TOPICS)  — uniform. S5 sweeps selectivity via `topic < T`
                                      (= T/N_TOPICS); S6 filters one topic value.
  status     "active" | "archived" — ACTIVE_FRAC active. S6 AGGREGATE keeps active.
  importance float in [0,1)        — S6 AGGREGATE avg(importance).
"""
import numpy as np

N_TOPICS = 1000        # topic<T ⇒ selectivity T/1000: 500→50%, 100→10%, 10→1%, 5→0.5%, 1→0.1%
ACTIVE_FRAC = 0.7
SEED = 20260701        # same seed as build_lme (INV-2)

# The S5 selectivity sweep (fractions) and the topic<T thresholds they map to.
SELECTIVITIES = [0.5, 0.1, 0.01, 0.005, 0.001]


def gen(n, seed=SEED):
    """Return {'topic': int64[n], 'status': str[n], 'importance': f64[n]}."""
    rng = np.random.default_rng(seed)
    topic = rng.integers(0, N_TOPICS, size=n).astype(np.int64)
    status = np.where(rng.random(n) < ACTIVE_FRAC, "active", "archived")
    importance = rng.random(n).astype(np.float64)
    return {"topic": topic, "status": status, "importance": importance}


def threshold(sel):
    """`topic < T` threshold for a target selectivity fraction `sel`."""
    return max(1, round(sel * N_TOPICS))
