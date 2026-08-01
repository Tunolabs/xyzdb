# xyzDB — Operator Reference

Detailed guidance for running xyzDB. Assumes you've worked through `usage/quickstart.md`. For query syntax see `xytalk-spec.md`; for internals see `architecture.md`.

---

## 1. Server configuration

### 1.1 CLI flags

Running `xyzdb-server --help` prints the authoritative list. Highlights:

| Flag | Default | Description |
|---|---|---|
| `--path <dir>` | `./data/xyzdb` | On-disk data directory. Created if missing. |
| `--port <u16>` | `2505` | TCP port. |
| `--bind <addr>` | `127.0.0.1` | Bind address. Default is loopback; a non-loopback bind (e.g. `0.0.0.0`) is refused at startup unless `--auth-token` is set or `--insecure-allow-no-auth` is passed. |
| `--storage-profile <ssd\|hdd>` | `ssd` | Tunes block size, bloom bits, compression per keyspace. `hdd` uses 64–256 KB blocks and 14 bits/key bloom. |
| `--io-scheduler <ssd\|hdd>` | `ssd` | I/O scheduler, independent from `--storage-profile`: `ssd` = Passthrough, `hdd` = lane-aware. Lets you run an SSD storage profile with the HDD scheduler on a rotating disk. |
| `--durability <durable\|batched\|async>` | `durable` | fsync mode. See §1.2. |
| `--batch-interval <ms>` | `100` | For `batched` mode: interval between fsyncs. |
| `--memory-budget-mb <MB>` | cgroup limit, else `1024` | **Primary memory knob** (env `XYZDB_MEMORY_BUDGET_MB`). The block cache is derived from it (`budget / 4`, clamped to `[32 MiB, 2 GiB]`), and **ingest is bounded by it**: writes stall for background flush when the summed memtable footprint reaches ~35 % of the budget, so a tight container bounds its own build instead of OOM-ing (budgets ≥ ~755 MB keep today's sizes exactly). Unset, it falls back to the process's cgroup memory limit on Linux (cgroup only — never physical RAM), then to a 1 GiB default. |
| `--cache-size <MB>` | — | **Deprecated** (hidden). Direct block-cache override; warns at boot. Prefer `--memory-budget-mb`. |
| `--record-cache-size <MB>` | `0` | In-memory RecordCache budget for `INCACHE` / `OUTCACHE`. `0` disables. Deprecated alias: `--hot-cache-size`. |
| `--throttle-profile <name>` | `balanced` | Write throttle. `transactional / analytical / balanced / maintenance / bulk`. |
| `--auto-ghost-min-hits <u64>` | `5` | Auto-ghost trigger: hits in the 10-min window. |
| `--auto-ghost-min-latency-ms <f64>` | `20.0` | Auto-ghost trigger: pattern avg latency. Pass `1e9` to effectively disable auto-creation. |
| `--tls-cert <path>` | — | PEM cert chain. Together with `--tls-key`, the server accepts **TLS 1.3 only**; otherwise it serves plain TCP (with a WARN at boot). Both flags are required together. |
| `--tls-key <path>` | — | PEM private key (PKCS#8 or RSA). See `--tls-cert`. |
| `--auth-token <path>` | — | File holding the bearer token clients must present (via the `AUTH_MAGIC` preamble, before the protocol version byte). Unset = open server. |
| `--wal-path <path>` | `<path>/journal.wal` | Override the WAL location. Must share a filesystem with `--path` (snapshot hard-link orchestration assumes it). |
| `--l0-batch <usize>` | profile default | Advanced: override the L0 compaction batch size. Unset uses the storage-profile default. |
| `--block-cache-lane-admission <enabled\|disabled>` | `disabled` | When `enabled`, compaction/flush block-misses do not insert into the block cache (they still benefit from warm user-read hits). |

### 1.2 Durability modes

- **`durable`** — `fsync` after every WAL batch. Crash-safe per commit. Group commit batches concurrent writers' fsyncs, so sustained throughput is close to `batched` under multi-writer load.
- **`batched`** — `fsync` every `--batch-interval` ms. Between fsyncs, a crash loses the unsynced batches. Higher throughput on HDD (recommended there) at the cost of a bounded data-loss window.
- **`async`** — no explicit fsync; the OS flushes on its own schedule. Used for bulk-load scenarios where you plan to re-ingest on crash.

### 1.3 Storage profiles

Under the hood, each of the five keyspaces (`spatial`, `identity`, `dictionary`, `ghosts`, `vectors`) gets its own block size, bloom config, and compression. On `--storage-profile ssd`, block sizes are 32 KB (`spatial`), 4 KB (`identity` / `dictionary` / `vectors`) and 64 KB (`ghosts`); blooms are 10 bits/key (`ghosts` use none); compression is Zstd on the large/cold keyspaces (`spatial`, `ghosts`) and LZ4 on the small ones (`identity`, `dictionary`, `vectors`). `--storage-profile hdd` bumps block sizes and bloom bits for fewer seeks.

Pick `hdd` if your disk is a rotating spindle — the default `ssd` profile will work but compaction costs more.

### 1.4 Tier sizing

Benchmarks use a tier taxonomy:

| tier | CPU | RAM | Cache (derived) | Notes |
|---|---:|---:|---:|---|
| t1 | 1 | 1 GB | 256 MB | Toy only. Not enough RAM for auto-ghost on workloads with > ~10 distinct filter_descs. |
| t3 | 2 | 4 GB | 1 GB | Minimum for auto-ghost-enabled workloads. |
| t4 | 4 | 8 GB | 2 GB | Room to scale up readers. |
| t5 | 4 | 16 GB | 2 GB | Analytical workloads (cache clamped at the 2 GiB ceiling). |
| t6 | 2 | 8 GB | 2 GB | Our benchmark reference. |

The **Cache** column is no longer hand-set: it's derived as `--memory-budget-mb / 4`, clamped to `[32 MiB, 2 GiB]`. The values above assume `--memory-budget-mb` set to the tier's RAM (or, on Linux, left to the cgroup limit). An 8 GiB budget therefore yields a **2 GiB** cache — not the 1 GiB the older hand-set column implied.

Under current defaults (cap=20 Ephemeral ghosts per lobe), plan on ~3 GB peak RAM for a fintech-like workload. That fits t3 with headroom.

### 1.5 Durability contract (D1)

Under `--durability durable`, the engine guarantees every acknowledged write is durable — recoverable after process crash via WAL replay or SSTable read. The post-Finding 9 group-commit contract ensures the writer thread blocks on fsync completion before returning Ok. WAL-mutating operations (rotate, truncate) maintain a stricter invariant: all acked writes are in SSTables before the operation proceeds. See `architecture.md` §9 for the formal invariant statement, the cluster of fixes (Findings 8, 9, 10), and test coverage map.

### 1.6 Transport security and auth

TLS is off by default and, on the default loopback bind, so is auth — fine for a local or trusted-network deployment. Since 1.0 a **non-loopback bind is refused at startup unless** `--auth-token` is set or `--insecure-allow-no-auth` is passed, so an exposed server cannot come up open by accident. To harden a network-exposed server:

- **TLS 1.3**: pass `--tls-cert <chain.pem>` and `--tls-key <key.pem>` together (PKCS#8 or RSA key). The server then accepts TLS 1.3 only; passing one flag without the other is a startup error. Without them it serves plain TCP and logs a WARN at boot.
- **Bearer token**: pass `--auth-token <file>`. Clients must send the trimmed token via the `AUTH_MAGIC` (`0x41`) preamble before the protocol version byte; a mismatch closes the connection. Token plaintext on disk is a known limitation. Unset = open server.

---

## 2. The query surface

Syntax and semantics: `xytalk-spec.md`. Operational notes here.

### 2.1 Schema bootstrap

```text
LOBE "name"
ANCHOR "field" UNIQUE IN "name"
GRAVITY BY <field> IN "name"        -- on-disk co-location key
SATELLITE BY <field> IN "name"      -- sub-divides one gravity bucket
VECTOR <field> IN "name"            -- searchable embedding column
```

Lobes are free to create; anchors are declared up front because they write a unique constraint on the lobe.

The last three are **declarations of physical layout**, not indexes, and they are what decides whether a query is bounded or a full sweep:

- **`GRAVITY BY`** picks the field whose value co-locates records on disk. It is the engine's central mechanism — an equality on it becomes a bounded range scan instead of a lobe sweep. It can also be declared per record with the `*` prefix on the first `PUT` (§2.2).
- **`SATELLITE BY`** sub-divides each gravity bucket by a second field, so a query pinning **both** reads only the matching rows. A pure optimisation — same rows, same order — and **the lobe must be empty when you declare it**: existing records would stay in the default sub-bucket where a bounded query cannot reach them. One axis per lobe. Full rules in `docs/xytalk-spec.md` §2.2.2, including that `SET` re-places a record whose satellite field changed while `ON CONFLICT UPDATE` does not.
- **`VECTOR`** declares an f32 embedding column for `NEAREST`. The engine never embeds; the caller supplies the vector.

### 2.2 PUT patterns

- **Single**: `PUT {rfc: "X", name: "Y"} IN "clients"` — one record.
- **Linked**: `PUT {_type: "Credit", amount: 1000} IN "credits" LINK TO "clients" WHERE rfc = "X" AS "owner"` — writes the record with a `_link_owner` field pointing at the parent's LID. Co-locates on disk.
- **Batched**: `PUT BATCH IN "lobe" [{...}, {...}]` — atomic multi-record write within a single WAL commit. Max 10,000 records per batch (a larger batch is rejected whole); chunk larger loads into ≤10K batches, each atomic on its own.

### 2.3 Reading

- **FIND**: point lookup by anchor — a single dictionary-keyspace `get` (a bloom-filtered LSM lookup), not a scan.
- **PULL**: FIND plus every linked descendant up to `depth=N`. Single block scan usually.
- **SCAN**: iterate the lobe. Router decides whether to scan the primary keyspace or a ghost. Add `LIMIT` if you don't need all rows; the engine streams.
  - **Default cap (v0.2.5.1)**: SCAN without explicit `LIMIT` and without `ORDER BY` is capped at 1 000 records and emits a `tracing::warn`. Add `LIMIT N` (≤ 10 000) for larger pages, or paginate via `CURSOR` (see §2.4).
  - **Hard ceiling**: `LIMIT N > 10000` is rejected. Use chunked-streaming SCAN (V2 `FORMAT_*_CHUNKED`) or paginate.
- **AGGREGATE** / **GROUP BY**: streaming accumulation. If a PreComputed ghost covers the pattern, response is zero-scan. The default-LIMIT cap does not apply to aggregate paths.

### 2.4 Cursor pagination (v0.2.5.1)

Plain SCAN that exceeds its page boundary returns a `PaginatedRecords` shape with an opaque `cursor` token and `has_more: true`. Pass the same token back in `SCAN ... CURSOR "<token>"` to fetch the next page.

```text
-- First page: no cursor.
SCAN "creditos" WHERE rfc = "X" LIMIT 1000
-- Returns:  records (1000), cursor = "AQEAAQ...", has_more = true

-- Next page: same query + cursor.
SCAN "creditos" WHERE rfc = "X" LIMIT 1000 CURSOR "AQEAAQ..."
-- Returns:  records (next 1000), cursor = "AQ...", has_more = true | false
```

**Not every `PaginatedRecords` is resumable — check `cursor`, not `has_more`.** A `NEAREST` cut short by the latency budget returns the *same shape* with `has_more: true` but **`cursor: null`**, plus a `budget_stop` object describing the cut. It is a partial answer, not a page: there is nothing to resume, because resuming would repeat the whole scoring pass. Branch on `cursor` being present; treating `has_more: true` alone as "call again with the token" leaves you with no token. See `docs/xytalk-spec.md` §2.20 for `budget_stop` and what its counts license you to conclude.

**Constraints**:
- Cursor + `ORDER BY` rejected (paginated sort is not yet implemented).
- Cursor + ghost-routed SCAN rejected (forces Primary route; ghost cursors are not yet implemented).
- The cursor binds to the WHERE clause via an xxh3-64 filter checksum; reusing a cursor under a different filter errors with `cursor invalid: WHERE clause does not match`.
- Cursor tokens are version-bound (filter checksum derived from the parser AST's `Debug` impl); upgrades that touch `FilterExpr` invalidate in-flight cursors.

### 2.5 Ghost management

- `CREATE GHOST "name" FROM "lobe" [WHERE ...] ORDER BY (field | metric) [GROUP BY ...] [AGGREGATE ...] [EMBED ...]` — declare a permanent ghost. `GROUP BY + AGGREGATE` makes the ghost PreComputed-routable (zero-scan response on matching aggregate queries). `ORDER BY <metric>` (e.g. `sum(monto) DESC`) keeps a metric-ordered rollup so `TOP n BY <metric>` reads O(N) instead of O(M). `EMBED` adds operator-supplied projection. See `xytalk-spec.md` §2.15.
- `SHOW GHOSTS` — list all, by type and status.
- `REFRESH GHOST "name"` — drop and rebuild (e.g. after large write bursts).
- `DROP GHOST "name"` — delete permanently.

Auto-ghosts (Ephemeral / Promoted classes) appear without `CREATE GHOST` when hot patterns cross the trigger threshold. They look identical to manual ghosts in `SHOW GHOSTS` but have non-null `ttl_seconds`.

### 2.6 Admin verbs (v0.2.5.1)

Operator-grade commands move out of xyTalk and into the `xyzdb-cli admin` subcommand. As of 1.0 the language statements still execute, emitting a `tracing::warn` deprecation that points to the `admin` subcommand; retiring them entirely is still pending.

```bash
xyzdb-cli admin compact                  # COMPACT (every keyspace)
xyzdb-cli admin analyze <lobe>           # ANALYZE "<lobe>"
xyzdb-cli admin bulkmode <on|off>        # BULKMODE ON / OFF
xyzdb-cli admin migrate <lobe>           # MIGRATE "<lobe>"
xyzdb-cli admin migrate --all            # MIGRATE (every lobe)
```

The CLI is a thin wrapper over the existing V1 protocol — same wire shape, same engine paths. New code should target the admin subcommand from v0.2.5.1 onwards. Existing drivers, validation suites, and external clients keep working under v0.2.5.x.

`INCACHE` / `OUTCACHE` are NOT admin — they remain operator-grade *workload tuning* inside the language. See spec §2.10.

---

## 3. The wire protocol

### 3.1 V1 (text)

Request: `[version=1: u8][length: u32 BE][UTF-8 xyTalk payload]`.
Response: `[status: u8][length: u32 BE][UTF-8 or plaintext payload]`.
Used by the REPL; easy to script from any language.

### 3.2 V2 (formatted)

Same framing but response bodies are JSON.

### 3.3 V3 (binary bulk)

Chunked streaming for high-throughput ingest. Client sends framed records; server acks per chunk.

All framing lengths are u32, but a single frame is capped at `MAX_FRAME_SIZE` = **16 MiB** (see `PROTOCOL.md`); an over-size frame is rejected. For larger writes, chunk at the client or use V3 streaming.

---

## 4. Observability

### 4.1 Server logs

`tracing`-based. Defaults to `INFO`. Log lines that matter for ops:

- `xyzDB server listening on ADDR` — server is up.
- `HDD + durable: consider --durability batched for higher write throughput` — hint at startup.
- `Ghost 'X' created: N index entries ...` — manual or auto-ghost was materialised.
- `LRU eviction: dropped Ephemeral 'X' from lobe Y` — LRU cap hit.
- `Router: scan routed to ghost 'X' (ordered/filter scan)` — a scan hit a ghost.
- `turba-compact: error: data corruption: bad X` — compact worker failed one cycle. Zero under current builds. Was the v0.2.0-alpha Finding 4 class of bug; closed in v0.2.1.

### 4.2 Counters

Engine exposes:
- `TurbaEngine::total_compact_errors()` — monotonic count of failed compact cycles. Should be zero.
- Per-tree: `Tree::compact_error_count()`, `Tree::l0_table_count()`, `Tree::sealed_memtable_count()`, `Tree::flushed_seqno()`.

These are internal APIs. The `/stats` HTTP endpoint surfaces them as JSON for external scraping (it follows `--auth-token` like `GET /`); `/metrics`, `/health` and `/ready` are served on the wire path. See `architecture.md` §10.

**Correctness signals, not capacity metrics.** `STATS` also carries `invariant_guards` — counters for states the read path assumes impossible — and `recovered_from_wal`, with matching `xyzdb_invariant_*` and `xyzdb_recovered_from_wal` series on `/metrics`. **Any non-zero `xyzdb_invariant_*` is an engine bug: page, do not tune.** They are emitted for every keyspace even at zero, because a missing series is indistinguishable from a scrape gap. `xyzdb_recovered_from_wal == 1` is different in kind — it reports a degraded *mode*, not a fault: a process that replayed WAL re-confirms anchor misses without the bloom for its whole life, which is correct but costs a level descent per miss until restart. Alerting thresholds are in `OPERATIONS.md` §5.

### 4.3 Benchmark harness

`benchmarks/native/` is the cross-engine harness (xyzDB vs PostgreSQL / MongoDB) for the fintech ERP workload. It drives setup → load → cold → concurrent → verify against a running engine and writes JSON + CSV + Markdown reports. Example (single engine):

```bash
cd benchmarks/native
cargo build --release -p native-orchestrator
./target/release/native-bench \
  --engine xyzdb --scale 0.1 --storage ssd \
  --duration 3600 --cold-runs 100 \
  --output ./results
```

Scale `0.1` ≈ 14.7M records (primary), `1.0` ≈ 149M. See `scripts/run_aws_4engines.sh` for the sequential multi-engine runner.

---

## 5. Operational gotchas

### 5.1 Data directory format versions

On-disk format is bumped on breaking changes. Current data is `MANIFEST_VERSION = 5` (turba-engine) and `GHOST_META_FORMAT = 0x09` (xyzdb-engine, the ghost-persistence format). v0.8.8 added the vectors keyspace and the V5 record format; a later format bump widened the on-disk key to 24 bytes with a `sat` axis, taking `MANIFEST_VERSION` to 5. That axis was reserved while there were no users and **went live in 1.1 with `SATELLITE BY` — no format change was needed**, which is why 1.0 and 1.1 share `MANIFEST_VERSION = 5` and a 1.0.x data directory opens unchanged. Opening data written by an older format fails with `Error::IncompatibleFormat` and a clear message pointing at re-ingest.

No in-place migration. If you need to preserve v0.2.0-alpha data, run a side-by-side v0.2.0-alpha binary, export to JSON, re-ingest with the current binary.

### 5.2 Mac + OrbStack + Docker benchmarks

- Start `caffeinate -d` before long benchmarks. macOS display sleep pauses OrbStack containers.
- Use `127.0.0.1`, not `localhost`. Localhost resolves to both IPv4 and IPv6 and doubles the TCP connection budget, which can exhaust ephemeral ports under high concurrency.
- After aborting a run, wait 60–90 seconds before starting the next so TIME_WAIT sockets drain.
- USB-mounted data dirs work via Docker bind mounts; make sure the USB drive is mounted as `local` (APFS, exFAT, HFS+) — virtiofs-only mounts can hit fsync semantics differences.

### 5.3 Clean shutdown

No explicit shutdown command. `SIGTERM` (or `SIGINT` via Ctrl-C) is graceful: the server finishes in-flight requests, flushes memtables, syncs the WAL, compacts if pending, then exits.

### 5.4 Write backpressure

Under sustained write load faster than compaction can keep up, L0 tables pile up. At L0 > 4 tables, the engine returns `Error::Overloaded` to new writes until compaction catches up. Retry with exponential backoff on the client.

### 5.5 Resource limits

- **File descriptors:** the server opens one FD per active SSTable plus the WAL. `ulimit -n 65536` is recommended (Docker image sets this).
- **Memory:** peak ≈ (block cache) + (memtable × 4) + (hot cache if enabled) + (~3 GB headroom for auto-ghost on a fintech-sized workload). Plan for ≥ 4 × block-cache.

### 5.6 Ghost churn under adversarial workloads

A workload that randomises filter values uniformly over a large pool (the adversarial benchmark case) can cause auto-ghost LRU churn — many creates, many evictions, no net benefit. Symptoms: `LRU eviction: dropped Ephemeral …` log lines at high rate, marginal read-latency wins.

Diagnose: `auto-ghost created` vs `LRU eviction` lines in a 10-minute window. If their counts are close and both non-trivial, you're thrashing.

Mitigations:
- Disable auto-creation: `--auto-ghost-min-latency-ms 1e9`. Use manual `CREATE GHOST` for the known-hot patterns.
- Tune thresholds higher: `--auto-ghost-min-hits 20`, `--auto-ghost-min-latency-ms 100`. Only very-hot patterns get ghosts.

Realistic workloads (80/20 distribution) don't thrash at current defaults. This matters only under adversarial or highly parameterised traffic.

---

## 6. Where to go next

- **Tune for your workload**: pick a storage profile, pick a durability mode, run `benchmarks/concurrent` with your profile to validate.
- **Read `xytalk-spec.md`** for the full language surface.
- **Read `architecture.md`** before extending or patching.
- **Watch `roadmap.md`** to understand what's changing next.

Operational issues and feedback: `ivan@tuno.bar` or project issues on GitHub.
