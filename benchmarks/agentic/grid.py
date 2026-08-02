#!/usr/bin/env python3
"""The measured grid — which (locality point × cardinality) cells the matrix runs.

WHY THIS FILE EXISTS RATHER THAN A NESTED LOOP
----------------------------------------------
The first grid was the full cross product of four locality points and four
cardinalities. Measuring the route of all three engines showed what that grid
actually asks: **fifteen of its sixteen cells are an exact scan in every engine**.
pg prunes to a leaf and its planner picks a seq scan; qdrant drops below its
`full_scan_threshold` and scans exactly; xyzDB bounds and scans by design. Those
cells compare implementation constants, which is a legitimate question and not the
one the benchmark was built to ask.

Running it anyway would have produced an artefact that LOOKS like the matrix —
complete, stamped, four versions per cell — and answers a question nobody asked in
twelve of its cells. That artefact is harder to discard later than to not build
now, because once it exists it gets cited.

WHAT DECIDES A CELL
-------------------
The bounded candidate set, not the selectivity. Three mechanisms exist to compare
only above qdrant's threshold (10,000 points, its default, left alone):

    bounded set = rows_per_bucket / cardinality

The measured crossing sits between 25,057 points (graph) and 5,039 (exact), so the
grid moves to where the set lands between ~25k and ~250k. No new fields are needed
— those cells already exist in the sealed corpus, at its COARSE locality points.

WHAT THE COARSE CELLS DO AND DO NOT GIVE THE RESOURCE QUESTION
--------------------------------------------------------------
In the coarse regime xyzDB scans 123k or 246k vectors per query — about 1 GB of
embeddings against a 64 MB cache at the tight tier. That is the disk-bound regime,
and it is where xyzDB has everything to lose.

It is NOT free cells for the resource question, and an earlier draft of this file
claimed it was. **The runners start the engine with `--nearest-budget-ms 0`**
(`run_aws.sh:56`, `run_beforeafter.sh:26`, "forces exact — mono never truncates").
With the airbag off the coarse cells are exact and slow, never truncated: `recall`
stays 1.0 and the S5a comparison is clean, which is exactly why the setting is
there and it stays.

But a flag that is switched off never fires. The resource question needs its OWN
arm, with a finite budget **declared as a cell condition**, because with budget 0
`budget_stop` cannot appear no matter how large the scan gets. One run does not buy
two questions here; it buys one question and a well-chosen regime for the second.

WHAT IS DELIBERATELY NOT HERE
-----------------------------
Anything harder. The matrix covers the space of real agentic working shapes, not
the space of configurations that hurt. The test of fairness was that nobody could
name a common query shape that was missing — not that nobody could name a harder
case. There is always a harder case.
"""
from dataclasses import dataclass

# Rows per bucket at each sealed locality point of the 246,738-turn corpus.
ROWS_PER_BUCKET = {"pool": 246_738, "big_group": 49_348, "group": 4_935, "user": 493}

# qdrant's documented default. Below it qdrant abandons the graph and scans
# exactly; it is NOT tuned, because leaving a rival's sensible default alone is
# the same discipline that refuses a strawman for ourselves.
QDRANT_FULL_SCAN_THRESHOLD = 10_000


@dataclass(frozen=True)
class Cell:
    point: str          # locality point (bucket_axis)
    cardinality: int    # catN field
    role: str           # "measured" | "context"

    @property
    def bounded_set(self) -> int:
        """Rows a query actually has to consider after gravity + the equality."""
        return ROWS_PER_BUCKET[self.point] // self.cardinality

    @property
    def qdrant_uses_graph(self) -> bool:
        return self.bounded_set >= QDRANT_FULL_SCAN_THRESHOLD

    @property
    def lobe(self) -> str:
        return f"mem_cat{self.cardinality}"

    def describe(self) -> dict:
        return {"point": self.point, "cardinality": self.cardinality,
                "bounded_set": self.bounded_set, "role": self.role,
                "qdrant_mechanism": "graph" if self.qdrant_uses_graph else "exact",
                "lobe": self.lobe}


# The measured cells: three above the crossing, two below it as contrast. Below the
# crossing every engine scans exactly, so those two are what makes "above" mean
# something — a grid with only the coarse cells could not show the convergence it
# claims to have found.
MEASURED = (
    Cell("pool", 2, "measured"),        # ~123,369 — the deepest, disk-bound for xyz
    Cell("pool", 10, "measured"),       # ~24,673 — just above the crossing
    Cell("big_group", 2, "measured"),   # ~24,674 — same set size, different locality
    Cell("big_group", 10, "measured"),  # ~4,934 — just below: all three scan
    Cell("pool", 100, "measured"),      # ~2,467 — well below
)

# Kept as DECLARED context, not run: we already know what they say, and saying it
# without spending a run is more honest than spending one to confirm it. Naming
# them is the point — a grid that silently dropped them would look like it had not
# considered them.
CONTEXT = (
    Cell("group", 2, "context"),        # ~2,467
    Cell("user", 2, "context"),         # ~246 — the original v1 shape
)


# ─── Signed before the run ───────────────────────────────────────────────────
#
# `pool×10` and `big_group×2` bound to the same number of rows (24,673 vs 24,674)
# out of parent buckets of 246,738 and 49,348. The pair separates SET SIZE from
# LOCALITY: identical work per query, wildly different parent.
#
# Predicted, and recorded here rather than after the fact, because a prediction
# written afterwards explains any result:
#
#   xyzDB      — near-identical. The satellite bounds to the same rows and the
#                parent's size must not leak into the cost. If they match, THAT is
#                the strong result: it shows the bounding works and that a large
#                parent is not paid for. If they diverge, the design does not
#                predict it and it is worth stopping to look at.
#   pgvector   — asymmetric BY CONSTRUCTION: one partition with 10 leaves against
#                five partitions with 2. Divergence expected, and it is data.
#   qdrant     — asymmetric too: one tenant against five. Same.
#
# So a divergence means opposite things per engine, which is why it is written down
# per engine instead of as one expectation.
PREDICTION_SAME_SET_DIFFERENT_PARENT = {
    "pair": ("pool×10", "big_group×2"),
    "bounded_set": (24_673, 24_674),
    "xyzdb": "near-identical latency; a match is the positive result",
    "pgvector": "divergence expected — 1 partition/10 leaves vs 5 partitions/2",
    "qdrant": "divergence expected — 1 tenant vs 5",
    "signed_before_running": True,
}


def summary() -> list:
    return [c.describe() for c in MEASURED + CONTEXT]


if __name__ == "__main__":
    import json
    for row in summary():
        print(json.dumps(row))
