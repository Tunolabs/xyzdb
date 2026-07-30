# Agentic benchmark — envelope capacity matrix

Capacity **and** latency of an agent-memory deployment across hardware envelopes:
does each deployment fit a given tier, and how fast does it serve? xyzDB is measured
against `pgvector`, `qdrant`, and `chroma`, each in its idiomatic best-form config, on
the **same** corpus, queries, and machine. Consolidated results and methodology:
[`docs/benchmark-agentic.md`](../../docs/benchmark-agentic.md).

This directory is self-contained: it drives the engines over the wire using the minimal
reference client shipped in [`examples/client/python`](../../examples/client/python) — no
external SDK. The only out-of-repo input is the public LongMemEval dataset, which
`fetch_corpus.sh` downloads from its canonical source.

---

## 1. What it measures

Corpus A = **LongMemEval-S** (real conversational turns), embedded with
**`BAAI/bge-large-en-v1.5`** (1024-d), gravity bucket = `question_id`. Six scenarios, run
by `measure_s1.py … measure_s6.py`:

| Scenario | Business op |
|---|---|
| S1 | retrieve top-k in a session bucket, then expand to full sessions |
| S2 | online insert of one memory (durability path) |
| S3 | multi-agent fleet lifecycle (create/destroy the Nth tenant), RAM-vs-#tenants curve |
| S4 | time-to-first-query after a cold start |
| S5 | exact structured filter (`topic < T`) + NEAREST |
| S6 | write → update → NEAREST → aggregate in one deployment |

Metrics are never merged: latency (p50/p99), recall, RAM peak (query balloon), RAM at
rest, disk at rest, and fit/OOM verdict per tier. The vector recall in the matrix tables
is exact NEAREST vs the oracle top-k (= 1.00 by construction); the separate academic
LongMemEval recall (~0.9x, embedder-dependent — see section 3) is the `checkpoint_lme.py`
sanity check.

## 2. Quick start — one command

On the box (branch `0.9.8` checked out, `/mnt/ssd` mounted):

```bash
cd benchmarks/agentic
./run_envelope_aws.sh
```

That is the whole thing. `run_envelope_aws.sh` is **self-provisioning**: on first run it
creates the Python venv and installs deps, downloads the LongMemEval corpus and embeds it,
builds the xyzDB engine image from this checkout, and pulls the pgvector / qdrant / chroma
images — then runs the matrix. Re-running detects and skips whatever is already in place
(force a fresh engine image after a `git pull` with `REBUILD_IMG=1`).

The only things it cannot create for you:

- **Docker**, and **Python 3** with the `venv` module
  (`sudo apt-get install -y python3-venv`).
- A mounted, writable **`/mnt/ssd`** — per-cell data (pg at 246k ≈ 28 GB) would otherwise
  fill the root disk.
- **Swap.** The tight tiers (T1/T2) at 246k spill the load spike to swap, so without a host
  swapfile they OOM (`docker --memory-swap` is a no-op without host swap). The runner warns;
  enable 4 GB on the SSD:
  `fallocate -l 4G /mnt/ssd/swapfile && chmod 600 /mnt/ssd/swapfile && mkswap /mnt/ssd/swapfile && swapon /mnt/ssd/swapfile`.
  Note that a swap-assisted FIT at the tightest corner thrashes (higher `load_s`) — report it
  as such, it is not a fit in hard RAM.

Narrow the run with env vars — e.g. xyzDB only, one tier, two scenarios:

```bash
DEPLOYMENTS_RUN=xyz TIERS=T1 SCALES="30000 246738" SCENARIOS="s1 s5" ./run_envelope_aws.sh
```

## 3. The corpus (prebuilt by default, or build from source)

The corpus is **LongMemEval-S**, embedded with **`BAAI/bge-large-en-v1.5`** (1024-d). The
runner provisions it automatically, two ways:

- **Default — prebuilt (fast).** `fetch_embeddings.sh` downloads the embeddings
  (`cvec.npy` / `qvec.npy` / `meta.json`, sha256-pinned, ~790 MB) from the release at
  [`Tunolabs/xyzdb-agentic-embeddings`](https://github.com/Tunolabs/xyzdb-agentic-embeddings).
  No torch, no GPU, seconds. These are the vectors behind the published numbers.
- **From source — set `BENCH_BUILD_CORPUS=1`.** The runner instead runs `fetch_corpus.sh`
  (download LongMemEval-S, sha-pinned) + `build_lme.py` (embed with bge-large). Needs torch +
  sentence-transformers; **GPU recommended** — on CPU the embed is ~2 days.

By hand (only to provision the corpus without launching the matrix):

```bash
./fetch_embeddings.sh                                 # default: prebuilt, sha-verified
# or, from source:
.venv/bin/pip install -r requirements-corpus.txt
./fetch_corpus.sh && .venv/bin/python build_lme.py    # QMAX=20 for a fast smoke
```

- The embedder runs **once** and every engine loads the same `cvec.npy`/`qvec.npy`, so the
  cross-engine comparison is fair by construction — independent of the embedder version.
- A source build pins `sentence-transformers==5.1.2` (deterministic, academic recall ~0.91);
  the prebuilt release was made with an earlier line (~0.96). The matrix tables'
  `recall = 1.00` is exact NEAREST vs the oracle and does not depend on the embedder.

## 4. The matrix

The full run is **4 tiers × 3 scales × 4 deployments × 6 scenarios**, one engine container
at a time, fresh container + wiped data dir per cell. Env vars: `TIERS` (T1–T4), `SCALES`,
`DEPLOYMENTS_RUN` (`xyz pg qdrant+pg chroma+pg`), `SCENARIOS` (`s1…s6`), `XYZDB_IMG`,
`REBUILD_IMG`, `BUILD_TIMEOUT`, `OUT`, `STORAGE_ROOT`.

On a Mac / non-SSD host, `run_envelope_full.sh` is the underlying runner (named volumes, no
`/mnt/ssd`); provision the venv + corpus first (section 3). The hands-off path is
`run_envelope_aws.sh` on the box.

## 5. Results

One JSON record per cell under `results/envelope_full/` (override with `OUT`). Each record
carries the deployment, tier, scale, scenario, the metrics above, and a `status`
(`null` = ok, else `OOM` / `crashed` / `thrash`). Aggregate with `report_agentic.py`.

## 6. Reproducibility notes

- **The xyzDB client is in-repo.** `examples/client/python/xyzdb_minimal.py` is a thin,
  stdlib-only wire client. The runner puts it on `PYTHONPATH`. It is a pedagogical example
  that is **also load-bearing for this harness** — if you simplify it, re-run the A/B in
  `docs/benchmark-agentic.md` before trusting the numbers.
- **Batch size is fixed at 600 records/PUT BATCH** (`adapters.py::XYZ_PUT_BATCH`). It is a
  measurement parameter, not a client default: 600 × 1024-d ≈ 11.5 MB of xyTalk text, under
  the engine's 16 MiB frame cap. This equals what the full SDK sent per call.
- **Recall is embedder-version-sensitive, so the version is pinned** in
  `requirements-corpus.txt` (`sentence-transformers==5.1.2`, academic recall ~0.91). The
  older 2.2.2 gave ~0.96 — an embedder change, not an engine regression. Rebuilds are
  therefore deterministic; to match an earlier run's exact vectors, reuse its prebuilt
  `corpora/lme/` (section 3).
- **Disposable-container auth.** The runner launches xyzDB with
  `--insecure-allow-no-auth` because from 1.0 the engine refuses a non-loopback bind
  without a token. This is safe only for a throwaway container on a private benchmark host
  — never copy it into a real deployment (use `--auth-token`).
- **DIRECTION vs publishable.** Mac / OrbStack runs mediate the page cache and give
  direction only; the publishable dataset is the AWS m6a.xlarge (x86-64-v3) run named in
  `docs/benchmark-agentic.md`.

## 7. Citation

LongMemEval is a public benchmark; this harness does not vendor or re-host it.

> Di Wu, Hongwei Wang, Wenhao Yu, Yuwei Zhang, Kai-Wei Chang, Dong Yu.
> *LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory.* ICLR 2025.
> [arXiv:2410.10813](https://arxiv.org/abs/2410.10813) ·
> [github.com/xiaowu0162/LongMemEval](https://github.com/xiaowu0162/LongMemEval)
