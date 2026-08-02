#!/usr/bin/env python3
"""S3 — multi-agent fleet with a lifecycle (design §1 S3).

N agents = N tenants (buckets); the fleet is born, grows, and dies (PURGE). Same
business op in the 4 engines, each with its best tenant form:
  xyzdb    — tenant = a gravity value (create: PUT; destroy: DELETE WHERE bucket).
  pgvector — partition + inherited HNSW (create: CREATE PARTITION; destroy: DROP).
  qdrant   — native multitenancy, is_tenant payload (create: upsert; destroy:
             delete-by-filter).
  chroma   — collection = one HNSW in RAM (create: create_collection; destroy:
             delete_collection).

Metrics: cost to create / destroy the Nth tenant (not the first), RAM-total vs
#tenants (the CURVE, sampled at each step), recall of a filtered query at each step.
Steps 10/100/1000 — if chroma cannot hold 1000 in the tier, that OOM IS the result.
Mac/OrbStack = DIRECTION.
"""
import argparse
import json
import time

import numpy as np
from adapters import XyzdbAdapter, PgvectorAdapter, QdrantAdapter, ChromaAdapter
from measure_x import docker_mem_mb, PORTS, hnsw_from_env, bench_stamp
from measure_lme import load_corpus_a
import recall_harness as rh

PER_TENANT = 200   # vectors per tenant (agent memory size); small so the curve is about count


def make(engine, host, dim, hnsw):
    if engine == "xyzdb":
        a = XyzdbAdapter(host=host, port=PORTS[engine], dim=dim)
        a.db.execute(f'LOBE "{a.lobe}" HINT="fleet"'); a.db.execute(f'VECTOR emb IN "{a.lobe}"'); a.db.execute(f'GRAVITY BY bucket IN "{a.lobe}"')
        return a
    if engine == "pgvector":
        a = PgvectorAdapter(host=host, port=PORTS[engine], dim=dim)
        cur = a.conn.cursor()
        cur.execute("DROP TABLE IF EXISTS items")
        cur.execute(f"CREATE TABLE items (gid int, bucket int, emb vector({dim})) PARTITION BY LIST (bucket)")
        m, efc = hnsw.get("m", 16), hnsw.get("efc", 64)
        cur.execute(f"CREATE INDEX ON items USING hnsw (emb vector_cosine_ops) WITH (m={m}, ef_construction={efc})")
        a._efs = hnsw.get("efs", 100)
        return a
    if engine == "qdrant":
        from qdrant_client import models
        a = QdrantAdapter(host=host, port=PORTS[engine], dim=dim)
        m, efc = hnsw.get("m", 16), hnsw.get("efc", 100)
        a.client.recreate_collection(collection_name=a.coll,
            vectors_config=models.VectorParams(size=dim, distance=models.Distance.COSINE),
            hnsw_config=models.HnswConfigDiff(m=m, ef_construct=efc))
        a.client.create_payload_index(collection_name=a.coll, field_name="bucket",
            field_schema=models.KeywordIndexParams(type=models.KeywordIndexType.KEYWORD, is_tenant=True))
        a._ef = hnsw.get("ef", 128)
        return a
    a = ChromaAdapter(host=host, port=PORTS[engine], dim=dim); a._cfg = a._config(hnsw); return a


def create_tenant(engine, a, tid, vecs):
    if engine == "xyzdb":
        for s in range(0, len(vecs), 600):
            a.db.put_batch(a.lobe, [{"*bucket": str(tid), "id": f"g{tid}_{i}", "emb": vecs[i].tolist()}
                                    for i in range(s, min(s + 600, len(vecs)))])
    elif engine == "pgvector":
        from psycopg2.extras import execute_values
        cur = a.conn.cursor()
        cur.execute(f"CREATE TABLE items_{tid} PARTITION OF items FOR VALUES IN ({tid})")
        execute_values(cur, "INSERT INTO items (gid,bucket,emb) VALUES %s",
                       [(i, tid, vecs[i]) for i in range(len(vecs))], template="(%s,%s,%s)")
    elif engine == "qdrant":
        from qdrant_client import models
        a.client.upsert(collection_name=a.coll, wait=True, points=[
            models.PointStruct(id=tid * 100000 + i, vector=vecs[i].tolist(),
                               payload={"bucket": str(tid)}) for i in range(len(vecs))])
    else:
        c = a.client.create_collection(f"mem_{tid}", configuration=a._cfg)
        a._colls[tid] = c
        c.add(ids=[str(i) for i in range(len(vecs))], embeddings=[vecs[i].tolist() for i in range(len(vecs))])


def destroy_tenant(engine, a, tid):
    if engine == "xyzdb":
        a.db.execute(f'DELETE "{a.lobe}" WHERE bucket = "{tid}"')
    elif engine == "pgvector":
        a.conn.cursor().execute(f"DROP TABLE IF EXISTS items_{tid}")
    elif engine == "qdrant":
        from qdrant_client import models
        a.client.delete(collection_name=a.coll, points_selector=models.FilterSelector(
            filter=models.Filter(must=[models.FieldCondition(key="bucket", match=models.MatchValue(value=str(tid)))])))
    else:
        a.client.delete_collection(f"mem_{tid}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--engine", required=True, choices=["xyzdb", "pgvector", "qdrant", "chroma"])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--container", required=True)
    ap.add_argument("--envelope", default="?")
    ap.add_argument("--steps", default="10,100,1000")
    ap.add_argument("--round", type=int, default=1)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    qvec, queries, qid2int, turn_vecs, turn_bucket, turn_session = load_corpus_a()
    dim = turn_vecs.shape[1]
    pool = turn_vecs[:PER_TENANT * 4]     # a small vector pool cycled across tenants
    steps = sorted(int(x) for x in args.steps.split(","))
    hnsw = hnsw_from_env(args.engine)

    try:
        a = make(args.engine, args.host, dim, hnsw)
    except Exception as e:
        open(args.out, "a").write(json.dumps({"kind": "s3", "engine": args.engine,
            "envelope": args.envelope, "status": "setup_failed", "err": str(e)[:120]}) + "\n"); return

    ram0 = docker_mem_mb(args.container)
    made = 0
    for target in steps:
        create_ms_last = None
        try:
            while made < target:
                tid = made
                vs = pool[(tid % 4) * PER_TENANT:(tid % 4) * PER_TENANT + PER_TENANT]
                t = time.perf_counter()
                create_tenant(args.engine, a, tid, vs)
                create_ms_last = (time.perf_counter() - t) * 1e3   # cost of the Nth create
                made += 1
        except Exception as e:
            open(args.out, "a").write(json.dumps({"kind": "s3", "engine": args.engine,
                "envelope": args.envelope, "step": target, "made": made,
                "status": "crash_or_oom_during_grow", "ram_mb": docker_mem_mb(args.container),
                "err": str(e)[:120]}) + "\n")
            print(f"  OOM/crash growing to {target} at tenant {made}")
            break
        ram = sorted(docker_mem_mb(args.container) for _ in range(3))[1]
        # destroy the Nth tenant (cost of tearing one down), then recreate to keep the count
        t = time.perf_counter(); destroy_tenant(args.engine, a, made - 1); destroy_ms = (time.perf_counter() - t) * 1e3
        vs = pool[((made - 1) % 4) * PER_TENANT:((made - 1) % 4) * PER_TENANT + PER_TENANT]
        create_tenant(args.engine, a, made - 1, vs)
        open(args.out, "a").write(json.dumps({
            "kind": "s3", "engine": args.engine, "envelope": args.envelope, "round": args.round,
            "step": target, "tenants": made, "per_tenant": PER_TENANT,
            "ram_mb": round(ram, 1), "ram_over_base_mb": round(ram - ram0, 1),
            "create_nth_ms": round(create_ms_last, 3) if create_ms_last else None,
            "destroy_nth_ms": round(destroy_ms, 3), "status": None, "stamp": bench_stamp(adapter)}) + "\n")
        print(f"  {args.engine} {made} tenants: ram={ram:.0f}MiB create_nth={create_ms_last:.1f}ms")
    a.close()


if __name__ == "__main__":
    main()
