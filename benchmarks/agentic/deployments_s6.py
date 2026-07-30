#!/usr/bin/env python3
"""S6 — one engine for the whole agent (design §1 S6). Cells are DEPLOYMENTS, not
binaries. Same business turn in all: write a memory (vector + topic/status/
importance in the SAME logical record) -> update a structured field -> NEAREST ->
AGGREGATE ("count memories of topic X with status active + avg(importance)").

Deployments:
  xyz        — one engine, ghosts ON (declared Δ vs vector-pure S1/S5); NEAREST +
               AGGREGATE native over the same records.
  pg         — one SYSTEM (pgvector): vector + columns + UPDATE + SQL AGGREGATE.
               The strong rival of this axis — we concede it ties on one-system.
  qdrant+pg  — real deployment: vector store + Postgres for structure. The stack
  chroma+pg    tax is measured: double write, inconsistency window (the gap where
               the two stores disagree), two connections, summed footprint.

Each deployment: setup(vecs, bids, meta) -> turn(...) -> aggregate(topic) -> close.
Reuses adapters.py for the vector side; a tiny PgStore for the structured side.
"""
import time
import numpy as np
from adapters import XyzdbAdapter, PgvectorAdapter, QdrantAdapter, ChromaAdapter
from measure_x import settle as _settle


# ── structured store used by the +store deployments ──────────────────────────
class PgStore:
    """Minimal Postgres structured store (the 'store' half of qdrant+store /
    chroma+store). Holds topic/status/importance keyed by turn id; serves the
    UPDATE and the AGGREGATE that the vector store cannot."""
    def __init__(self, host="127.0.0.1", port=5433):
        import psycopg2
        self.conn = psycopg2.connect(host=host, port=port, user="postgres",
                                     password="bench", dbname="postgres")
        self.conn.autocommit = True   # durable: synchronous_commit=on is the pg default

    def setup(self, bids, meta):
        cur = self.conn.cursor()
        cur.execute("DROP TABLE IF EXISTS mem_struct")
        cur.execute("CREATE TABLE mem_struct (gid int primary key, topic int, status text, importance float8)")
        from psycopg2.extras import execute_values
        rows = [(int(i), int(meta["topic"][i]), str(meta["status"][i]), float(meta["importance"][i]))
                for i in range(len(meta["topic"]))]
        for s in range(0, len(rows), 2000):
            execute_values(cur, "INSERT INTO mem_struct (gid,topic,status,importance) VALUES %s",
                           rows[s:s + 2000])
        cur.execute("CREATE INDEX ON mem_struct (topic, status)")

    def insert(self, gid, topic, status, importance):
        self.conn.cursor().execute(
            "INSERT INTO mem_struct (gid,topic,status,importance) VALUES (%s,%s,%s,%s) "
            "ON CONFLICT (gid) DO UPDATE SET topic=EXCLUDED.topic, status=EXCLUDED.status, "
            "importance=EXCLUDED.importance", (int(gid), int(topic), str(status), float(importance)))

    def update_status(self, gid, status):
        self.conn.cursor().execute("UPDATE mem_struct SET status=%s WHERE gid=%s", (str(status), int(gid)))

    def aggregate(self, topic):
        cur = self.conn.cursor()
        cur.execute("SELECT count(*), COALESCE(avg(importance),0) FROM mem_struct "
                    "WHERE topic=%s AND status='active'", (int(topic),))
        n, avg = cur.fetchone()
        return int(n), float(avg)

    def close(self):
        try: self.conn.close()
        except Exception: pass


# ── deployments ──────────────────────────────────────────────────────────────
class XyzS6:
    """One engine. Ghosts ON (Δ vs S1/S5 vector-pure — declared)."""
    label, one_system = "xyz", True

    def __init__(self, host="127.0.0.1", port=2505, dim=1024, hnsw=None):
        self.a = XyzdbAdapter(host=host, port=port, dim=dim)
        self.db = self.a.db
        self.lobe = self.a.lobe
        self.host, self.port = host, port

    def settle(self, container):
        """Post-setup settle (change 3): restart+reconnect so the base index and the
        ghost read from settled state; the ghost persists across the restart."""
        ms = _settle(container, "xyzdb", self.host, self.port, self.a)
        self.db = self.a.db
        return ms

    def setup(self, vecs, bids, meta):
        self.a.load(vecs, bids, meta=meta)
        # Ghosts ON (the S6 Δ vs vector-pure): an auto-maintained count/avg-by-topic
        # rollup so the AGGREGATE routes to a precomputed sub-ms read (native Q2 shape).
        # Must be the CLASSIC clause form (ORDER BY g GROUP BY g AGGREGATE ...): that
        # builds an aggregate point-lookup ghost. The pipeline `| ... | TAKE BY <metric>`
        # form builds a METRIC-ORDER ghost instead (serves TAKE n BY metric), which the
        # GROUP BY|AGGREGATE query does NOT route to -> it falls to a runtime scan.
        # Verified at 30k: classic form 0.24ms point-lookup vs 80ms scan; avg is a
        # supported ghost aggregate and the rollup survives the settle-restart.
        self.db.execute(
            f'CREATE GHOST "s6_by_topic" FROM "{self.lobe}" WHERE status = "active" '
            'ORDER BY topic GROUP BY topic AGGREGATE count(), avg(importance)')

    def turn(self, gid, bucket, topic, status, importance, qvec, k, up_status):
        """write -> update -> NEAREST; return {write_ms, update_ms, near_ms, incons_ms}."""
        t = time.perf_counter()
        self.db.put_batch(self.lobe, [{"*bucket": str(bucket), "id": f"g{gid}", "emb": qvec.tolist(),
                                       "topic": int(topic), "status": str(status),
                                       "importance": float(importance)}])
        w = (time.perf_counter() - t) * 1e3
        t = time.perf_counter()
        self.db.execute(f'SCAN "{self.lobe}" WHERE bucket = $b AND id = $i | SET status = $s',
                        {"b": str(bucket), "i": f"g{gid}", "s": str(up_status)})
        u = (time.perf_counter() - t) * 1e3
        t = time.perf_counter()
        self.a.query(qvec, bucket, k)
        n = (time.perf_counter() - t) * 1e3
        return {"write_ms": w, "update_ms": u, "near_ms": n, "incons_ms": 0.0}  # one engine: atomic

    def aggregate(self, topic):
        # GROUP BY topic + topic=$t (Eq-on-group-key) routes to the s6_by_topic ghost
        # → precomputed sub-ms read (vs a runtime scan). Matches the ghost's signature
        # (status="active" filter, count()/avg(importance)).
        return self.db.execute(
            f'SCAN "{self.lobe}" WHERE status = "active" AND topic = $t '
            '| GROUP BY topic | AGGREGATE count(), avg(importance)', {"t": int(topic)})

    def close(self):
        self.a.close()


class PgS6:
    """One SYSTEM: pgvector holds vector + columns; UPDATE + SQL AGGREGATE. The
    strong rival of this axis (concede it ties on one-system)."""
    label, one_system = "pg", True

    def __init__(self, host="127.0.0.1", port=5432, dim=1024, hnsw=None):
        self.a = PgvectorAdapter(host=host, port=port, dim=dim)
        self.conn = self.a.conn
        self.host, self.port = host, port

    def settle(self, container):
        ms = _settle(container, "pgvector", self.host, self.port, self.a)
        self.conn = self.a.conn
        return ms

    def setup(self, vecs, bids, meta):
        self.a.load(vecs, bids, {"m": 16, "efc": 64, "efs": 100}, meta=meta)
        # One system: structure lives in the same `items` table. Index (topic,status)
        # for the S6 AGGREGATE. (mem_struct is the EXTERNAL store, not used here.)
        self.conn.cursor().execute("CREATE INDEX IF NOT EXISTS s6_ts ON items (topic, status)")

    def turn(self, gid, bucket, topic, status, importance, qvec, k, up_status):
        cur = self.conn.cursor()
        t = time.perf_counter()
        cur.execute("INSERT INTO items (gid,bucket,topic,status,importance,emb) "
                    "VALUES (%s,%s,%s,%s,%s,%s) ON CONFLICT DO NOTHING",
                    (int(gid), int(bucket), int(topic), str(status), float(importance), qvec))
        w = (time.perf_counter() - t) * 1e3
        t = time.perf_counter()
        cur.execute("UPDATE items SET status=%s WHERE gid=%s", (str(up_status), int(gid)))
        u = (time.perf_counter() - t) * 1e3
        t = time.perf_counter()
        self.a.query(qvec, bucket, k)
        n = (time.perf_counter() - t) * 1e3
        return {"write_ms": w, "update_ms": u, "near_ms": n, "incons_ms": 0.0}  # one system: 1 txn

    def aggregate(self, topic):
        cur = self.conn.cursor()
        cur.execute("SELECT count(*), COALESCE(avg(importance),0) FROM items "
                    "WHERE topic=%s AND status='active'", (int(topic),))
        return cur.fetchone()

    def close(self):
        self.a.close()


class _VecPlusStore:
    """qdrant+store / chroma+store — the real two-system deployment. Vector in the
    specialist, structure in Postgres. Measures the stack tax: double write and the
    inconsistency window (vector written, structure not yet — the gap a reader sees
    the two disagree)."""
    one_system = False

    def __init__(self, vec_adapter, store, dim, vec_engine="", host="127.0.0.1", vport=0):
        self.vec = vec_adapter
        self.store = store
        self._vengine, self.host, self.vport = vec_engine, host, vport

    def settle(self, container):
        """Settle the VECTOR container only; the +store (Postgres) is already durable
        and is not restarted (a two-system deployment settles its specialist half)."""
        return _settle(container, self._vengine, self.host, self.vport, self.vec)

    def setup(self, vecs, bids, meta):
        self.vec.load(vecs, bids, {"m": 16, "efc": 100, "ef": 128, "cef": 100, "sef": 128}, meta=meta)
        self.store.setup(bids, meta)

    def turn(self, gid, bucket, topic, status, importance, qvec, k, up_status):
        # Write 1: vector store. Between it and write 2 the two systems DISAGREE.
        t = time.perf_counter()
        self._vec_insert(gid, bucket, topic, status, importance, qvec)
        after_vec = time.perf_counter()
        # Write 2: structured store.
        self.store.insert(gid, topic, status, importance)
        after_store = time.perf_counter()
        w = (after_store - t) * 1e3
        incons = (after_store - after_vec) * 1e3   # the window the store lagged the vector
        t = time.perf_counter()
        self.store.update_status(gid, up_status)   # structured update -> the store
        u = (time.perf_counter() - t) * 1e3
        t = time.perf_counter()
        self.vec.query(qvec, bucket, k)            # NEAREST -> the vector store
        n = (time.perf_counter() - t) * 1e3
        return {"write_ms": w, "update_ms": u, "near_ms": n, "incons_ms": incons}

    def aggregate(self, topic):
        return self.store.aggregate(topic)         # AGGREGATE -> the structured store

    def close(self):
        self.vec.close(); self.store.close()


class QdrantPgS6(_VecPlusStore):
    label = "qdrant+pg"

    def __init__(self, host="127.0.0.1", port=6333, dim=1024, hnsw=None, store_port=5433):
        super().__init__(QdrantAdapter(host=host, port=port, dim=dim), PgStore(host=host, port=store_port),
                         dim, vec_engine="qdrant", host=host, vport=port)

    def _vec_insert(self, gid, bucket, topic, status, importance, qvec):
        from qdrant_client import models
        self.vec.client.upsert(collection_name=self.vec.coll, wait=True, points=[
            models.PointStruct(id=int(gid), vector=qvec.tolist(),
                               payload={"bucket": str(int(bucket))})])


class ChromaPgS6(_VecPlusStore):
    label = "chroma+pg"

    def __init__(self, host="127.0.0.1", port=8000, dim=1024, hnsw=None, store_port=5433):
        super().__init__(ChromaAdapter(host=host, port=port, dim=dim), PgStore(host=host, port=store_port),
                         dim, vec_engine="chroma", host=host, vport=port)

    def _vec_insert(self, gid, bucket, topic, status, importance, qvec):
        self.vec._flat.add(ids=[str(int(gid))], embeddings=[qvec.tolist()],
                           metadatas=[{"bucket": int(bucket)}])


DEPLOYMENTS = {"xyz": XyzS6, "pg": PgS6, "qdrant+pg": QdrantPgS6, "chroma+pg": ChromaPgS6}
