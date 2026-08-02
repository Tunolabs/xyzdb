#!/usr/bin/env python3
"""Gate: the satellite axis is a PURE OPTIMISATION — same rows, same order.

WHAT IS BEING ASSERTED, AND WHY NOT RECALL
------------------------------------------
A satellite bounds a query to one sub-range of the gravity bucket. It must return
exactly what the unbounded scan returns: same rows, in the same order. So the gate
compares **row by row including position**, not recall.

Equal recall is a weaker claim that passes while the ordering changes, and this
corpus has the ties to make that happen — measured at 4.2% of queries at the user
granularity and 5.8% at big_group. Under an id-set or recall comparison a reordered
answer looks identical. It is not: for a pure optimisation, one differing row or one
swapped pair is a bug, not an improvement.

This mirrors the engine's own G1 gate (bounded route against a forced parent scan,
row for row including order), which is the standard this arm has to meet.

THE SECOND CONTROL: PROVE THE REQUEST TOOK THE BOUNDED ROUTE
-----------------------------------------------------------
Declaring the axis is not using it. If the query does not pin equality on the axis
field, the engine sweeps the parent — **same results, no speed-up, and an
equivalence test passes exactly the same**. A green that never touched the gate it
claims to test proves nothing.

Latency is a weak witness, especially on a laptop. The discriminating signal is
binary and hardware-independent: run with a tiny `--nearest-budget-ms` and watch for
`budget_stop`. Bounded to one satellite the candidate set is small and the query
completes; sweeping the parent it is the whole bucket and the airbag fires. The flag
appearing or not is the proof that the route differed — not how many milliseconds it
took.
"""
import argparse
import json
import sys

import numpy as np

sys.path.insert(0, "/Applications/Projects/Tunolabs/xyz/xyzdb/examples/client/python")


def _ids(resp):
    """Ordered row ids from a response, as the engine returned them."""
    return [r["id"] for r in resp.get("records", []) if "id" in r]


def compare_ordered(bounded, parent):
    """First position where the two answers diverge, or None if identical.

    Returns a dict describing the divergence so a failure names the row rather than
    only the count.
    """
    if len(bounded) != len(parent):
        return {"kind": "length", "bounded": len(bounded), "parent": len(parent)}
    for i, (a, b) in enumerate(zip(bounded, parent)):
        if a != b:
            return {"kind": "order_or_row", "position": i, "bounded": a, "parent": b}
    return None


def satellite_oracle(qvec, vecs, rows, k):
    """Independent truth: the top-k among `rows`, computed in numpy, not by the engine.

    Comparing the bounded answer only against another engine query would let a shared
    bug pass twice. This is the anchor: f32 products, f64 reduction, no engine
    involved.
    """
    import recall_harness as rh
    scores = rh.exact_scores(qvec, vecs[rows])
    order = np.argsort(-scores, kind="stable")[:k]
    return [f"g{int(rows[i])}" for i in order]


def run(db, lobe_sat, lobe_plain, axis, axis_value, bucket, qvec, k):
    """The same question asked three ways.

    - `lobe_sat` declares the axis, so equality on it resolves a key sub-range.
    - `lobe_plain` holds the identical rows with NO axis declared, so the same
      equality is a residual filter over the whole bucket — a different route to the
      same answer. This is what makes the comparison a route comparison; the engine's
      own force-parent knob is an in-process atomic and cannot be reached over TCP.
    - The oracle (above) is neither.

    A first attempt compared the bounded answer against the parent bucket's top-k
    filtered down to the satellite. That was wrong and the gate said so immediately:
    filtering a top-10 of the whole bucket to one of ten categories leaves about one
    row, not ten, so it reported `parent: 0` rows. The parent's top-k is not the
    satellite's top-k, and validating the gate on a small case before spending it at
    246k is what caught it.
    """
    qs = json.dumps([float(x) for x in qvec])
    stmt = (f'WHERE bucket = "{bucket}" AND {axis} = {axis_value} '
            f'| NEAREST {k} BY emb TO {qs} USING cosine')
    return (db.execute(f'SCAN "{lobe_sat}" {stmt}'),
            db.execute(f'SCAN "{lobe_plain}" {stmt}'))


def prove_route(db, lobe_sat, lobe_plain, axis, axis_value, bucket, qvec, k) -> dict:
    """Prove the two queries took DIFFERENT routes, without trusting latency.

    Run against an engine started with a tiny `--nearest-budget-ms`. Bounded to one
    satellite the candidate set is small enough to finish; sweeping the parent bucket
    it is not, and the airbag returns a partial carrying `budget_stop`. The presence
    or absence of that flag is binary and does not depend on how fast the machine is,
    which is what makes it usable on a laptop.

    A run where NEITHER truncates means the budget was not tight enough to
    discriminate — the instrument could not see the effect, so the result is
    inconclusive rather than a pass.
    """
    qs = json.dumps([float(x) for x in qvec])
    stmt = (f'WHERE bucket = "{bucket}" AND {axis} = {axis_value} '
            f'| NEAREST {k} BY emb TO {qs} USING cosine')

    def ask(lobe):
        """Three possible outcomes, because the airbag has TWO paths.

        Expiring during HYDRATION degrades to a prefix-correct partial carrying
        `budget_stop`. Expiring during the SCORING SCAN is a hard error by design —
        which is what a full 246,738-candidate sweep hits first. The first version of
        this gate only knew about the partial and died on the error, so it could not
        read the very signal it was built to read.
        """
        try:
            r = db.execute(f'SCAN "{lobe}" {stmt}')
            return ("partial", r.get("budget_stop")) if r.get("budget_stop") else ("complete", None)
        except Exception as e:
            msg = str(e)
            return ("airbag_error", msg[:120]) if "budget" in msg else ("error", msg[:120])

    b_kind, b_info = ask(lobe_sat)
    p_kind, p_info = ask(lobe_plain)
    cut_short = {"partial", "airbag_error"}

    if b_kind == "complete" and p_kind in cut_short:
        verdict = (f"PASS — bounded completed while the parent {p_kind}: the two "
                   "queries did NOT take the same route")
    elif b_kind == "complete" and p_kind == "complete":
        verdict = ("INCONCLUSIVE — neither was cut short; the budget is too generous "
                   "to discriminate, so this run proves nothing about the route")
    elif b_kind in cut_short and p_kind in cut_short:
        verdict = ("INCONCLUSIVE — both were cut short; the budget is too tight and "
                   "the bounded path never got to finish either")
    elif b_kind in cut_short:
        verdict = "FAIL — the BOUNDED query was cut short while the parent completed"
    else:
        verdict = f"ERROR — unexpected outcome: bounded={b_kind} parent={p_kind}"
    return {"gate": "bounded_route_taken",
            "bounded": {"outcome": b_kind, "detail": b_info},
            "parent": {"outcome": p_kind, "detail": p_info},
            "verdict": verdict}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--lobe", default="mem_sat", help="lobe WITH the axis declared")
    ap.add_argument("--lobe_plain", default="mem_plain", help="identical rows, NO axis")
    ap.add_argument("--axis", required=True, help="the declared satellite field")
    ap.add_argument("--store", required=True)
    ap.add_argument("--axis_point", type=int, required=True)
    ap.add_argument("--queries", type=int, default=50)
    ap.add_argument("--k", type=int, default=10)
    ap.add_argument("--prove-route", action="store_true",
                    help="run the route proof instead (needs a tiny --nearest-budget-ms)")
    args = ap.parse_args()

    sys.path.insert(0, "/Applications/Projects/Tunolabs/xyz/xyzdb/benchmarks/agentic")
    from bucket_axis import load_point
    from xyzdb_minimal import connect

    c = load_point(args.store, args.axis_point)
    db = connect(args.host, args.port, timeout=300.0)

    axis_vals = c["fields"][args.axis]
    bids = c["bucket_ids"]

    if args.prove_route:
        j = 0
        b = int(c["q_bucket"][j])
        rows = np.flatnonzero(bids == b)
        val = axis_vals[rows[0]]
        lit = f'"{val}"' if isinstance(val, (str, np.str_)) else int(val)
        out = prove_route(db, args.lobe, args.lobe_plain, args.axis, lit, b,
                          c["qvecs"][j], args.k)
        out.update({"axis": args.axis, "axis_point": args.axis_point,
                    "bucket_rows": int(len(rows)),
                    "satellite_rows": int((axis_vals[rows] == val).sum())})
        print(json.dumps(out, indent=1))
        db.close()
        sys.exit(0 if out["verdict"].startswith("PASS") else 1)

    vecs = c["vecs"]
    checked = bad_route = bad_truth = skipped = 0
    first_bad = None
    for j in range(min(args.queries, len(c["qvecs"]))):
        b = int(c["q_bucket"][j])
        rows = np.flatnonzero(bids == b)
        if len(rows) == 0:
            continue
        val = axis_vals[rows[0]]
        sat_rows = rows[axis_vals[rows] == val]
        if len(sat_rows) < args.k:
            skipped += 1        # a satellite too thin to fill top-k: declared out, §catN
            continue
        lit = f'"{val}"' if isinstance(val, (str, np.str_)) else int(val)
        sat, plain = run(db, args.lobe, args.lobe_plain, args.axis, lit, b,
                         c["qvecs"][j], args.k)
        truth = satellite_oracle(c["qvecs"][j], vecs, sat_rows, args.k)
        checked += 1
        d_route = compare_ordered(_ids(sat), _ids(plain))
        d_truth = compare_ordered(_ids(sat), truth)
        if d_route is not None:
            bad_route += 1
            first_bad = first_bad or {"against": "undeclared lobe", "query": j,
                                      "bucket": b, "axis_value": str(val), **d_route}
        if d_truth is not None:
            bad_truth += 1
            first_bad = first_bad or {"against": "oracle", "query": j, "bucket": b,
                                      "axis_value": str(val), **d_truth}

    verdict = "PASS" if (bad_route == 0 and bad_truth == 0 and checked > 0) else "FAIL"
    if checked == 0:
        verdict = "INCONCLUSIVE — every satellite was too thin to fill top-k"
    print(json.dumps({"gate": "satellite_equivalence_ordered", "axis": args.axis,
                      "axis_point": args.axis_point, "queries_checked": checked,
                      "skipped_thin_satellites": skipped,
                      "mismatched_vs_undeclared_lobe": bad_route,
                      "mismatched_vs_oracle": bad_truth,
                      "first_divergence": first_bad, "verdict": verdict}, indent=1))
    mismatched = bad_route + bad_truth
    db.close()
    sys.exit(0 if mismatched == 0 else 1)


if __name__ == "__main__":
    main()
