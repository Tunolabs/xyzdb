#!/usr/bin/env python3
"""P6: can PostgreSQL adopt the multilevel partitioning the satellite arm needs?

WHAT IS BEING DECIDED
---------------------
The S5a flagship gives every engine its structurally-equivalent form. For
postgres that is `PARTITION BY LIST(bucket)` with each partition sub-partitioned
by the filter field — the native analogue of gravity + satellite. If pg cannot
build or plan it at a point of the bucket axis, that arm needs its second-best
form, and the honest thing is to name which and why BEFORE running, not to leave
a blank cell.

Leaf partitions = n_buckets x cardinality. At the small-bucket end of the axis
that is 500 x K, which is thousands of leaves; the prediction (P6, confidence
medium) is that the planner degrades there.

This is VIABILITY, not latency: partitions either build and plan or they do not.
That verdict transfers off this machine, which is why it belongs in the local
phase. Nothing here is a publishable timing — the Mac is arm64 while the
publishable box is x86, and for xyzDB the image is built with `target-cpu=
x86-64-v3` only on x86, so local numbers come from a different binary. The
seconds recorded below are for spotting a cliff, not for quoting.

WHAT COUNTS AS "CAN'T"
----------------------
Declared before running, so the verdict is not negotiated after seeing it:
  - DDL that exceeds `--ddl-timeout` (default 300 s), or errors.
  - A planner that cannot produce a plan inside `--plan-timeout` (default 30 s).
  - A plan that scans every leaf instead of pruning to one (partition pruning is
    the entire point; without it the structure costs and buys nothing).
The third is the interesting failure: it builds, it answers, and it is useless.
"""
import argparse
import json
import os
import subprocess
import time

DDL_TIMEOUT_S = 300
PLAN_TIMEOUT_S = 30


def psql(container: str, sql: str, timeout: int) -> tuple[int, str, float]:
    """Run SQL, returning (exit, output, seconds). Never raises on SQL error.

    SQL goes in on **stdin**, not as an argv element. The first version of this
    passed it to `psql -c` and the probe reported `pg` as non-viable at 5,000 leaf
    partitions with `exec /usr/bin/psql: argument list too long` — which is ARG_MAX
    on the `docker exec` command line, not a postgres limit. The harness had failed
    before the system, and the tell was in the data: that "DDL failure" took 0.05 s,
    *faster* than the 500-leaf case that succeeded. Creating five thousand tables
    cannot be quicker than creating five hundred. Any verdict of "cannot" must come
    from postgres, so the transport must be able to carry the question.
    """
    t0 = time.perf_counter()
    try:
        r = subprocess.run(
            ["docker", "exec", "-i", "-e", "PGPASSWORD=bench", container,
             "psql", "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-tAq", "-f", "-"],
            input=sql, capture_output=True, text=True, timeout=timeout)
        return r.returncode, (r.stdout or r.stderr).strip(), time.perf_counter() - t0
    except subprocess.TimeoutExpired:
        return 124, f"TIMEOUT after {timeout}s", time.perf_counter() - t0


def fresh_pg(container: str, image: str, timeout: int = 60) -> bool:
    """Replace `container` with a new one from `image`, waiting until it accepts.

    A fresh container per point, which is what the matrix itself does ("contenedor
    fresco y datadir borrado"). The first version reused one container and reset it
    with `DROP TABLE IF EXISTS p6 CASCADE` — whose exit code it then ignored. The
    drop silently did not happen, 5,051 objects survived into the next point, and
    that point reported itself non-viable with `relation "p6" already exists`: a
    second fabricated verdict from leftover state, in the same probe as the first.
    Recreating removes the destructive statement and the cross-point contamination
    together, which is cheaper than getting the cleanup right.
    """
    subprocess.run(["docker", "rm", "-f", container],
                   capture_output=True, text=True, timeout=60)
    r = subprocess.run(["docker", "run", "-d", "--name", container,
                        "-e", "POSTGRES_PASSWORD=bench", image],
                       capture_output=True, text=True, timeout=120)
    if r.returncode != 0:
        return False
    for _ in range(timeout):
        p = subprocess.run(["docker", "exec", container, "pg_isready", "-U", "postgres"],
                           capture_output=True, text=True, timeout=30)
        if p.returncode == 0:
            return True
        time.sleep(1)
    return False


def probe(container: str, n_buckets: int, cardinality: int, args) -> dict:
    """Build one (n_buckets x cardinality) partitioned table and try to plan against it."""
    leaves = n_buckets * cardinality
    rec = {"n_buckets": n_buckets, "cardinality": cardinality, "leaf_partitions": leaves}

    if not fresh_pg(container, args.image):
        rec.update({"viable": None, "failed_at": "harness",
                    "err": "could not start a fresh postgres — this is the probe, not pg"})
        return rec
    rc, out, _ = psql(container, "CREATE EXTENSION IF NOT EXISTS vector;", args.ddl_timeout)

    ddl = ["CREATE TABLE p6 (bucket int, kind int, gid int, emb vector(64)) "
           "PARTITION BY LIST (bucket);"]
    for b in range(n_buckets):
        ddl.append(f"CREATE TABLE p6_b{b} PARTITION OF p6 FOR VALUES IN ({b}) "
                   f"PARTITION BY LIST (kind);")
        for c in range(cardinality):
            ddl.append(f"CREATE TABLE p6_b{b}_k{c} PARTITION OF p6_b{b} FOR VALUES IN ({c});")

    rc, out, secs = psql(container, "\n".join(ddl), args.ddl_timeout)
    rec["ddl_s"] = round(secs, 2)
    if rc != 0:
        rec.update({"viable": False, "failed_at": "ddl", "err": out[:200]})
        return rec

    # Sanity: postgres must agree the leaves exist. A DDL that "succeeds" without
    # creating them would make every downstream verdict meaningless.
    rc, out, _ = psql(container,
                      "SELECT count(*) FROM pg_class WHERE relname LIKE 'p6\\_b%\\_k%';",
                      args.ddl_timeout)
    rec["leaves_created"] = int(out) if rc == 0 and out.isdigit() else -1
    if rec["leaves_created"] != leaves:
        rec.update({"viable": False, "failed_at": "ddl_incomplete",
                    "err": f"expected {leaves} leaves, found {rec['leaves_created']}"})
        return rec

    # Does the planner prune to ONE leaf? That is what the structure is for.
    rc, out, secs = psql(
        container,
        "EXPLAIN (COSTS OFF) SELECT gid FROM p6 WHERE bucket = 0 AND kind = 0 "
        "ORDER BY emb <=> '[" + ",".join(["0.1"] * 64) + "]' LIMIT 10;",
        args.plan_timeout)
    rec["plan_s"] = round(secs, 2)
    if rc != 0:
        rec.update({"viable": False, "failed_at": "plan", "err": out[:200]})
        return rec

    scanned = out.count("p6_b")
    rec["leaves_in_plan"] = scanned
    rec["pruned_to_one"] = scanned == 1
    rec["viable"] = bool(scanned == 1
                         and rec["ddl_s"] < args.ddl_timeout
                         and rec["plan_s"] < args.plan_timeout)
    if not rec["viable"] and rec.get("failed_at") is None:
        rec["failed_at"] = "pruning" if scanned != 1 else "budget"
    return rec


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--container", default="p6-pg")
    ap.add_argument("--image", default=os.environ.get("IMG_PG", ""),
                    help="digest-pinned pg image; defaults to $IMG_PG from images.env")
    ap.add_argument("--buckets", default="1,5,50,500",
                    help="axis points to probe (bucket counts)")
    ap.add_argument("--cardinalities", default="2,10,100",
                    help="satellite-field cardinalities (see encargo 5.4)")
    ap.add_argument("--ddl-timeout", type=int, default=DDL_TIMEOUT_S)
    ap.add_argument("--plan-timeout", type=int, default=PLAN_TIMEOUT_S)
    ap.add_argument("--out", default="")
    args = ap.parse_args()
    if "@sha256:" not in args.image:
        raise SystemExit("FATAL: --image must be digest-pinned. Source images.env first "
                         "(see §5.1) — an unpinned rival makes the verdict unreproducible.")

    out = []
    for nb in [int(x) for x in args.buckets.split(",")]:
        for card in [int(x) for x in args.cardinalities.split(",")]:
            r = probe(args.container, nb, card, args)
            out.append(r)
            print(json.dumps(r), flush=True)
            if not r["viable"]:
                # A bigger table at the same bucket count will not become viable.
                break
    if args.out:
        with open(args.out, "a") as f:
            for r in out:
                f.write(json.dumps(r) + "\n")


if __name__ == "__main__":
    main()
