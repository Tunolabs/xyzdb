"""Engine adapters — each in its BEST-FORM config (doc 04 amendment 2026-07-06).

f32 lossless on ALL (no quantization anywhere — main run). Rivals run in TWO points:
  - scoped=True  → per-bucket structure (the analog to xyzDB gravity co-location):
                   pgvector PARTITION BY bucket · qdrant is_tenant payload index ·
                   chroma collection-per-bucket.
  - scoped=False → one flat index + metadata filter (reveals the scoping cost xyzDB
                   doesn't pay).
xyzDB has no scoped/flat dial — declared gravity + `*g` IS its native form.
HNSW params come from BENCH_* env; ef/hnsw_ef/search_ef are tuned on dev (max recall @
p99≤50ms) and frozen for held-out.
"""
import os
from typing import List

# Where the engines answer. 127.0.0.1 when a runner is invoked on the host;
# `host.docker.internal` when the harness itself runs in `Dockerfile.bench`, which
# is how the rival CLIENTS get pinned alongside their servers instead of inheriting
# whatever Python the host has. One env var rather than a parameter threaded
# through every call site — the address is a property of where the harness runs,
# not of any single measurement.
DEFAULT_ENGINE_HOST = os.environ.get("BENCH_ENGINE_HOST", "127.0.0.1")

# Added to every engine's standard port. The containers always listen on their own
# ports; only the published host port moves, because a development machine may
# already be using one (this one runs a project postgres on 5432). Read from the
# same env var `lib_docker.sh` publishes with, so shell and Python cannot disagree.
PORT_OFFSET = int(os.environ.get("BENCH_PORT_OFFSET", "0"))
import numpy as np

BATCH = 2000
# xyzDB PUT BATCH size (records per PUT BATCH statement). The minimal client serialises
# each record as xyTalk text, so a 1024-d f32 vector is ~19 KB of text; 600 records
# ≈ 11.5 MB, under the engine's 16 MiB frame cap (the ceiling for this dim is ~1000).
# This equals what the fluent SDK sent per put_batch call — batch size is a measurement
# parameter, so it is fixed here on purpose, not left to the client.
XYZ_PUT_BATCH = 600
# in-memory HNSW build budget (pg default 64MB → on-disk build, 30min+ measured). Scaled to the
# envelope in coverage runs via BENCH_PG_MWM, so a tight envelope gets an HONEST build budget
# instead of a fixed 2GB that OOMs trivially. Default 2GB = the tuned best-form for roomy envelopes.
PG_MAINT_WORK_MEM = os.environ.get("BENCH_PG_MWM", "2GB")


# The last partial reported by `_xyz_ids_from_json`, for the caller that wants it.
# A module-level slot rather than a changed return type: every existing call site
# reads a list of ids, and widening that signature would touch scenarios this
# change has no business editing.
LAST_PARTIAL: dict = {}


def _xyz_ids_from_json(resp: dict) -> List[int]:
    """Extract integer turn ids (`g<i>` -> i) from a raw xyzDB JSON response.

    ALSO records whether the answer was a partial. This function used to read
    `records` and nothing else, so a `NEAREST` cut short by the latency airbag —
    which says so, in `budget_stop`, and marks the frame `has_more` — arrived here
    looking exactly like a complete answer. The missing rows would then have been
    scored as MISSED RECALL: xyzDB penalised for the one thing it did right, which
    is announcing that it stopped early.

    It is the same defect the three client SDKs were just fixed for, sitting in the
    benchmark's own extractor. A partial belongs in its own column and must never
    be folded into a recall number — those are different facts and only one of them
    is about search quality.
    """
    resp = resp or {}
    LAST_PARTIAL.clear()
    if resp.get("budget_stop") or resp.get("has_more"):
        LAST_PARTIAL.update({"partial": True,
                             "budget_stop": resp.get("budget_stop"),
                             "has_more": bool(resp.get("has_more"))})
    out = []
    for r in resp.get("records", []):
        rid = r.get("id")
        if isinstance(rid, str) and rid.startswith("g") and rid[1:].isdigit():
            out.append(int(rid[1:]))
    return out


def xyz_last_partial() -> dict:
    """The partial report from the most recent `_xyz_ids_from_json`, or `{}`.

    Empty means the last answer was complete — not that nobody looked, which is why
    the caller reads this rather than inferring completeness from a row count.
    """
    return dict(LAST_PARTIAL)


def _xyz_nearest(db, lobe: str, bucket_val: str, qvec, k: int) -> list:
    """Bucket-scoped exact NEAREST via the minimal client; returns raw record dicts.

    The query vector is bound as `$q` (protocol V4 substitution); k is inlined as a
    literal (the parser rejects `$k` inside NEAREST(...)). This is the same statement
    the fluent SDK builder emitted — identical engine work, spelled with execute()."""
    out = db.execute(
        f'SCAN "{lobe}" WHERE bucket = $b | NEAREST(emb, $q, {int(k)}, cosine)',
        {"b": str(bucket_val), "q": qvec.tolist()})
    return out.get("records", []) or []


# ── The structured fields that travel with every record — DECLARED ONCE ──────
#
# These used to be named literally ("topic", "status", "importance") in four
# separate places: the chroma metadata builder, the xyzDB record builder, the pg
# column list and row builder, and the qdrant payload builder. Adding a field meant
# finding all four, and two fields the v2 design needs had been added to the corpus
# without reaching any engine.
#
# `tenant` is the one that blocks whole questions. It is the original question-id —
# the user. At the `user` granularity nobody notices it is missing, because the
# bucket IS the tenant; from `group` outward the bucket is the pool and the tenant
# vanishes from the engine entirely. Without it, "what did THIS user tell me" cannot
# be expressed on the coarse half of the axis, the pooled Q3 has no residual to
# filter on, and the result worth selling — pooling tenants costs nothing if you
# declare the tenant as the satellite axis — has no field to declare.
#
# (name, python caster, pg column type). Order fixes the pg column order.
EXTRA_FIELDS = [
    ("tenant", str, "text"),              # the user — structural, see above
    ("topic", int, "int"),                # S5b range sweep
    ("status", str, "text"),              # Q4 aggregate filter
    ("importance", float, "double precision"),   # Q4 aggregate value
    ("cat2", int, "int"),                 # Q3 equality sweep: selectivity 1/2
    ("cat10", int, "int"),                #                    1/10
    ("cat100", int, "int"),               #                    1/100
    ("cat1000", int, "int"),              #                    1/1000
]


def present_fields(meta):
    """The declared fields actually supplied by this run, in declaration order.

    A scenario passes only what it needs, so the adapters carry the intersection
    rather than demanding every field exist.
    """
    if meta is None:
        return []
    return [(n, c, t) for (n, c, t) in EXTRA_FIELDS if n in meta]


def _chroma_md(i, bids, sids, meta, with_bucket):
    """Per-turn chroma metadata dict (bucket/sid + every declared field present)."""
    d = {}
    if with_bucket:
        d["bucket"] = int(bids[i])
    if sids is not None:
        d["sid"] = str(sids[i])
    for name, cast, _ in present_fields(meta):
        d[name] = cast(meta[name][i])
    return d


class XyzdbAdapter:
    """xyzDB: declared gravity + `*g` (physical co-location), fused exact NEAREST, cache to
    envelope. Ghosts OFF (verified: SHOW GHOSTS empty after NEAREST-per-bucket). No dial."""
    name = "xyzdb"

    def __init__(self, host=DEFAULT_ENGINE_HOST, port=2505 + PORT_OFFSET, dim=1024, scoped=True,
                 satellite=None, lobe="mem", **_):
        import xyzdb_minimal as xyzdb
        self.db = xyzdb.connect(host, port, timeout=300.0)
        # Named so the equivalence gate can hold the same rows twice — once with the
        # axis declared and once without — and compare the two routes over the wire.
        self.lobe = lobe
        # The satellite axis is a CELL PARAMETER, not a property of the adapter.
        # `SATELLITE BY` is refused on a non-empty lobe and cannot be changed
        # afterwards, so each (granularity, axis) pair is its own load: Q3-scoped
        # wants gravity=tenant with axis=catN, Q3-pool wants gravity=pool with the
        # tenant residual, and the multi-tenant result wants gravity=group with
        # axis=TENANT. Hardwiring it here would silently make one of those
        # impossible. Thirteen distinct loads at 246,738 rows each — worth counting
        # before launching rather than discovering mid-matrix.
        self.satellite = satellite

    def load(self, vecs, bids, hnsw=None, sids=None, meta=None):
        # DDL via execute() — the minimal client has no typed create_lobe/create_vector/
        # gravity_by helpers; these statements are their exact equivalent.
        self.db.execute(f'LOBE "{self.lobe}" HINT="agentic"')
        self.db.execute(f'VECTOR emb IN "{self.lobe}"')
        self.db.execute(f'GRAVITY BY bucket IN "{self.lobe}"')
        if self.satellite:
            # Declared BEFORE the first write: the engine refuses it on a non-empty
            # lobe, because existing rows would stay at satellite 0 where a bounded
            # query cannot reach them.
            self.db.execute(f'SATELLITE BY {self.satellite} IN "{self.lobe}"')
        for s in range(0, len(vecs), XYZ_PUT_BATCH):
            recs = []
            for i in range(s, min(s + XYZ_PUT_BATCH, len(vecs))):
                r = {"*bucket": str(bids[i]), "id": f"g{i}", "emb": vecs[i].tolist()}
                if sids is not None:  # S1: session id co-located in the same gravity bucket
                    r["sid"] = str(sids[i])
                for name, cast, _ in present_fields(meta):  # structured fields, SAME record
                    r[name] = cast(meta[name][i])
                recs.append(r)
            self.db.put_batch(self.lobe, recs)
        # Operational cost of scoping (the moat): xyzDB scopes by DECLARING it.
        # The count moves with the satellite so it stays honest — one line without an
        # axis, two with. Two lines against pg's 50,000 statements and 145.7s for the
        # same effect (see the P6 viability result); a stale `1` would be flattering
        # by inertia rather than by measurement.
        if self.satellite:
            self.setup_cost = {
                "kind": "gravity+satellite-declared", "structures": 1, "ddl_lines": 2,
                "note": f"GRAVITY BY bucket + SATELLITE BY {self.satellite}; "
                        "co-location and sub-bucketing are declarations, not structures"}
        else:
            self.setup_cost = {"kind": "gravity-declared", "structures": 1, "ddl_lines": 1,
                               "note": "1 GRAVITY BY declaration; scope co-location is free"}

    def query(self, qvec, bucket, k) -> List[int]:
        recs = _xyz_nearest(self.db, self.lobe, str(bucket), qvec, k)
        return [int(r["id"][1:]) for r in recs if isinstance(r.get("id"), str)]

    def retrieve_expand(self, qvec, bucket, k):
        """S1: NEAREST top-k in the bucket, then expand each hit to its full session.
        TWO co-located statements (no fused NEAREST|PULL — PULL is gravity-bucket-
        scoped = the whole question, not the sid; verified spec §2.13). Both scans hit
        the same warm gravity bucket. Returns (expanded_turn_ids, hit_sids, roundtrips)."""
        b = str(bucket)
        # RT1 — NEAREST over the bucket; the hit records carry their sid (co-located).
        recs = _xyz_nearest(self.db, self.lobe, b, qvec, k)
        hit_sids = list(dict.fromkeys(str(r["sid"]) for r in recs if r.get("sid") is not None))
        if not hit_sids:
            return [], [], 1
        # RT2 — ONE co-located range scan. In v0.9.5 the primary gravity SCAN applies
        # `sid IN [...]` (the coherence wave) and `| SHAPE {id}` projects out the emb
        # payload — so the whole session expansion is a single warm-page scan → 2
        # roundtrips total. (On the pre-v0.9.5 image IN did not filter and there was
        # no SHAPE, which forced a per-session-scan fallback = 4 RT — the stale-image
        # artifact, premise-20.)
        sid_lits = ", ".join(f'"{s}"' for s in hit_sids)
        q = f'SCAN "{self.lobe}" WHERE bucket = "{b}" AND sid IN [{sid_lits}] | SHAPE {{id}}'
        return _xyz_ids_from_json(self.db.execute(q)), hit_sids, 2

    def query_filtered(self, qvec, bucket, k, topic_lt):
        """S5: exact structured WHERE (topic < T) + exact NEAREST, one co-located
        scan. The vector is a bound $q param; k must be a literal (the parser rejects
        $k inside NEAREST(...)/LIMIT — verified)."""
        kk = int(k)
        q = (f'SCAN "{self.lobe}" WHERE bucket = $b AND topic < $t '
             f'| NEAREST(emb, $q, {kk}, cosine) LIMIT {kk}')
        out = self.db.execute(q, {"b": str(bucket), "t": int(topic_lt), "q": qvec.tolist()})
        return _xyz_ids_from_json(out)

    def insert_one(self, gid, bucket, vec):
        """S2: online insert of one memory (durable — server started --durability durable)."""
        self.db.put_batch(self.lobe, [{"*bucket": str(bucket), "id": f"g{gid}", "emb": vec.tolist()}])

    def close(self):
        try: self.db.close()
        except Exception: pass


class PgvectorAdapter:
    name = "pgvector"

    def __init__(self, host=DEFAULT_ENGINE_HOST, port=5432 + PORT_OFFSET, dim=1024, hnsw=None, scoped=False, **_):
        import psycopg2
        from pgvector.psycopg2 import register_vector
        self.dim, self.scoped = dim, scoped
        self.conn = psycopg2.connect(host=host, port=port, user="postgres",
                                     password="bench", dbname="postgres")
        self.conn.autocommit = True
        cur = self.conn.cursor()
        cur.execute("CREATE EXTENSION IF NOT EXISTS vector")
        register_vector(self.conn)

    def load(self, vecs, bids, hnsw, sids=None, meta=None):
        from psycopg2.extras import execute_values
        self._has_sid = sids is not None
        self._has_meta = meta is not None
        cur = self.conn.cursor()
        cur.execute(f"SET maintenance_work_mem = '{PG_MAINT_WORK_MEM}'")
        cur.execute("SET max_parallel_maintenance_workers = 2")
        # A too-slow on-disk HNSW build (tight envelope) must abort server-side — signal.alarm can't
        # interrupt a blocking libpq CREATE INDEX. statement_timeout cancels it → we raise TimeoutError.
        _build_to = int(os.environ.get("BUILD_TIMEOUT", 600))
        cur.execute(f"SET statement_timeout = '{_build_to * 1000}'")
        cur.execute("DROP TABLE IF EXISTS items")
        m, efc = hnsw.get("m", 16), hnsw.get("efc", 64)
        buckets = sorted(set(int(x) for x in bids))
        # S1 sid column (JOIN) + S5/S6 structured columns, all in the same row.
        extra = ""
        if self._has_sid:
            extra += ", sid text"
        for name, _, sqltype in present_fields(meta):
            extra += f", {name} {sqltype}"
        if self.scoped:
            # one partition per bucket → WHERE bucket=X prunes to a small per-bucket HNSW.
            cur.execute(f"CREATE TABLE items (gid int, bucket int{extra}, emb vector({self.dim})) PARTITION BY LIST (bucket)")
            for b in buckets:
                cur.execute(f"CREATE TABLE items_{b} PARTITION OF items FOR VALUES IN ({b})")
            # Operational cost of scoping (the moat): rival must build N partitions + DDL.
            self.setup_cost = {"kind": "partitions", "structures": len(buckets),
                               "ddl_lines": len(buckets) + 2,
                               "note": f"{len(buckets)} CREATE TABLE PARTITION + parent + HNSW"}
        else:
            cur.execute(f"CREATE TABLE items (gid int, bucket int{extra}, emb vector({self.dim}))")
            self.setup_cost = {"kind": "flat-hnsw+btree", "structures": 1, "ddl_lines": 3,
                               "note": "1 table + global HNSW + btree(bucket) + iterative_scan"}
        cols_l = ["gid", "bucket"]
        if self._has_sid:
            cols_l.append("sid")
        cols_l += [n for n, _, _ in present_fields(meta)]
        cols_l.append("emb")
        cols = ",".join(cols_l)
        tmpl = "(" + ",".join(["%s"] * len(cols_l)) + ")"
        rows = []
        for i in range(len(vecs)):
            row = [int(i), int(bids[i])]
            if self._has_sid:
                row.append(str(sids[i]))
            row += [cast(meta[name][i]) for name, cast, _ in present_fields(meta)]
            row.append(vecs[i])
            rows.append(tuple(row))
        for s in range(0, len(rows), BATCH):
            execute_values(cur, f"INSERT INTO items ({cols}) VALUES %s", rows[s:s + BATCH], template=tmpl)
        if not self.scoped:
            cur.execute("CREATE INDEX ON items (bucket)")
        if self._has_sid:
            # Composite (bucket,sid) → the S1 session-expansion JOIN is an index scan, not seq.
            cur.execute("CREATE INDEX ON items (bucket, sid)")
        if self._has_meta:
            # (bucket,topic) btree → S5 exact filter is an index scan; iterative_scan
            # (query time) keeps pulling HNSW candidates until k pass the filter.
            cur.execute("CREATE INDEX ON items (bucket, topic)")
        # CREATE INDEX on a partitioned parent builds one HNSW per partition.
        try:
            cur.execute(f"CREATE INDEX ON items USING hnsw (emb vector_cosine_ops) WITH (m={m}, ef_construction={efc})")
        except Exception as e:
            if "statement timeout" in str(e).lower():
                raise TimeoutError(f"pg HNSW build > {_build_to}s (statement_timeout)") from e
            raise
        cur.execute("ANALYZE items")
        self._efs = hnsw.get("efs", 100)

    def query(self, qvec, bucket, k) -> List[int]:
        cur = self.conn.cursor()
        cur.execute(f"SET hnsw.ef_search = {self._efs}")
        if not self.scoped:
            # flat = global HNSW + post-filter. Without iterative_scan the filter can leave
            # < k rows (recall sunk by config, not thesis); relaxed_order + a high scan cap
            # let it keep pulling candidates until k in-bucket are found (that IS the flat cost).
            cur.execute("SET hnsw.iterative_scan = relaxed_order")
            cur.execute("SET hnsw.max_scan_tuples = 1000000")
        cur.execute("SELECT gid FROM items WHERE bucket=%s ORDER BY emb <=> %s LIMIT %s",
                    (int(bucket), qvec, k))
        return [r[0] for r in cur.fetchall()]

    def retrieve_expand(self, qvec, bucket, k):
        """S1: NEAREST top-k then expand to full sessions in ONE server-side query
        (pgvector's legitimate arm — the relational JOIN). Composite index
        (bucket,sid) makes the expand an index scan. Expansion is bucket-scoped
        (a sid can recur across buckets — corpus A INV-3). 1 roundtrip.
        Returns (expanded_gids, hit_sids, roundtrips)."""
        cur = self.conn.cursor()
        cur.execute(f"SET hnsw.ef_search = {self._efs}")
        if not self.scoped:
            cur.execute("SET hnsw.iterative_scan = relaxed_order")
            cur.execute("SET hnsw.max_scan_tuples = 1000000")
        cur.execute(
            "WITH hits AS ("
            "  SELECT DISTINCT sid FROM ("
            "    SELECT sid FROM items WHERE bucket=%s ORDER BY emb <=> %s LIMIT %s) h)"
            " SELECT gid, sid FROM items WHERE bucket=%s AND sid IN (SELECT sid FROM hits)",
            (int(bucket), qvec, k, int(bucket)))
        rows = cur.fetchall()
        return [r[0] for r in rows], list(dict.fromkeys(r[1] for r in rows)), 1

    def query_filtered(self, qvec, bucket, k, topic_lt):
        """S5: exact filter (topic < T) + NEAREST. Flat uses iterative_scan (keeps
        pulling HNSW candidates until k pass the filter — pgvector 0.8+); scoped
        prunes via the partition first. (bucket,topic) btree backs the filter."""
        cur = self.conn.cursor()
        cur.execute(f"SET hnsw.ef_search = {self._efs}")
        if not self.scoped:
            cur.execute("SET hnsw.iterative_scan = relaxed_order")
            cur.execute("SET hnsw.max_scan_tuples = 1000000")
        cur.execute("SELECT gid FROM items WHERE bucket=%s AND topic < %s ORDER BY emb <=> %s LIMIT %s",
                    (int(bucket), int(topic_lt), qvec, k))
        return [r[0] for r in cur.fetchall()]

    def insert_one(self, gid, bucket, vec):
        """S2: online insert of one memory (durable — autocommit + synchronous_commit=on)."""
        self.conn.cursor().execute(
            "INSERT INTO items (gid,bucket,emb) VALUES (%s,%s,%s) ON CONFLICT DO NOTHING",
            (int(gid), int(bucket), vec))

    def close(self):
        try: self.conn.close()
        except Exception: pass


class QdrantAdapter:
    name = "qdrant"

    def __init__(self, host=DEFAULT_ENGINE_HOST, port=6333 + PORT_OFFSET, dim=1024, hnsw=None, scoped=False,
                 s1_variant="scroll", **_):
        from qdrant_client import QdrantClient
        self.dim, self.scoped = dim, scoped
        # S1 has no server-side JOIN → two legitimate arms (one motor, two points):
        #   "scroll"      — NEAREST then a 2nd filtered scroll by sid (2 RT, pays a roundtrip)
        #   "payload-dup" — each turn carries its session's turn-ids in payload (1 RT, pays disk)
        self.s1_variant = s1_variant
        self.client = QdrantClient(host=host, port=port, timeout=600)
        self.coll = "mem"

    def load(self, vecs, bids, hnsw, sids=None, meta=None):
        from qdrant_client import models
        self._has_sid = sids is not None
        self._has_meta = meta is not None
        m, efc = hnsw.get("m", 16), hnsw.get("efc", 100)
        self.client.recreate_collection(
            collection_name=self.coll,
            vectors_config=models.VectorParams(size=self.dim, distance=models.Distance.COSINE),
            hnsw_config=models.HnswConfigDiff(m=m, ef_construct=efc),
        )
        # scoped: is_tenant keyword index co-locates points per bucket (multi-tenant) →
        # efficient filtered search. flat: a plain keyword index.
        self.client.create_payload_index(
            collection_name=self.coll, field_name="bucket",
            field_schema=models.KeywordIndexParams(type=models.KeywordIndexType.KEYWORD,
                                                    is_tenant=bool(self.scoped)),
        )
        # S1 payloads. payload-dup precomputes each session's membership (bucket,sid) -> [turn ids]
        # and stamps it on every turn (the disk tax that buys a 1-RT expand).
        by_bs = {}
        if self._has_sid:
            for i in range(len(vecs)):
                by_bs.setdefault((str(int(bids[i])), str(sids[i])), []).append(int(i))
            self.client.create_payload_index(
                collection_name=self.coll, field_name="sid",
                field_schema=models.KeywordIndexParams(type=models.KeywordIndexType.KEYWORD))
        if self._has_meta:
            # A payload index for EVERY declared field this run supplies, derived
            # from the declaration instead of a hand-written list.
            #
            # The hand-written version indexed `bucket`, `sid` and `topic`; the
            # `catN` fields were added later for the Q3 selectivity sweep and never
            # reached it. The route control caught it — `field_is_indexed: False` on
            # all four — and it is a handicap WE introduced, not one qdrant has: its
            # filterable-HNSW needs the payload index to apply a filter inside the
            # graph traversal. Measuring latency against an engine we quietly denied
            # its own index is the mirror image of the strawman this benchmark
            # refuses to build for itself.
            #
            # Deriving it from EXTRA_FIELDS means the next field added to the corpus
            # cannot repeat this: a field the harness filters on is a field qdrant
            # gets an index for, by construction.
            _QD_SCHEMA = {int: models.PayloadSchemaType.INTEGER,
                          float: models.PayloadSchemaType.FLOAT,
                          str: models.PayloadSchemaType.KEYWORD}
            for name, cast, _ in present_fields(meta):
                self.client.create_payload_index(
                    collection_name=self.coll, field_name=name,
                    field_schema=_QD_SCHEMA[cast])
        qb = 250   # qdrant caps the HTTP payload at ~32MB; 250×1024d ≈ 5MB, safe
        for s in range(0, len(vecs), qb):
            pts = []
            for i in range(s, min(s + qb, len(vecs))):
                pl = {"bucket": str(int(bids[i]))}
                if self._has_sid:
                    pl["sid"] = str(sids[i])
                    if self.s1_variant == "payload-dup":
                        pl["sess"] = by_bs[(str(int(bids[i])), str(sids[i]))]
                for name, cast, _ in present_fields(meta):
                    pl[name] = cast(meta[name][i])
                pts.append(models.PointStruct(id=int(i), vector=vecs[i].tolist(), payload=pl))
            self.client.upsert(collection_name=self.coll, points=pts, wait=True)
        self._ef = hnsw.get("ef", 128)
        self.setup_cost = ({"kind": "tenant-index", "structures": 1, "ddl_lines": 2,
                            "note": "collection + is_tenant payload index (multi-tenant co-location)"}
                           if self.scoped else
                           {"kind": "keyword-index", "structures": 1, "ddl_lines": 2,
                            "note": "collection + keyword payload index"})

    def query(self, qvec, bucket, k) -> List[int]:
        from qdrant_client import models
        res = self.client.query_points(
            collection_name=self.coll, query=qvec.tolist(),
            query_filter=models.Filter(must=[models.FieldCondition(
                key="bucket", match=models.MatchValue(value=str(int(bucket))))]),
            limit=k, search_params=models.SearchParams(hnsw_ef=self._ef),
        )
        return [int(p.id) for p in res.points]

    def retrieve_expand(self, qvec, bucket, k):
        """S1: NEAREST top-k then expand to full sessions. Two labelled arms
        (self.s1_variant): payload-dup (1 RT, session ids ride the payload — pays
        disk) / scroll (2 RT, a 2nd filtered read by sid — pays a roundtrip).
        Returns (expanded_turn_ids, hit_sids, roundtrips)."""
        from qdrant_client import models
        b = str(int(bucket))
        res = self.client.query_points(
            collection_name=self.coll, query=qvec.tolist(),
            query_filter=models.Filter(must=[models.FieldCondition(
                key="bucket", match=models.MatchValue(value=b))]),
            limit=k, with_payload=True,
            search_params=models.SearchParams(hnsw_ef=self._ef))
        hits = res.points
        hit_sids = list(dict.fromkeys(str(p.payload.get("sid")) for p in hits if p.payload))
        if self.s1_variant == "payload-dup":
            ids = []
            for p in hits:
                ids.extend(int(x) for x in (p.payload or {}).get("sess", []))
            return list(dict.fromkeys(ids)), hit_sids, 1
        if not hit_sids:
            return [], [], 1
        # scroll: 2nd filtered read — bucket AND sid IN hit_sids (MatchAny = IN).
        out, offset = [], None
        flt = models.Filter(must=[
            models.FieldCondition(key="bucket", match=models.MatchValue(value=b)),
            models.FieldCondition(key="sid", match=models.MatchAny(any=hit_sids))])
        while True:
            pts, offset = self.client.scroll(collection_name=self.coll, scroll_filter=flt,
                                             limit=1000, with_payload=False, with_vectors=False,
                                             offset=offset)
            out.extend(int(p.id) for p in pts)
            if offset is None:
                break
        return out, hit_sids, 2

    def query_filtered(self, qvec, bucket, k, topic_lt):
        """S5: filterable-HNSW — the range filter (topic < T) is applied DURING the
        graph traversal via the integer payload index (qdrant's strong S5 arm), not
        a post-filter. Returns top-k turn ids."""
        from qdrant_client import models
        res = self.client.query_points(
            collection_name=self.coll, query=qvec.tolist(),
            query_filter=models.Filter(must=[
                models.FieldCondition(key="bucket", match=models.MatchValue(value=str(int(bucket)))),
                models.FieldCondition(key="topic", range=models.Range(lt=float(topic_lt)))]),
            limit=k, search_params=models.SearchParams(hnsw_ef=self._ef))
        return [int(p.id) for p in res.points]

    def insert_one(self, gid, bucket, vec):
        """S2: online upsert of one memory (durable — wait=True flushes to disk)."""
        from qdrant_client import models
        self.client.upsert(collection_name=self.coll, wait=True, points=[
            models.PointStruct(id=int(gid), vector=vec.tolist(),
                               payload={"bucket": str(int(bucket))})])

    def close(self):
        pass


class ChromaAdapter:
    name = "chroma"

    def __init__(self, host=DEFAULT_ENGINE_HOST, port=8000 + PORT_OFFSET, dim=1024, hnsw=None, scoped=False, **_):
        import chromadb
        self.client = chromadb.HttpClient(host=host, port=port)
        self.dim, self.scoped = dim, scoped
        self._colls = {}   # scoped: bucket -> collection

    def _config(self, hnsw):
        # chroma 1.0: HNSW params via CreateCollectionConfiguration (the old metadata
        # `hnsw:*` keys are ignored → config-injection failure). max_neighbors == M.
        from chromadb.api.collection_configuration import (
            CreateCollectionConfiguration, CreateHNSWConfiguration)
        return CreateCollectionConfiguration(hnsw=CreateHNSWConfiguration(
            space="cosine", ef_construction=hnsw.get("cef", 100),
            max_neighbors=hnsw.get("m", 16), ef_search=hnsw.get("sef", 128)))

    def load(self, vecs, bids, hnsw, sids=None, meta=None):
        cfg = self._config(hnsw)
        self._has_sid = sids is not None
        self._has_meta = meta is not None
        _any = self._has_sid or self._has_meta
        if self.scoped:
            # one collection (own HNSW) per bucket.
            by_b = {}
            for i in range(len(vecs)):
                by_b.setdefault(int(bids[i]), []).append(i)
            for b, idx in by_b.items():
                name = f"mem_{b}"
                try: self.client.delete_collection(name)
                except Exception: pass
                c = self.client.create_collection(name, configuration=cfg)
                self._colls[b] = c
                for s in range(0, len(idx), BATCH):
                    chunk = idx[s:s + BATCH]
                    md = [_chroma_md(i, bids, sids, meta, False) for i in chunk] if _any else None
                    c.add(ids=[str(i) for i in chunk], embeddings=[vecs[i].tolist() for i in chunk],
                          metadatas=md)
            self.setup_cost = {"kind": "collections", "structures": len(by_b), "ddl_lines": len(by_b),
                               "note": f"{len(by_b)} create_collection (one HNSW each)"}
        else:
            try: self.client.delete_collection("mem")
            except Exception: pass
            c = self.client.create_collection("mem", configuration=cfg)
            self._flat = c
            for s in range(0, len(vecs), BATCH):
                idx = list(range(s, min(s + BATCH, len(vecs))))
                md = [_chroma_md(i, bids, sids, meta, True) for i in idx]
                c.add(ids=[str(i) for i in idx], embeddings=[vecs[i].tolist() for i in idx],
                      metadatas=md)
            self.setup_cost = {"kind": "flat", "structures": 1, "ddl_lines": 1,
                               "note": "1 collection + metadata filter"}

    def query(self, qvec, bucket, k) -> List[int]:
        if self.scoped:
            r = self._colls[int(bucket)].query(query_embeddings=[qvec.tolist()], n_results=k)
        else:
            r = self._flat.query(query_embeddings=[qvec.tolist()], n_results=k,
                                 where={"bucket": int(bucket)})
        return [int(x) for x in r["ids"][0]]

    def retrieve_expand(self, qvec, bucket, k):
        """S1: NEAREST top-k then expand to full sessions. No server-side JOIN → a
        2nd filtered `get` by sid (chroma's real form). 2 roundtrips.
        Returns (expanded_turn_ids, hit_sids, roundtrips)."""
        b = int(bucket)
        if self.scoped:
            coll = self._colls[b]
            r = coll.query(query_embeddings=[qvec.tolist()], n_results=k, include=["metadatas"])
        else:
            coll = self._flat
            r = coll.query(query_embeddings=[qvec.tolist()], n_results=k,
                           where={"bucket": b}, include=["metadatas"])
        metas = (r.get("metadatas") or [[]])[0]
        hit_sids = list(dict.fromkeys(str(m.get("sid")) for m in metas if m and m.get("sid")))
        if not hit_sids:
            return [], [], 1
        where = {"sid": {"$in": hit_sids}} if self.scoped else \
                {"$and": [{"bucket": b}, {"sid": {"$in": hit_sids}}]}
        g = coll.get(where=where, include=[])   # ids only
        return [int(x) for x in g["ids"]], hit_sids, 2

    def query_filtered(self, qvec, bucket, k, topic_lt):
        """S5: metadata pre-filter (bucket AND topic < T), then HNSW over the filtered
        set (chroma applies the where-filter, then searches). Returns top-k turn ids."""
        if self.scoped:
            r = self._colls[int(bucket)].query(query_embeddings=[qvec.tolist()], n_results=k,
                                                where={"topic": {"$lt": int(topic_lt)}})
        else:
            r = self._flat.query(query_embeddings=[qvec.tolist()], n_results=k,
                                  where={"$and": [{"bucket": int(bucket)},
                                                  {"topic": {"$lt": int(topic_lt)}}]})
        return [int(x) for x in r["ids"][0]]

    def insert_one(self, gid, bucket, vec):
        """S2: online add of one memory. Chroma's durability is its own (declared in
        the record); it has no per-write fsync knob like the others."""
        self._flat.add(ids=[str(int(gid))], embeddings=[vec.tolist()],
                       metadatas=[{"bucket": int(bucket)}])

    def close(self):
        pass


ADAPTERS = {"xyzdb": XyzdbAdapter, "pgvector": PgvectorAdapter,
            "qdrant": QdrantAdapter, "chroma": ChromaAdapter}
