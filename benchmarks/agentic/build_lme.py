#!/usr/bin/env python3
"""Corpus A — LongMemEval (recall axis), per prompt-agente-bench-0.9 §Corpus A.

Real bge-large embeddings of real conversational turns. Bucket = question_id (the
natural gravity key — NO invented k-means). GT = answer_session_ids (session-level,
INV-1). Turn-granularity embedding (bge truncates at 512 tokens; sessions exceed it).
Dedup by text identity (INV-3, shared distractor sessions get the SAME vector under
DIFFERENT question_ids = honest multi-bucketing, not padding). Split 50-dev/450-held-out,
stratified by question_type, seed 20260701 (INV-2).

Artifact -> corpora/lme/: cvec.npy (unique turn vectors), qvec.npy (500 query vectors),
meta.json (turn records + query GT + dev split).
"""
import json
import os
import hashlib
import numpy as np
import torch
from sentence_transformers import SentenceTransformer

# FULL haystack with distractors (~48 sessions/q, real needle-in-haystack) — NOT the
# oracle (answer-only). This is the source the dev50 provenance names.
_HERE = os.path.dirname(os.path.abspath(__file__))
# Source corpus: the official LongMemEval-S "cleaned" file. `fetch_corpus.sh` downloads
# it to data/; override the location with the LME_SRC env var.
SRC = os.environ.get("LME_SRC", os.path.join(_HERE, "data", "longmemeval_s_cleaned.json"))
# Output embeddings dir. MUST match measure_lme.py's BENCH_CORP default so the recall
# run reads exactly what this builder writes. Override with BENCH_CORP.
OUT = os.environ.get("BENCH_CORP", os.path.join(_HERE, "corpora", "lme"))
# bge-large-en-v1.5 retrieval: queries get the instruction prefix, passages do not.
QUERY_PREFIX = "Represent this sentence for searching relevant passages: "
SEED = 20260701
# fp16 + 256-token cap: avg turn ~230 tokens so seq256 covers most; halves embed time.
# QMAX (env) = subset of questions for a fast direction run; unset/0 = all 500.
MAX_SEQ = int(os.environ.get("MAX_SEQ", 256))
QMAX = int(os.environ.get("QMAX", 0))


def stratified_dev(queries, n_dev=50):
    """Deterministic stratified (by question_type) dev split of exactly n_dev ids."""
    by_type = {}
    for qid, qtype, _, _ in queries:
        by_type.setdefault(qtype, []).append(qid)
    rng = np.random.default_rng(SEED)
    quotas = {t: len(ids) * n_dev / len(queries) for t, ids in by_type.items()}
    dev = []
    # largest-remainder to hit exactly n_dev
    base = {t: int(q) for t, q in quotas.items()}
    for t, ids in by_type.items():
        ids = sorted(ids)
        rng.shuffle(ids)
        by_type[t] = ids
        dev += ids[:base[t]]
    rem = sorted(quotas, key=lambda t: quotas[t] - base[t], reverse=True)
    i = 0
    while len(dev) < n_dev:
        t = rem[i % len(rem)]
        if base[t] < len(by_type[t]):
            dev.append(by_type[t][base[t]])
            base[t] += 1
        i += 1
    return sorted(dev[:n_dev])


def main():
    d = json.load(open(SRC))
    if QMAX:
        d = d[:QMAX]
    turns, queries = [], []
    for q in d:
        qid = q["question_id"]
        queries.append((qid, q["question_type"], q["question"], list(q["answer_session_ids"])))
        for sid, sess in zip(q["haystack_session_ids"], q["haystack_sessions"]):
            for tidx, turn in enumerate(sess):
                if isinstance(turn, dict):
                    text = (turn.get("content") or turn.get("text") or "").strip()
                    if text:
                        ha = turn.get("has_answer")
                        turns.append((qid, sid, tidx, ha in (True, "True", "true"), text))
    print(f"questions={len(queries)}  turn-records={len(turns)}")

    uniq = {}
    for *_, text in turns:
        if text not in uniq:
            uniq[text] = len(uniq)
    texts = [None] * len(uniq)
    for t, i in uniq.items():
        texts[i] = t
    print(f"unique turn texts (dedup) = {len(texts)}")

    dev = stratified_dev(queries)
    dev_set = set(dev)
    print(f"dev split = {len(dev)} (held-out = {len(queries) - len(dev)})")

    device = "mps" if torch.backends.mps.is_available() else "cpu"
    print(f"embedding on {device}  (fp16, seq={MAX_SEQ}, batch=128) ...")
    model = SentenceTransformer("BAAI/bge-large-en-v1.5", device=device)
    if device == "mps":
        model.half()
    model.max_seq_length = MAX_SEQ
    cvec = model.encode(texts, normalize_embeddings=True, batch_size=128,
                        show_progress_bar=True, convert_to_numpy=True).astype(np.float32)
    qvec = model.encode([QUERY_PREFIX + qt for _, _, qt, _ in queries],
                        normalize_embeddings=True, batch_size=128,
                        show_progress_bar=True, convert_to_numpy=True).astype(np.float32)

    os.makedirs(OUT, exist_ok=True)
    np.save(f"{OUT}/cvec.npy", cvec)
    np.save(f"{OUT}/qvec.npy", qvec)
    meta = {
        "embedder": "BAAI/bge-large-en-v1.5", "dim": 1024, "seed": SEED,
        "turns": {
            "qid": [t[0] for t in turns], "sid": [t[1] for t in turns],
            "tidx": [t[2] for t in turns], "has_answer": [t[3] for t in turns],
            "vec_idx": [uniq[t[4]] for t in turns],
        },
        "queries": [{"qid": q[0], "qtype": q[1], "gt": q[3], "split": ("dev" if q[0] in dev_set else "held")}
                    for q in queries],
        "dev_ids": dev,
    }
    json.dump(meta, open(f"{OUT}/meta.json", "w"))
    sha = hashlib.sha256(json.dumps(meta["dev_ids"]).encode()).hexdigest()[:16]
    print(f"saved: cvec{cvec.shape} qvec{qvec.shape} -> {OUT}  dev_sha={sha}")


if __name__ == "__main__":
    main()
