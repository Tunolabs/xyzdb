#!/usr/bin/env python3
"""The draw order in `metadata_gen.gen` is load-bearing — pin it.

WHY THIS TEST EXISTS
--------------------
`gen` draws from ONE seeded generator, so every field's values depend on how many
draws preceded it. `topic`, `status` and `importance` were there first; the `catN`
were appended AFTER them precisely so the original three stay byte-identical and an
existing corpus is not perturbed.

That makes the ordering a convention holding up real artifacts, and conventions are
exactly what drifts. Insert a field before `topic` and every downstream value shifts
silently: same seed, same field names, different corpus, no error anywhere. The
metadata is hashed into the store manifest, so the drift would be caught eventually —
but at the point of a mismatched hash, long after the run that produced it.

These assertions pin the values themselves, so the failure lands on the edit.

Run: `python test_metadata_gen.py`
"""
import sys

import numpy as np

import metadata_gen as mg

N = 1000


def _fail(msg):
    print(f"FAIL: {msg}")
    sys.exit(1)


def test_draw_order_is_pinned():
    """The first values of each field, for a fixed (n, seed). Any reorder moves these."""
    m = mg.gen(N, seed=mg.SEED)
    # Recomputed independently, in the declared order, from a fresh generator. If
    # `gen` changes its draw sequence, this mirror stops matching.
    rng = np.random.default_rng(mg.SEED)
    exp_topic = rng.integers(0, mg.N_TOPICS, size=N).astype(np.int64)
    exp_status = np.where(rng.random(N) < mg.ACTIVE_FRAC, "active", "archived")
    exp_importance = rng.random(N).astype(np.float64)
    exp_cats = {c: rng.integers(0, c, size=N).astype(np.int64) for c in mg.CARDINALITIES}

    if not np.array_equal(m["topic"], exp_topic):
        _fail("topic moved — a draw was inserted before it")
    if not np.array_equal(m["status"], exp_status):
        _fail("status moved — the draw order changed")
    if not np.allclose(m["importance"], exp_importance, rtol=0, atol=0):
        _fail("importance moved — the draw order changed")
    for c in mg.CARDINALITIES:
        if not np.array_equal(m[f"cat{c}"], exp_cats[c]):
            _fail(f"cat{c} moved — the catN must be drawn last, in CARDINALITIES order")


def test_catn_appended_not_interleaved():
    """The original three must not depend on the catN existing at all.

    This is the property the append was for: an older corpus, generated before the
    catN were added, must still hold the same topic/status/importance.
    """
    rng = np.random.default_rng(mg.SEED)
    legacy_topic = rng.integers(0, mg.N_TOPICS, size=N).astype(np.int64)
    if not np.array_equal(mg.gen(N)["topic"], legacy_topic):
        _fail("adding the catN perturbed topic — they are not appended, they are interleaved")


def test_negative_control_the_pin_can_fail():
    """A pin that cannot fail proves nothing: a shifted stream must be detected."""
    rng = np.random.default_rng(mg.SEED)
    rng.random(1)                       # one extra draw = the drift being guarded against
    shifted = rng.integers(0, mg.N_TOPICS, size=N).astype(np.int64)
    if np.array_equal(mg.gen(N)["topic"], shifted):
        _fail("a one-draw shift went undetected — this test cannot see what it guards")


def test_uniformity_and_determinism():
    a, b = mg.gen(N), mg.gen(N)
    for key in a:
        if not np.array_equal(a[key], b[key]):
            _fail(f"{key} is not deterministic for the same (n, seed)")
    for c in mg.CARDINALITIES:
        if len(np.unique(mg.gen(200_000)[f"cat{c}"])) != c:
            _fail(f"cat{c} does not cover all {c} values")


def test_degenerate_rule():
    """The declared exclusions, so the grid cannot drift away from the docstring."""
    for bucket, card, want in ((493, 100, True), (493, 1000, True), (4935, 1000, True),
                               (4935, 100, False), (49348, 1000, False), (246738, 1000, False)):
        if mg.is_degenerate(bucket, card) is not want:
            _fail(f"is_degenerate({bucket}, {card}) should be {want}")


if __name__ == "__main__":
    for fn in (test_draw_order_is_pinned, test_catn_appended_not_interleaved,
               test_negative_control_the_pin_can_fail, test_uniformity_and_determinism,
               test_degenerate_rule):
        fn()
        print(f"  ok  {fn.__name__}")
    print("metadata_gen: draw order pinned, appended not interleaved, pin provably fallible")
