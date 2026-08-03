# xyzDB Operations

Operator runbook for xyzDB 1.1. This is the document an operator opens when running xyzDB in an internal deployment, without needing to read `docs/architecture.md` end-to-end.

> **Status:** current as of 1.1 (2026-08-01). Covers single-node deployment, configuration, health checks, backup/restore, observability, and the operational caveats that still apply. 1.1 is an in-place upgrade from 1.0 (§8) and adds two things an operator sees: the `xyzdb_invariant_*` / `xyzdb_recovered_from_wal` series (§5), and a one-time `REFRESH GHOST` if you run aggregate ghosts over lobes that take upserts (§8).

---

## 1. Deployment topology

xyzDB ships as a **single-process daemon** (`xyzdb-server`) that owns one engine instance, listens on one TCP port (default `2505`), and multiplexes three protocol surfaces onto the same socket:

| Surface | First byte | What sees it |
|---|---|---|
| Wire V1/V2/V3/V4 (text/binary/bulk-load/bound-params) | `0x01` / `0x02` / `0x03` / `0x04` | xyzdb-cli, the `xyzdb` clients (Python, TypeScript, Rust), xyzdb-mcp `--connect`, xyzdb-bench |
| Bearer-token auth preamble (`AUTH_MAGIC`) | `0x41` (`A`) | Same wire clients when `--auth-token` is set |
| HTTP/1.1 GET (operator surface, /stats poll) | `G` / `H` / `P` / `O` / `D` / `T` / `C` | Browsers, curl, Prometheus (via a `/metrics` sidecar — see §5) |

The dispatcher in `xyzdb-server/src/connection.rs` peeks the first byte once and routes to either the wire path or the HTTP path. There is no second listener and no second port — TLS, when configured via `--tls-cert`/`--tls-key`, wraps the same socket and the same byte-detection runs on the decrypted stream.

### 1.1 The image picks its instruction set at startup

On x86-64 the published images carry **two** builds of the binary — a portable
baseline and an AVX2 (`x86-64-v3`) one — and the container entrypoint
`xyzdb-launch` execs whichever the host CPU can actually run. You choose nothing
and the tag does not change.

It says which one on **stderr**, once, before handing over:

```
xyzdb-launch: exec xyzdb-server.v3 (CPU implements x86-64-v3 (AVX2/FMA/BMI2))
```

Read that line when comparing two hosts: a machine that reports
`xyzdb-server.v2 (CPU does not implement x86-64-v3 ...)` is running the portable
build and will be slower on vector work. It is not a misconfiguration — it is the
only build that host can execute — but it explains a latency difference that
would otherwise look inexplicable.

The launcher `exec`s rather than spawns, so the engine still becomes the
container's process image and receives `SIGTERM` directly (§9 Shutdown). If
neither build is present the container exits 78 (`EX_CONFIG`) with the paths it
looked for, rather than starting something unintended.

### 1.2 Standalone

```
        ┌──────────────────────────────────────┐
        │          xyzdb-server                │
        │   one process · port 2505            │
        │                                      │
        │   ┌────────────┐    ┌────────────┐   │
        │   │ wire V1/V2 │    │  HTTP /    │   │
        │   │   V3 BULK  │    │  HTTP /stats │ │
        │   └────────────┘    └────────────┘   │
        │       ↑ ↑                  ↑         │
        └───────┼─┼──────────────────┼─────────┘
                │ │                  │
        xyzdb-cli, SDK         operator browser
```

Access patterns:

- **Wire clients** (xyzdb-cli, xyzdb-python, xyzdb-bench, xyzdb-mcp `--connect`): connect to `host:2505`, send `AUTH_MAGIC` + token if `--auth-token` is configured (env `XYZDB_TOKEN` for the SDKs), then proceed with V1/V2/V3.
- **Operator browser**: open `http://host:2505/` (or `https://` when TLS is on). The page polls `GET /stats` every 5 s for live state.
- **Prometheus scraper**: `/metrics` is served natively on the wire — a V1 `/metrics` query returns the Prometheus exposition, and it follows `--auth-token` like `STATS`. Prometheus scrapes HTTP, so bridge it with a small sidecar that issues the wire query (`echo /metrics | xyzdb-cli`, or a ~30-line wrapper) and re-exposes the body over HTTP. See §5.

### 1.3 MCP `--connect` topology (multi-process)

```
        ┌─────────────┐          ┌──────────────┐
        │ MCP /       │  stdio   │   xyzdb-mcp  │   wire V2  ┌──────────────┐
        │  agent      │ ────────▶│   --connect  │ ──────────▶│ xyzdb-server │
        └─────────────┘          └──────────────┘            └──────────────┘
                                                              port 2505
```

`xyzdb-mcp --connect host:2505` is a thin wire-V2 client; the engine lives in `xyzdb-server`. `xyzdb-mcp --embed` (the canonical single-process mode) opens the engine in-process and does **not** use the network at all. The operator surface is only relevant for the `--connect` topology (browse the *server* host, not the MCP host).

> **MCP `--connect` auth**: `xyzdb-mcp --connect` reads `XYZDB_TOKEN` and sends it as the bearer-token preamble, so it authenticates against an upstream server started with `--auth-token`. Set `XYZDB_TOKEN` in the MCP process environment to the same token; leave it unset when the server is open.

### 1.4 Operator surface — accessing `GET /`

| Step | Without `--auth-token` | With `--auth-token <file>` |
|---|---|---|
| 1 | Open `http://host:2505/` in a browser | Same |
| 2 | Page loads; auto-polls `/stats` every 5 s | Page returns `401 Unauthorized` |
| 3 | — | Set the bearer token via one of: |
|   |   | a) `Cookie: xyzdb_token=<token>` (preferred — survives navigations and refreshes; set via DevTools → Application → Cookies) |
|   |   | b) `?token=<token>` query string (debug ergonomic — leaks into access logs and browser history; not recommended for production) |
|   |   | c) `Authorization: Bearer <token>` (curl/script use; browsers do not send this on plain GET unless via fetch with explicit headers) |

**TLS**: if `--tls-cert`/`--tls-key` are set, replace `http://` with `https://`. The same auth applies; the cookie is `xyzdb_token` regardless of scheme.

**XSS posture**: the operator HTML is fully static (`include_str!`) — the server never interpolates user-controlled state into the response body. The HTML's JS calls `escapeHtml()` on every string drawn from `/stats` (lobe names, ghost names, statement fingerprints) before DOM injection, so an attacker who inserts `<script>` into a keyspace name cannot reach a browser.

**Performance**: `/stats` polled every 5 s is the same payload the wire-side `STATS` query returns; cost is dominated by `serde_json::to_vec(&stats_snapshot)` which is microseconds per call.

### 1.5 Single-host vs distributed (post-v1.0)

xyzDB is **single-host only**. There is no cluster mode, no replica, no shard router. Durability is local-disk (WAL + fsync) and operability is single-binary-on-one-VM. Distributed shapes (cross-AZ replication, leader/follower, multi-tenant lobe partitioning) are **post-v1.0** explorations.

Operationally this means:

- HA = run two independent xyzDB instances and serve writes to one; restoring the other is a snapshot copy + `xyzdb-cli admin snapshot restore` (see §4 Backup). Hot snapshots are safe under sustained load (the compaction-drain fix — see §4).
- Read scale-out = add more cores/RAM to the single host. The shape this is tuned and benchmarked against is **2 vCPU / 8 GB**; smaller envelopes work (the engine derives its budgets from the cgroup limit) but that is the reference.
- Cross-host coordination = none; the operator surface on each instance is independent.

---

## 2. Configuration

`xyzdb-server` flags (run `xyzdb-server --help` for the complete list with defaults):

| Flag | Default | Notes |
|---|---|---|
| `--path` | `./data/xyzdb` | Data directory. Must be on a single filesystem (snapshot/restore use hard links). |
| `--port` | `2505` | TCP port. |
| `--version` | — | Print the version and exit. The first step of §8.4; it exists since 1.1.0. |
| `--bind` | `127.0.0.1` | Bind address. Loopback by default (not reachable off-host). A non-loopback bind (e.g. `0.0.0.0`) with no `--auth-token` refuses to start. |
| `--storage-profile` | `ssd` | `ssd` or `hdd`. HDD widens block size to 64 KB and bloom to 14 bits/key. |
| `--io-scheduler` | `ssd` | `ssd` (Passthrough) or `hdd` (Laned). Independent from `--storage-profile`. |
| `--durability` | `durable` | `durable` (fsync per group commit), `batched` (timer @ `--batch-interval` ms), `async` (OS-scheduled). See [§3.2 of architecture.md](docs/architecture.md#32-durability) for the full mapping. |
| `--batch-interval` | `100` (ms) | Flush interval when `--durability batched`. |
| `--memory-budget-mb` | cgroup limit, else `1024` (MB) | Primary memory knob (env `XYZDB_MEMORY_BUDGET_MB`). The block cache is DERIVED as budget/4, clamped to [32 MiB, 2 GiB]. Precedence: this flag → cgroup memory limit (Linux; cgroup only, never physical RAM) → 1 GiB default. |
| `--cache-size` | (none) | **Deprecated / hidden override.** When set, overrides the derived block cache and logs a warning. Size memory via `--memory-budget-mb` instead. |
| `--throttle-profile` | `balanced` | `transactional`, `analytical`, `balanced`, `maintenance`, `bulk`. See `crates/engine/src/throttle.rs` for absolute limits per profile. |
| `--record-cache-size` | `0` (MB) | In-memory RecordCache budget for `INCACHE` / `OUTCACHE`. `0` disables. Deprecated alias: `--hot-cache-size`. |
| `--wal-path` | `<path>/journal.wal` | Override the WAL location. Must share a filesystem with `--path` (snapshot hard-link orchestration assumes it). |
| `--l0-batch` | profile default | Advanced: override the L0 compaction batch size. Unset uses the storage-profile default. |
| `--nearest-budget-ms` | `3000` (ms) | Wall-clock airbag for a single `NEAREST`. A latency wall, never a recall wall — and what expiry does depends on the path: a **bounded** `NEAREST` returns the best-scoring rows found so far with a `budget_stop` object, while an **unbounded** scoring scan aborts with an error. `0` disables. The partial's contract is `docs/xytalk-spec.md` §2.20. |
| `--block-cache-lane-admission` | `disabled` | Lane-aware block-cache admission. Off by default; see §7 for what it changes and why it is opt-in. |
| `--auto-ghost-min-hits` | `5` | Hit threshold within the 10 min window for auto-ghost promotion. |
| `--auto-ghost-min-latency-ms` | `20.0` | Average latency threshold. Pass `1e9` to effectively disable auto-ghost. |
| `--tls-cert` | (none) | PEM cert chain. Set together with `--tls-key`; both empty = plain TCP with WARN. |
| `--tls-key` | (none) | PEM private key (PKCS#8 or RSA). |
| `--auth-token` | (none) | Path to a UTF-8 file with the bearer token. Trimmed at boot. |
| `--insecure-allow-no-auth` | (off) | Explicitly permit a non-loopback bind with no token. Exposes an open server — only behind your own access control. |

**TLS + auth recommendation**: production deployments should set both `--tls-cert`/`--tls-key` AND `--auth-token`. Without TLS, the bearer token travels in plaintext on the wire.

**Network default (v1.0)**: the server binds `127.0.0.1` (loopback) and is not reachable off-host. Binding any other address requires `--auth-token <file>` or the explicit `--insecure-allow-no-auth` override — a non-loopback bind with no token is a hard startup error (non-zero exit). The published Docker image binds `0.0.0.0`, so `docker run` without a token fails with that message: pass `--auth-token` (mount the token file), or append `--insecure-allow-no-auth` to run open on purpose.

**Token rotation (current limitation)**: editing the `--auth-token` file requires a server restart to pick up the new token. Proper in-flight rotation (mTLS or JWT) is not yet implemented.

---

## 3. Health checks

The only queries that bypass authentication are the liveness probes `/health` and `/ready` — they expose no data, and load balancers / Kubernetes probes must reach them without the token. **`STATS` / `SHOW STATS` and `/metrics` return the engine stats snapshot** (keyspace sizes, counts, query telemetry) and follow the token: with `--auth-token` set they require it (send the `AUTH_MAGIC` preamble, or `Authorization: Bearer` for HTTP `/stats`); with no token the server is open anyway. **In one line: authentication applies to everything except the liveness probes.** All wire probes use the V1 frame; clients send the literal query string and receive `[status u8][len u32 BE][JSON body]`.

### `/health` — liveness

```
$ printf '\x01\x00\x00\x00\x07/health' | nc 127.0.0.1 2505 | xxd | head
```

Response: `STATUS_OK` + `{"alive": true}`. Always succeeds while the server process is responding.

### `/ready` — readiness

Response: `STATUS_OK` + `{"ready": true}` if the engine is ready to serve queries; otherwise `STATUS_ERROR` + `{"ready": false, "reason": "...", ...}`.

Current heuristic:
- Sync thread heartbeat (`last_successful_sync_ts_ms`) within 5 s of now → ready.
- `last_successful_sync_ts_ms == 0` → ready (durability `batched`/`async` modes never tick the timestamp; this is the back-compat case).
- Heartbeat stale > 5 s with `--durability durable` → not ready.

**Known false-503 case under `--durability durable`**: the heuristic above triggers `not ready` whenever `last_successful_sync_ts_ms` is older than 5 s. That timestamp only advances when the sync thread completes a real fsync; during Idle stretches of a workload that has no writes pending (`pending_epoch == synced_epoch`), the timestamp stays flat even though the engine is fully healthy and `heartbeat_count` advances every millisecond. A bursty two-state write workload hits this case routinely — `--durability durable` deployments with bursty writers will see `/ready` returning 503 between bursts despite serving traffic correctly. Operator workarounds while the fix is pending:
- Configure your load balancer / readiness probe to combine the `/ready` body with a positive check against `xyzdb_sync_thread_heartbeat_total` (Prometheus): treat the instance as ready when `heartbeat_count` is advancing even if `/ready` returns 503. This is the recommended readiness pattern.
- Alternatively, set the readiness probe timeout / failure threshold high enough to absorb Idle stretches (e.g. 3 consecutive 503s before marking unhealthy) so a single Idle window does not deroute traffic.
- For `--durability batched` / `async` the timestamp == 0 back-compat clause already covers this; this only affects `--durability durable`.

Fix path (open): refine the heuristic to "(`pending_epoch` > `synced_epoch`) AND (now - `last_sync_ts` > 5 s)" — distinguishes "fsync failing" from "no writes pending". Requires exposing `pending_epoch` / `synced_epoch` in `StatsSnapshot` first (tracked).

Refinement candidates (planned):
- `BULKMODE` active → not ready (currently not exposed via `/stats`; tracked).
- `compact_err_total{keyspace} > N` → not ready.

### Load balancer config

Generic TCP health check pointing at port `2505` works in trivial cases but cannot read the JSON response body. For richer probes use any TCP-aware LB that supports custom send/expect (HAProxy `option tcp-check`, AWS NLB target group with TCP_HC, Kubernetes liveness/readiness probes via a sidecar that translates to HTTP).

### Example: HAProxy

```
backend xyzdb
    option tcp-check
    tcp-check connect
    tcp-check send-binary 010000000007 ; V1 + len=7
    tcp-check send /health
    tcp-check expect rstring \"alive\":\\s*true
    server xyzdb1 10.0.1.10:2505 check
```

---

## 4. Backup

> **Snapshots are physical.** They do not survive an on-disk format change between minor versions (a pre-1.0 data directory is already refused at open — see [`docs/releases/v1.0.0.md`](docs/releases/v1.0.0.md) → Compatibility). A minor bump does not *imply* such a change — 1.0 → 1.1 has none — so check that release's Compatibility section rather than assuming either way. **Verify that you can restore before you need to**, and until the logical export (`dump` / `load`) lands ([`ROADMAP.md`](ROADMAP.md)), keep the ability to re-ingest from source.

### Hot snapshot

```bash
# Server must be reachable. XYZDB_TOKEN env var if --auth-token is configured.
xyzdb-cli admin snapshot create nightly-2026-05-09
```

Lands the snapshot at `<server data dir>/snapshots/nightly-2026-05-09/`. Returns the JSON `SnapshotMeta` on stdout. The lock window blocking new writers is reported in `meta.lock_window_us` and is < 100 ms in normal mode (empirically 4–7 ms on a workstation).

The snapshot directory contains:

- `<keyspace>/MANIFEST` (one per keyspace) — copied at snapshot point.
- `<keyspace>/*.sst` — **hard-linked** from the live data dir. POSIX inode reference counting keeps the inode alive even if compaction unlinks the source after the snapshot lock releases.
- `journal.wal` — copied at snapshot point. Captures sealed-but-unflushed memtable writes that recover on restore via WAL replay.
- `snapshot.meta` — JSON sidecar with provenance (timestamp, SST inventory, WAL bytes, lock window, BULKMODE flag).

> **Snapshot under load — resolved before 1.0, so every released build has the fix.** Earlier builds could fail a hot snapshot with a `hard_link` error ("No such file or directory") under sustained writes: `set_compaction_enabled(false)` did not drain in-flight compaction before `live_table_paths()`, so a compaction finishing mid-capture left dangling SST paths (the snapshot dir ended up partial — typically only `spatial/`, no MANIFEST). **Current builds drain in-flight compaction** — it disables compaction and acquires each tree's compaction lock (which waits for the in-flight pass) **before** taking the WAL lock, and holds the guards across the hard-link loop (`crates/turba-engine/src/engine.rs`). The drain happens before the WAL lock, so it does not block writers. Hot snapshots are now safe under sustained load.
>
> **Backup-automation note:** host-side automation (cron/sidecar) invokes `xyzdb-cli admin snapshot …`, so the `xyzdb-cli` binary must be available on the host. If your runtime image ships only `xyzdb-server`, build the CLI first (`cargo build --release -p xyzdb-cli`) before scheduling the job.

### Offline restore

```bash
# Server pointing at <source> must be STOPPED.
xyzdb-cli admin snapshot restore \
    --source /var/lib/xyzdb \
    nightly-2026-05-09 \
    --target /var/lib/xyzdb-restored
```

Restore is offline by design (no server contact). The CLI hard-links SSTs, copies MANIFEST + WAL, and prints the next-step instruction. After restore, point a fresh `xyzdb-server --path /var/lib/xyzdb-restored ...` at the new dir; the engine recovers normally (WAL replay during `Engine::open`).

### Same-filesystem requirement

Hard-links cannot cross POSIX mount points. The CLI fails fast with a clear `cross-filesystem` error if `--target` is on a different mount than `<source>/snapshots/<name>/`. To restore across mounts, copy the snapshot dir manually (`cp -a` or `rsync -a`) onto the same mount as the target, then run the restore.

### Recovery contract

- **RPO** (data loss on engine kill): the snapshot captures every write acknowledged by the engine prior to the snapshot lock acquisition + the in-flight WAL up to the captured offset. Writes ack'd AFTER the lock acquires are NOT in the snapshot.
- **RTO** (restore wall-clock): hard-link is sub-ms per SST + one WAL copy. For a typical data dir with 100 SSTs, restore completes in < 1 s. Engine open + WAL replay on the restored target then takes the same time as a normal cold start.

### BULKMODE caveat

If the snapshot is taken while compaction is disabled on any tree (`xyzdb-cli admin bulkmode on` is active), the engine forces `flush_sealed()` inside the snapshot lock to keep the snapshot consistent — `WriteBatch::commit` skips WAL writes in BULKMODE and the captured WAL would otherwise be missing recent batches. This extends the lock window beyond the < 100 ms gate; `meta.bulkmode_at_capture` is `true` in that case.

**Operator recommendation**: pause bulk loads (`xyzdb-cli admin bulkmode off` + `xyzdb-cli admin compact`) before snapshotting in BULKMODE-active deployments.

### Retention

xyzDB does NOT auto-prune old snapshots. Operators manage `snapshots/<name>/` directories manually:

```bash
ls -la /var/lib/xyzdb/snapshots/
rm -rf /var/lib/xyzdb/snapshots/nightly-2026-05-01  # delete a specific snapshot
```

The `rm` is safe even while the engine is running — the SST hard-links are reference-counted, so removing the snapshot dir just decrements the inode count; the engine's view of the same SSTs is unaffected.

### Common patterns

**Daily backup at 03:00 via cron** (server runs as `xyzdb` user):

```cron
0 3 * * * XYZDB_TOKEN=$(cat /etc/xyzdb/token) /usr/local/bin/xyzdb-cli admin snapshot create "daily-$(date +%Y%m%d)"
```

**Backup retention** (keep last 7):

```bash
cd /var/lib/xyzdb/snapshots
ls -t -d daily-* | tail -n +8 | xargs -r rm -rf
```

**Test restore offline** (verify a backup is restorable without affecting prod):

```bash
xyzdb-cli admin snapshot restore --source /var/lib/xyzdb daily-20260509 --target /tmp/restore-test
xyzdb-server --path /tmp/restore-test --port 12345 &  # different port
# poke around with a separate xyzdb-cli session, then kill + rm -rf /tmp/restore-test
```

---

## 5. Monitoring

### Prometheus scraping

`/metrics` exposes the Prometheus exposition format on the same TCP port `2505`. When `--auth-token` is set it **requires the token** (like `STATS`; see §3) — give the scraper an `authorization` bearer credential in its `scrape_config`. With the default loopback bind, `/metrics` is reachable only from the same host: run the scraper/sidecar locally, or bind a reachable address (which then also requires the token).

The endpoint speaks the xyzDB V1 wire frame (not raw HTTP). Stock Prometheus does not support binary framing; a thin sidecar / adapter is required for now. Two patterns:

**Pattern A — `xyzdb-cli admin metrics` (reserved)**: a CLI wrapper that opens a V1 connection and prints the body to stdout. Compose with `node-exporter`-style textfile collector. Not yet shipped.

**Pattern B — small helper TCP client + HTTP shim**: a 30-LOC sidecar (`socat` + a shell wrapper, or a tiny Python/Go scraper) that connects to `2505`, sends the V1 frame for `/metrics`, returns the body over HTTP. Most operators land on this pattern.

Until a native OTel push exporter lands, Pattern B is the recommended approach.

### Critical alerts

| Alert | Condition | Severity |
|---|---|---|
| Sync thread heartbeat stale | `time() - xyzdb_sync_thread_last_successful_ts_ms / 1000 > 5` AND `xyzdb_sync_thread_heartbeat_total{}` not increasing | critical (durability degradation) |
| Memory ceiling | `xyzdb_process_vmrss_bytes / cgroup_memory_max > 0.85` | warn → critical at 0.95 |
| Compaction error climbing | `rate(xyzdb_keyspace_compact_err_total[5m]) > 0` | critical |
| `/ready` returning 503 | scrape /ready and pattern-match `"ready": false` for ≥ 60 s | critical |
| Ghost LRU thrashing | `rate(xyzdb_ghost_auto_dedup_lost_total[5m]) > N` (operator-tuned threshold) | warn |
| Block cache miss rate spike | `rate(xyzdb_block_cache_misses_total[1m]) / rate(xyzdb_block_cache_hits_total[1m]) > 5` sustained | warn (cold workload or eviction storm) |
| **Engine invariant guard fired** | `xyzdb_invariant_level_overlap_total > 0` (per-keyspace breakdown in `xyzdb_invariant_level_overlap_by_keyspace_total`) | **critical — engine bug** |
| **Duplicate anchor prevented after recovery** | `xyzdb_invariant_anchor_bloom_false_negative_total > 0` | **critical — the post-recovery bloom defect is live here** |
| Running in post-recovery armed mode | `xyzdb_recovered_from_wal == 1` | info (explains slower anchored writes until restart) |
| TLS handshake failure rate | derive from server WARN log (no metric yet — registered for refinement) | warn |
| Gravity keel omitted | `/stats` → `keel_health[].omit_ratio` rising on a lobe (or the one-shot server WARN "gravity keel omitted above threshold") | warn (silent scoped-recall degradation) |

> The three `xyzdb_invariant_*` series are **correctness signals, not capacity metrics** — page, do not tune. They are emitted for every keyspace even at zero: a missing series is indistinguishable from a scrape gap, and "we did not look" must not read like "it did not happen".
>
> **This paragraph is the source for what the guards mean.** The same distinction is
> needed in three other places and was wrong in all of them at once; the CHANGELOG
> entry and `docs/mcp-integration.md` now point here instead of restating it. The MCP
> `stats` tool description is a deliberate exception — an agent reading it cannot follow
> a pointer — so it carries its own wording and has to be updated with this.
>
> They do not all mean the same thing when non-zero, and the difference decides what you do. `xyzdb_invariant_level_overlap_total` counts a state the read path assumes impossible — point reads can miss keys a scan still finds, so that one is an engine bug to report. `xyzdb_invariant_anchor_bloom_false_negative_total` counts a duplicate the guard **prevented**: the guard worked and no data was harmed, but the deployment is meeting the post-recovery bloom defect, so it is worth telling someone rather than worth panicking about.
>
> `xyzdb_recovered_from_wal` is different in kind again: it reports a degraded MODE (an anchor miss is re-confirmed without the bloom for the life of a process that replayed WAL), which is correct but costs a level descent per anchor miss until restart — published so slower writes can be explained instead of guessed.

> **Gravity keel-omit health (#11).** For every lobe with a declared `GRAVITY BY <field>`, `/stats.keel_health` reports `{lobe, keel_present, keel_absent, omit_ratio}`. A PUT that omits the declared field still lands and is recoverable by an unfiltered `SCAN`, but it is **not** co-located and is (correctly) excluded from `WHERE <field> = X` — so a rising `omit_ratio` means scoped queries silently under-recall those records. The server also emits a one-shot structured `warn!` (lobe + field) the first time a lobe's ratio crosses the threshold (`XYZDB_KEEL_OMIT_WARN_RATIO`, default `0.01`). Diagnostic only — the engine never rejects the PUT (heterogeneous lobes legitimately omit gravity on some record types). The fix is on the writer: include the gravity field.

### Reap-cycle log fallback

When the scraper is unavailable, the same data prints to stderr every ~60 s in compact text format (also includes the cgroup-limit 85 % WARN inline). Useful for forensic / pre-prometheus debugging:

```
xyzdb-server ... 2>&1 | grep "reap-cycle:"
```

---

## 6. Incident playbook

Five scenarios. Each one names the symptom you can observe (logs, `/stats` field, alert), the diagnostic step (a `SHOW *` query or `/stats` field), the mitigation, what NOT to do, and when to escalate.

The xyTalk surface accepts these `SHOW` verbs (full list per `crates/xytalk-parser/src/parser.rs::parse_show`): `SHOW LOBES`, `SHOW ANCHORS IN "<lobe>"`, `SHOW GHOSTS`, `SHOW SCAN STATS`, `SHOW PROFILE "<lobe>"`, `SHOW THROTTLE`, `SHOW CACHE`. Plus the JSON dump via `STATS` (or `/stats` over the wire / browser). Run any of them with `xyzdb-cli` REPL or piped:

```bash
echo "SHOW SCAN STATS" | xyzdb-cli --host 127.0.0.1 --port 2505
```

### 6.1 Engine OOM (memory ceiling approached)

**Symptom**.

- `reap-cycle: VmRSS approaching cgroup limit: <X>MB / <Y>MB (≥85%)` in stderr (`crates/engine/src/engine/ghosts.rs:222`). Linux only.
- Prometheus alert `xyzdb_process_vmrss_bytes / cgroup_memory_max > 0.85`.
- In severe cases, Linux OOM-killer terminates the process; restart loop logged by the supervisor.

**Diagnose**.

```bash
echo "STATS" | xyzdb-cli --host 127.0.0.1 --port 2505 | jq '{
  vmrss: .process.vmrss_bytes,
  cgroup_anon: .cgroup.anon_bytes,
  cgroup_file: .cgroup.file_bytes,
  cache_active: (.keyspaces | map_values(.memory.mem_active_bytes)),
  block_cache: .block_cache,
  ghosts_total: .ghosts.total
}'
```

Read the breakdown. Most OOM cases trace to one of:

- `block_cache.weight_bytes` close to the derived cache ceiling (`--memory-budget-mb`/4) AND working set + bloom + index + memtable approach the cgroup limit → memory budget too aggressive.
- `mem_active_bytes` per keyspace large AND `levels.l0` ≥ 8 → flush back-pressure not draining; reads + writes piling on the active memtable.
- `ghosts.total` orders of magnitude larger than expected → auto-promotion runaway (cf. §6.4).
- `cgroup.file_bytes / cgroup.active_file_bytes` dominate VmRSS → OS page cache, not heap. Not actionable from xyzDB; it's the kernel reclaiming on demand.
- A recent or concurrent `ANALYZE "<lobe>"` ran on a large lobe (10 k+ records sampled) → ANALYZE memory burst: VmRSS inflates ~3.2× for ~3 min and leaves a persistent residue of ~60 MB per run. On T6 (2C/8G) this peak crosses 5 GB and can collide with concurrent peak load to trip the 85 % WARN or the OOM-killer. Diagnostic clue: timing of the spike aligns with `analyze_cron` cadence or a manual `ANALYZE` issued from the CLI; baseline VmRSS returns to ~1.6 GB within minutes once the procedure finishes. See §7 ANALYZE cadence guidance for prevention.

**Mitigate**.

- Reduce `--memory-budget-mb` (restart) — see §7 Tuning. The block cache follows at budget/4; there is no separate "percent of RAM" target.
- Switch `--throttle-profile` to `transactional` (lower max writes, draining the pile-up faster).
- If a specific keyspace is the culprit (e.g. `spatial.memory.mem_active_bytes` ≫ others), inspect with `SHOW PROFILE "<lobe>"` to confirm.

**Do NOT**.

- Raise `--memory-budget-mb` to "buy time" — the working set exceeds the cgroup; you'll OOM faster.
- Send `SIGKILL` while writes are in flight if you can avoid it; recovery via WAL replay is correct but doubles startup latency on a hot dataset.

**Escalate** when VmRSS reaches 95 % of cgroup limit AND the breakdown does not point at a configurable knob (cache, throttle, lobe-specific memtable). Ticket: capture the full `STATS` dump + last 5 minutes of reap-cycle log.

### 6.2 Slow query (P99 latency degradation)

**Symptom**.

- Application reports a query latency cliff (sub-ms → 100s of ms or seconds).
- `xyzdb_keyspace_pread_service_time_ms` mass shifting toward the 50–300 ms+ buckets for one keyspace.
- Browser dashboard shows the `Block cache` card miss rate climbing relative to hits.

**Diagnose**.

1. Identify the slow pattern:

   ```
   SHOW SCAN STATS
   ```

   Returns top scan patterns by frequency + their rolling average latency. A pattern whose `avg_latency_ms` exceeds the auto-ghost threshold (`--auto-ghost-min-latency-ms`, default 20 ms) is a candidate for promotion.

2. Verify the route the query took:

   ```
   SHOW PROFILE "<lobe>"
   ```

   Lists field profiles, the anchors registered for the lobe, and the searchable vector field (if any). If the slow query is `WHERE x=k | GROUP BY x | …` and `x` is anchored, the router prefers the primary anchor lookup over a ghost — confirm the anchor is present.

3. Inspect block-cache attribution per keyspace via `STATS`:

   ```bash
   echo "STATS" | xyzdb-cli ... | jq '.keyspaces | map_values({
     hits: .block_cache.hits,
     misses: .block_cache.misses,
     avg_disk_us: .block_cache.avg_disk_read_us
   })'
   ```

   A high `avg_disk_read_us` (> 5 000 µs on SSD, > 50 000 µs on HDD) means physical I/O dominates — cache too small or working set too large.

**Mitigate**.

- Anchor the predicate column if it's a hot equality filter:

  ```
  AUTOANCHOR APPLY "<lobe>"
  ```

  (Or restart with `--auto-ghost-min-latency-ms` lower to make auto-promotion fire on more patterns.)

- Pre-build the ghost the slow query needs (`CREATE GHOST …`) — see `docs/xytalk-spec.md` for the full grammar.
- Raise `--memory-budget-mb` (the block cache follows at budget/4) if `misses` dominate `hits` and VmRSS has headroom (cf. §6.1).

**Do NOT**.

- Drop ghosts during the incident — `DROP GHOST …` while a query routes through it returns errors mid-execution.
- Run `xyzdb-cli admin compact` blindly: major compaction is privileged but holds resources; do it once you have a candidate lobe (§6.3).

**Escalate** when a pattern with `avg_latency_ms > 100` persists after anchor + ghost mitigations OR when block-cache miss/hit ratio stays > 5 sustained across multiple cache windows. Ticket: include the `SHOW SCAN STATS` output + the slow query text + `SHOW PROFILE "<lobe>"`.

### 6.3 Stuck compactor

**Symptom**.

- `xyzdb_keyspace_compact_err_total` > 0 in `/metrics` (alert: `rate(xyzdb_keyspace_compact_err_total[5m]) > 0`).
- `reap-cycle: ... compact_errors=<N>` log line with `N > 0`.
- L0 SST count climbing (`/stats.keyspaces.<ks>.levels.l0`) without compaction reducing it.
- Write back-pressure: the throttle profile drives writes into the degraded band (clients see 429-style stalls).

**Diagnose**.

```bash
echo "STATS" | xyzdb-cli ... | jq '.keyspaces | map_values({
  l0: .levels.l0,
  disk_sst: .disk_sst,
  compact_ok: .compact.compact_ok,
  compact_err: .compact.compact_err,
  major_ok: .compact.major_ok
})'
```

- Identify the keyspace where `compact_err` > 0. The error itself is in the server stderr stream (`tracing::error!("compact failed: ...")`), not in `/stats`.
- Confirm the compactor is alive (not deadlocked) by sampling `compact_ok` twice 60 s apart. If `compact_ok` increments somewhere and `compact_err` only on the offending keyspace, the bg threads are healthy and the failure is keyspace-specific (disk full, permission, corrupted SST).

```bash
df -h /var/lib/xyzdb         # disk space
ls -la /var/lib/xyzdb/<ks>/  # SST inventory; permission/quota check
```

**Mitigate**.

- If disk is full: free space, then trigger major compaction:

  ```bash
  xyzdb-cli admin compact
  ```

  Wakes background workers across all 5 keyspaces (spatial, identity, dictionary, ghost, vectors) and consolidates L0 SSTs. COMPACT also seals + flushes every WAL-backed keyspace — including `vectors`, which is co-committed with `spatial` in one batch — before truncating the WAL; `rotate_journal` verifies this precondition at runtime and refuses with `Error::WalRotatePrecondition` if any keyspace still holds unflushed acked writes (this closed the compact-skips-vectors durability bug).

- If permission/quota: fix the underlying mount, then `admin compact`.
- If a specific SST is corrupted (rare): the manifest atomic-publish invariant (cf. arch.md §11 Principle 5) prevents partial writes, but a hardware-induced corruption may still occur. Restore from snapshot (§4 Backup).

**Do NOT**.

- Manually `rm` SSTs from the data dir — the manifest tracks them and the engine will reject the next open with a corruption error.
- Run `BULKMODE on` to "stop compactions for now" without a clear reason — BULKMODE skips WAL writes, raising RPO risk.

**Escalate** when `compact_err` increments faster than `compact_ok` for > 5 minutes AND the underlying disk/permission cause is not visible from the host. Ticket: include the keyspace name, the last 10 lines of stderr matching `compact`, and the `disk_sst` + `levels` snapshot.

### 6.4 Ghost LRU thrashing / auto-promotion runaway

**Symptom**.

- `ghosts.auto.dedup_lost` climbs continuously — multiple threads spawn for the same candidate ghost and lose the dedup race (CPU burned for no result).
- `ghosts.auto.pool_submit_failed > 0` — bounded auto-creator pool is rejecting candidates because the channel is full (sustained spawn rate exceeds capacity).
- `SHOW GHOSTS` lists hundreds of `auto_<lobe>_<hash>` ghosts; `ghosts.total` order-of-magnitude higher than the schema-declared count.

**Diagnose**.

```bash
echo "STATS" | xyzdb-cli ... | jq '.ghosts.auto'
# expected fields: candidate_total, candidate_spawn, dedup_lost, singleflight_skipped, create_failed_other, pool_submit_failed
```

```
SHOW SCAN STATS
```

If a single scan pattern fires hundreds of times per second AND its rolling average latency exceeds `--auto-ghost-min-latency-ms`, the engine fires `maybe_create_ephemeral_ghost` once per slow scan. A **single-flight gate** (since v0.3.2, `crates/engine/src/ghost_pool.rs`) de-duplicates concurrent creations of the same ghost, and a bounded worker pool (`clamp(cpus/2, 1, 4)`) caps the build work — duplicates of one pattern no longer race. A sustained storm of *distinct* hot patterns can still churn ephemeral ghosts; the mitigations below apply to that case.

**Mitigate**.

- Raise the auto-promotion gate to dampen the firing rate:

  ```bash
  # restart server with:
  xyzdb-server ... --auto-ghost-min-latency-ms 100.0  # default 20.0; raises bar
  ```

  Effectively requires patterns to be 5× slower before the engine considers promotion.

- Pre-declare the ghost the workload wants. A `CREATE GHOST …` registers it in the schema and the auto-promoter never fires for that pattern.
- If the promoter is helping but creating clutter (lots of small auto ghosts), `DROP GHOST …` the unused ones manually after a calm window.

**Do NOT**.

- Set `--auto-ghost-min-latency-ms 1e9` in production unless you have validated that ghost-less performance is acceptable for that workload — disabling auto-promotion outright trades CPU for tail latency.
- Restart the server to "reset" auto-ghost state during the spike — Ephemeral ghosts are in-memory only and disappear at shutdown, but the underlying scan pattern recurs and the spike resumes within minutes.

**Escalate** when `dedup_lost` / `ghost_pool_dropped_full_count` rate stays > 100/s for more than 5 minutes AND `--auto-ghost-min-latency-ms` raise + pre-declared ghost coverage do not flatten it (a high drop rate means the bounded pool is shedding load — the single-flight is working, but distinct patterns are arriving faster than the pool builds them). Ticket: capture `ghosts.auto`, the top 10 `SHOW SCAN STATS` patterns, and the active `--auto-ghost-min-latency-ms`.

### 6.5 Durability degradation Sentinel (sync thread heartbeat)

**Symptom**.

- `/ready` returns `503 Service Unavailable` with body `{"ready": false, "reason": "sync_thread heartbeat stale", ...}`.
- Prometheus alert `time() - xyzdb_sync_thread_last_successful_ts_ms / 1000 > 5` while `xyzdb_sync_thread_heartbeat_total` is **not** climbing → thread dead.
- Or the heartbeat is climbing but `last_successful_sync_ts_ms` stays flat → thread alive, every fsync failing (covered by regression tests).

**Diagnose**.

```bash
echo "STATS" | xyzdb-cli ... | jq '.sync_thread'
# {last_successful_sync_ts_ms, heartbeat_count}
```

Sample twice, ~10 s apart:

- `heartbeat_count` advances AND `last_successful_sync_ts_ms` advances → healthy (`/ready` returns 200).
- `heartbeat_count` advances, `last_successful_sync_ts_ms` flat, **`dmesg` / `mount` clean AND `--durability durable` configured AND workload has no writes pending** → **NOT fsync failing, this is the false-503 case**. The timestamp only advances on actual fsyncs; during Idle stretches of a bursty workload, no writes pending means no fsync, means no timestamp advance — engine is fully healthy. Cross-check by sampling `pending_epoch` (when exposed) or simply confirm `dmesg` / `mount` are clean and the workload is currently idle/bursty. No action required; see the §3 false-503 caveat for the operational workaround.
- `heartbeat_count` advances, `last_successful_sync_ts_ms` flat, **`dmesg` shows disk/filesystem errors OR `mount` shows read-only** → fsync failing every cycle. Most common cause: the data dir filesystem went read-only (failed disk, quota, mount remount). Check `dmesg` and the data dir mount:

  ```bash
  mount | grep "$(dirname /var/lib/xyzdb)"
  dmesg | tail -50
  ```

- Both flat → thread dead. Server stderr should contain a panic trace (`tracing::error!` from the sync loop). Restart is required.

**Mitigate**.

- For "fsync failing" (read-only fs / disk full): fix the underlying filesystem, then the next group-commit cycle ticks the timestamp normally (no restart needed in many cases — verify `last_successful_sync_ts_ms` advances within 5 s of the fix).
- For "thread dead": stop and restart the server. Recovery is via WAL replay (the engine re-opens, replays unflushed writes, resumes group commit). The drop chain on graceful shutdown is best-effort; cf. §9 Decommission for the actual semantic.
- For systemic durability concern, switch to `--durability batched` temporarily (timer-based fsync at `--batch-interval ms`; trades RPO for liveness). Restart required.

**Do NOT**.

- Pretend a stale heartbeat is benign. Acknowledged writes that haven't been fsync'd survive the OS page cache but are lost on power-cut. The durability contract (see `docs/architecture.md` §9) is precisely what must hold.
- `SIGKILL` the server during a stale-heartbeat window if you can avoid it — write loss = (writes ack'd since the last successful fsync). For `--durability durable`, that's typically < 100 ms of writes; for `--durability batched`, it's `--batch-interval` worth.

**Escalate** when `last_successful_sync_ts_ms` stays flat > 30 s OR the fix-up does not restart the heartbeat within 60 s. Ticket: full `/stats.sync_thread`, last 50 lines of stderr, `mount` + `dmesg`, and the configured `--durability` mode.

### 6.6 Snapshot under load — resolved before 1.0

The non-deterministic `hard_link` failure when creating a hot snapshot under sustained writes (a race between disabling compaction and capturing live SST paths) is **fixed**, and the fix predates 1.0, so every released build carries it: snapshot creation drains in-flight compaction before capturing the SST paths (see §4 Backup). You should not see it; if a snapshot still fails, treat it as a filesystem problem (permissions, disk full, read-only mount) — check `df -h <data dir>`, `ls -la <data>/snapshots/`, and `dmesg | tail`.

### 6.7 Disk space not reclaimed after a crash — run COMPACT

**Symptom**: after an ungraceful stop (SIGKILL, OOM-kill, power loss) the data directory is larger than the logical dataset warrants, and a plain restart does **not** shrink it.

**Cause (by design)**: a compaction interrupted mid-flight can leave orphan `*.sst` files on disk (an output already renamed but not yet in the MANIFEST, or inputs not yet unlinked). **Recovery on restart does not sweep them** — startup only deletes torn `*.sst.tmp` debris and replays the WAL; orphan `.sst` files are simply ignored (not in the MANIFEST) and stay on disk. The orphan sweep (`cleanup_orphan_ssts`) runs **only at the end of a major compaction**, so the space is reclaimed by the next `COMPACT`, not by the restart alone.

**Action**:
```bash
xyzdb-cli admin compact     # sweeps orphaned SSTs + fully merges; reclaims the space
```
Data is never at risk either way: acked writes survive the crash (WAL replay), and `NEAREST`/reads are correct before and after — this is purely on-disk footprint hygiene. In measurement (append-only + update workloads), the crash-induced overhang was negligible (≤0.3 MiB / <0.2 %) because the vulnerable window is tiny, and the post-COMPACT footprint does **not** ratchet across repeated crashes. Run COMPACT after a crash only if `df`/footprint shows a gap you want back; the space is otherwise reclaimed by the next scheduled major compaction.

---

### 6.8 Memory spike from an unbounded `FIND` / `PULL`

**Symptom**: a single request drives a large resident-memory spike and sustained CPU — often a `FIND` or `PULL` with no `LIMIT` over a large lobe.

**Cause (by design)**: `FIND` and `PULL` have no default row cap and no wall-clock budget — unlike `SCAN` (`SCAN_LIMIT_DEFAULT = 1000`) and `NEAREST` (`--nearest-budget-ms`). `FIND` without a `LIMIT` materializes the whole matching set into one response; `PULL` caps only traversal depth (`MAX_PULL_DEPTH = 10`), not cardinality. On a lobe with millions of records that is a multi-GB allocation for one query. A default cap plus a per-query budget are tracked for a 1.0.x release.

**Action** (until then):
```bash
# Always bound interactive FIND/PULL over large lobes with an explicit LIMIT;
# page with CURSOR instead of materializing the whole set.
FIND "big" WHERE field = "x" LIMIT 100
```
- Set a memory budget (`--memory-budget-mb`, §7) so a spike is bounded by the cgroup rather than the host: an oversized query then fails its own allocation instead of taking the process down.
- If untrusted clients can reach the port, put a connection/rate limit in front (reverse proxy) — the engine has no built-in connection cap — and keep the default `--auth-token` requirement.

---

## 7. Tuning

### Memory budget (`--memory-budget-mb`)

`--memory-budget-mb` (env `XYZDB_MEMORY_BUDGET_MB`) is the single memory knob. The block cache is DERIVED as budget/4, clamped to [32 MiB, 2 GiB]; there is no separate cache flag to tune. When the flag is unset the budget falls back to the cgroup memory limit (Linux; cgroup only, never physical RAM), then to a 1 GiB default. `--cache-size` remains only as a deprecated, hidden override (warns when used) — size memory via the budget. Raise `--memory-budget-mb` when:

- `xyzdb_block_cache_misses_total` is climbing faster than hits in steady state.
- Working set (typical hot lobe + index + bloom bytes) exceeds `xyzdb_block_cache_capacity_bytes`.

Lower it when memory pressure is observed (`xyzdb_process_vmrss_bytes / cgroup limit > 0.85` repeatedly, or reap-cycle WARN at 85 %).

### Throttle profile (`--throttle-profile`)

Default: `balanced` (L0 degraded > 8, critical > 16, sealed-stall > 3, max writes 8K → 2K under degradation).

- `transactional`: tighter limits (5K → 1K writes/s) for OLTP workloads where p99 latency matters more than throughput.
- `analytical`: loose limits (writes ≤ MAX → 10K under critical). Suited to bulk analytical writes that tolerate brief stalls.
- `bulk`: throttle effectively disabled. Use during one-shot bulk loads; switch back to `balanced` after.
- `maintenance`: relaxed read/write limits during off-hours background work.

### RecordCache (`--record-cache-size`)

`0` = disabled (default). When > 0 (megabytes), records loaded via `INCACHE` are kept in a separate in-memory map for predictable single-digit µs reads — operates as a Redis replacement for hot operational data. Enable when `xyzdb_block_cache_hits_total` for a specific lobe shows variance even after cache warmup. Memory cost = configured budget on top of the derived block cache.

### Router behaviour

The router prefers the **primary anchor lookup** over a ghost when the query has an `Eq` predicate on an anchored column. No tunable; it's automatic. If you observe Q2-style queries (`WHERE rfc=X | GROUP BY rfc | AGGREGATE`) that should hit a ghost but instead go through `Primary`, verify the anchor on `rfc` is registered for the lobe. The router fallback is by design.

### Block cache lane admission (`--block-cache-lane-admission`)

**Status: experimental, disabled by default.**

When enabled, `Compaction` and `Flush` block-misses do NOT insert into the BlockCache — they still benefit from cache hits warmed by user reads, but they don't compete for capacity. Intent: prevent compaction churn from evicting user-warm blocks.

- **Default `disabled`** because an A/B microbench measured 0 % improvement in user-side hit rate under quick_cache 0.6's S3-FIFO eviction (which already protects hot user blocks from cold compaction churn).
- The policy plumbing + admission counters stay operational; toggle with `--block-cache-lane-admission enabled` to opt in.
- **When to enable**: cache-pressure deployments (track via `xyzdb_block_cache_misses_total{keyspace="spatial"}` rising during compaction windows) where `xyzdb_block_cache_hits_total{keyspace="spatial"}` drops concurrently. Validate empirically by toggling for one cache window and observing.
- **Diagnostic counters** (always available regardless of flag): `xyzdb_block_cache_admission_total{lane,outcome}` reports admitted vs skipped per lane. With the flag `disabled`, `skipped` stays at 0 across all lanes; with `enabled`, `skipped` accumulates for `flush` + `compaction`.
- The default stays `disabled`; whether richer workloads justify flipping it is still unresolved.

### Lane-aware scheduler (`--io-scheduler hdd`) — observability only

**Status: the enforce ladder was retired; the scheduler is observability-only.** The lane-aware scheduler now provides per-lane observability (sliding-window P50, EWMA, outstanding counters, SLO breach detection, cross-lane peak) without throttling. Use `--io-scheduler hdd` for instrumented operation on rotational media; `--io-scheduler ssd` for zero-overhead Passthrough.

What stayed (observability surface in `/stats`):

- Per-lane: `p50_us`, `ewma_p50_us`, `slo_breach_count`, outstanding peak.
- Scheduler-wide: `cross_lane_outstanding_peak` (kernel disk-queue saturation proxy).
- Lane semantics unchanged: `user_io_read`, `writer_durable`, `flush`, `compaction`.

What is gone (v0.5):

- `--xydisk-mode {enforce, observe}` flag — removed; the enforce mode it selected no longer exists.
- `compaction_blocked_us_total` counter — removed. It was the instrumentation for the retired ladder and additionally exhibited an instrumentation bug.
- `current_n_max_compaction()` ladder logic — removed. Compaction is no longer throttled by reader-feedback EWMA.
- `LanedSchedulerConfig::h1_realistic()` preset — removed.

Rationale: an HDD A/B (observe vs enforce) showed the ladder trade-off was net-negative under realistic workloads. The workload ran ×4.4 slower under enforce, write P50 was +40% worse, P99 inflated ×3-7 across all queries, and the engine was idle-bound (CPU 17 % under enforce vs 63 % under observe). The infrastructure pieces are preserved because future placement/scheduling work may benefit from per-lane latency signals — but **without enforcement**.

Migration: scripts that set `--xydisk-mode` must drop the flag (the binary rejects unknown flags). Consumers parsing `scheduler.compaction_blocked_us_total` from `/stats` must drop that field. The remaining `/stats.scheduler.*` fields are unchanged.

### ANALYZE memory — resolved before 1.0

`ANALYZE "<lobe>"` (xyTalk verb, also exposed as `xyzdb-cli admin analyze <lobe>`) samples up to 10 000 records per lobe and computes per-field cardinality + suggestions. Earlier builds inflated VmRSS ~3.2× for ~3 min because dictionary creation re-scanned the whole lobe (`prefix()` materialised every record). **The re-scan was removed before 1.0** (`crates/engine/src/analyze.rs`), so every released build is on the cheap path: the unique values now come from the same sampled records the profile is built from, in a single streaming pass — the burst is gone, and the cost is bounded by the sample size, not the lobe size.

A small residue (~60 MB per run, asymptotic toward glibc arena steady state, not unbounded) can remain. Light guidance:

- **Multi-lobe ANALYZE** (sidecar pattern: `SHOW LOBES | xargs -n1 xyzdb-cli admin analyze`): serialise the runs with a brief sleep between them so the per-run residue settles rather than stacking across 10+ back-to-back lobes.
- A small VmRSS bump whose timing aligns with `analyze_cron` (or a manual `ANALYZE`) and returns to baseline within minutes is ANALYZE residue, not a leak. See §6.1 OOM diagnostic catalogue for the cross-reference.

---

## 8. Upgrade

In-place upgrades are allowed whenever the on-disk format bytes match across the two versions — always within a patch series, and across a minor bump when that minor did not change them (**1.0 → 1.1 did not**, so it is in-place). Cross-format migrations are called out in the corresponding release notes (e.g. the one-time v0.8.0 migration); a binary refuses to open a data dir whose `MANIFEST_VERSION` does not match, so the check below is what decides, not the version number.

A release may still ask for a **one-time action after** the upgrade without changing the format — 1.1 asks for a single `REFRESH GHOST` per aggregate ghost over a lobe that takes upserts (see [`docs/releases/v1.1.0.md`](docs/releases/v1.1.0.md)). Read the release note's Compatibility section as well as the format check.

### 8.1 What "in-place" means

An upgrade between two releases is allowed when **both** on-disk format bytes match across them:

| Constant | Current | Source of truth |
|---|---:|---|
| `MANIFEST_VERSION` (turba-engine manifest header) | `5` | [`crates/turba-engine/src/manifest.rs:65`](crates/turba-engine/src/manifest.rs#L65) |
| `GHOST_META_FORMAT` (ghost metadata record header) | `0x04` | [`crates/engine/src/ghost/mod.rs:270`](crates/engine/src/ghost/mod.rs#L270) |

Relative to the 0.8.x line, the 0.9.x on-disk format widened `SpatialKey` from 22 to 24 bytes — reserving a 2-byte satellite (sub-gravity) axis, always `0` today (`crates/core/src/key.rs`) — and bumped `MANIFEST_VERSION` from 4 to 5. The byte offsets shifted, so data written by a v4 / 22-byte build is rejected on open (`Error::IncompatibleFormat`); there is no in-place widening. Within a 0.9.x patch series both constants stay at the values above; a release that bumps either one ships with explicit upgrade notes rather than a silent format change.

If a binary opens a data dir whose manifest version does not match its compiled `MANIFEST_VERSION`, it returns `Error::IncompatibleFormat { found, expected }` with a clear log line and the engine refuses to start. **There is no in-place format migration** — the operator path across a format bump is re-ingestion (recreate the dataset from source). The separate `xyzdb-cli admin migrate` verb is not a format converter; it rehashes gravity keys (see §8.5).

### 8.2 In-place upgrade procedure

```bash
# 1. Drain at the load balancer (stop sending new connections to this instance).

# 1b. Wait ~30 s post-drain and verify compaction is settled — snapshotting
#     a quiet engine sidesteps the historical snapshot-under-load race
#     (fixed before 1.0, see §6.6); this is belt-and-suspenders. Sampling
#     compact_ok twice ~30 s apart should show no advancement on any
#     keyspace before snapshotting.
echo "STATS" | xyzdb-cli ... | jq '{
  compact_ok: (.keyspaces | map_values(.compact.compact_ok)),
  l0: (.keyspaces | map_values(.levels.l0))
}'
# Wait 30s, re-sample. Expect compact_ok unchanged across the two samples.

# 2. Take a snapshot — this is the rollback artefact. Cf. §4 Backup,
#    §6.6 mitigations if the create returns the I/O error symptom.
XYZDB_TOKEN=$(cat /etc/xyzdb/token) \
  xyzdb-cli admin snapshot create "pre-upgrade-$(date +%Y%m%d-%H%M)"

# 3. Stop the running server. SIGTERM/Ctrl-C trigger a
#    graceful drain + flush + clean-shutdown marker (cf. §9); a SIGKILL
#    still recovers via WAL replay on next start.
sudo systemctl stop xyzdb        # or: kill <pid>

# 4. Replace the binary.
sudo cp /tmp/xyzdb-server-new /usr/local/bin/xyzdb-server
sudo chmod +x /usr/local/bin/xyzdb-server

# 5. Restart. The engine reopens, replays the WAL, and resumes serving.
sudo systemctl start xyzdb

# 6. Watch the boot log. Expect:
#      INFO Opening database at: /var/lib/xyzdb ...
#      INFO xyzDB server listening on 0.0.0.0:2505
#    If you see:
#      ERROR Engine open failed: incompatible format: found=N expected=M
#    the upgrade crossed a format-version bump — STOP and follow §8.4.

# 7. Probe /ready before re-opening the load balancer:
echo "/ready" | xyzdb-cli --host 127.0.0.1 --port 2505
# expect: STATUS_OK + {"ready": true}
```

WAL replay during step 5 takes the same wall-clock as a normal cold start (single-digit seconds per GiB of unflushed WAL on SSD). You can monitor it via the boot log; the engine emits per-keyspace `flushed_seqno` once replay completes.

### 8.3 Rollback to the previous release

The new binary failed `/ready` or surfaced incorrect behaviour after step 7. Procedure:

```bash
# 1. Stop the failing server.
sudo systemctl stop xyzdb

# 2. Restore the snapshot taken in step 2 of §8.2 — offline.
xyzdb-cli admin snapshot restore \
  --source /var/lib/xyzdb \
  "pre-upgrade-<timestamp>" \
  --target /var/lib/xyzdb-restored

# 3. Replace the binary with the previous version.
sudo cp /usr/local/bin/xyzdb-server.prev /usr/local/bin/xyzdb-server

# 4. Repoint the server at the restored data dir (or swap mount points
#    so the path stays /var/lib/xyzdb).
sudo systemctl edit xyzdb     # adjust --path if needed
sudo systemctl start xyzdb
```

The snapshot captured every write acknowledged before the snapshot lock acquired (cf. §4 Backup recovery contract). Writes accepted between the snapshot and the failed upgrade are lost — that is the rollback RPO.

### 8.4 Format-version mismatch (rejected open)

Symptom: `Error::IncompatibleFormat { found: <N>, expected: <M> }` on engine open.

This happens when the binary's compiled `MANIFEST_VERSION` (or `GHOST_META_FORMAT`) does not match what's on disk. It should NOT occur on a patch upgrade — the constants are pinned within a minor line, and 1.0 → 1.1 has no format change. If it does occur:

- Confirm the binary version: `xyzdb-server --version`.
- Confirm the data dir was not touched by a different release: `ls -la /var/lib/xyzdb/spatial/MANIFEST` (file headers contain the version byte at offset 0, dump with `xxd | head -1`).
- If the mismatch is real, the supported path is **re-ingestion**: stop the server, restore from the most recent snapshot taken with the matching binary version, and re-apply any writes from the application's source-of-truth (audit log, upstream queue, etc.).
- File a ticket with the binary versions on both sides and the manifest header dump.

### 8.5 What is NOT supported

- **Automatic in-place cross-format migration.** When `MANIFEST_VERSION` changes between releases the upgrade is re-ingestion (recreate from source) — not a silent in-place rewrite. `xyzdb-cli admin migrate <lobe>` / `--all` is a different tool: it rehashes gravity keys to the canonical value-only convention and is crash-safe, idempotent, and re-runnable (committed in windows). It is not a record-format converter.
- **Concurrent old + new binaries on the same data dir**. The on-disk MANIFEST and WAL are not multi-writer safe across versions.
- **Online (zero-downtime) upgrade** within a single instance. Take the instance out of rotation, do the upgrade, put it back. For high-availability deployments use two instances with snapshot-based copy (§1.5 Single-host vs distributed).

---

## 9. Decommission

How to take an xyzDB instance out of service cleanly, with the data dir in a state that either re-opens cleanly elsewhere or can be discarded safely.

### 9.1 Shutdown semantics (read this first)

xyzDB's "shutdown contract" lives in two `Drop` implementations:

- [`Engine::drop`](crates/engine/src/engine/mod.rs#L325) — persists the field registry and total-writes counter.
- [`TurbaEngine::drop`](crates/turba-engine/src/engine.rs#L1294) — signals the WAL janitor to stop and joins it, syncs the journal best-effort, then for each of the five keyspaces (`spatial`, `identity`, `dictionary`, `ghosts`, `vectors`) seals the active memtable, notifies background workers, and joins them. The bg workers flush sealed memtables and run any pending compactions before exiting.

The server binary installs a signal handler (`shutdown_signal()` in `crates/server/src/main.rs`) and the accept loop races `listener.accept()` against a shutdown signal via `tokio::select!`. On signal, strictly in order and never in parallel with accept: stop accepting, drain in-flight connections (tracked in a `JoinSet`, bounded to 5 s), abort any stragglers, then run `engine.graceful_shutdown()` (writes the clean-shutdown marker, seals + flushes every tree, reclaims the WAL — the same work `Drop` would do), then `std::process::exit(0)`. Concretely:

| Signal | What happens | Clean shutdown runs? |
|---|---|---|
| `SIGINT` (Ctrl-C) | Caught: graceful drain + flush + clean marker, then `exit(0)` | Yes |
| `SIGTERM` (Unix) | Caught: graceful drain + flush + clean marker, then `exit(0)` | Yes |
| `SIGKILL` | Process killed by kernel — no handler possible | No |

Under `SIGINT`/`SIGTERM` the engine exits clean, so the next start replays an empty (reclaimed) WAL. Under `SIGKILL` recovery is via **WAL replay on the next start**: the engine open path replays unflushed-but-WAL'd writes; the contract holds because every acknowledged write was fsync'd to the WAL before the ack (under `--durability durable`) or scheduled for fsync within `--batch-interval ms` (`--durability batched`).

Subprocess-based crash-recovery tests (`crates/turba-engine/tests/crash_recovery.rs`) exercise the `SIGKILL` path with real kills and assert correctness; the clean `SIGTERM` path has an end-to-end test (`crates/server/tests/graceful_shutdown_e2e.rs`, Unix) that asserts `exit(0)` and that the clean-shutdown marker was written.

> **Graceful signal handling is wired** (before 1.0, so every released build has it): `SIGINT`/`SIGTERM` trigger the drain + drop chain instead of relying on WAL replay.

### 9.2 Decommission procedure

The "best decommission" today is a clean WAL replay on next start, paired with a final snapshot for portability/rollback. Sequence:

```bash
# 1. Drain at the load balancer / Kubernetes service. New connections
#    stop arriving; in-flight requests drain or hit the IDLE_TIMEOUT
#    (300 s; xyzdb-server/src/connection.rs).
kubectl scale deploy xyzdb --replicas=0      # or: remove from LB

# 1b. Wait ~30 s post-drain and verify compaction is settled — same
#     precaution as §8.2. Snapshot is reliable on a quiet engine; under
#     in-flight compaction it could historically return the I/O error
#     symptom from §6.6 (fixed before 1.0).
echo "STATS" | xyzdb-cli ... | jq '{
  compact_ok: (.keyspaces | map_values(.compact.compact_ok)),
  l0: (.keyspaces | map_values(.levels.l0))
}'
# Wait 30s, re-sample. Expect compact_ok unchanged across the two samples.

# 2. Final snapshot. This is the artefact you keep for the next 30+ days
#    in case forensics or partial restoration is needed later. If the
#    create returns the I/O error, follow §6.6 mitigations
#    (immediate retry, or stop+tar fallback for hard cases).
XYZDB_TOKEN=$(cat /etc/xyzdb/token) \
  xyzdb-cli admin snapshot create "decommission-$(date +%Y%m%d-%H%M)"

# 3. Stop the server. SIGTERM is the conventional signal;
#    it triggers a graceful drain + flush + clean-shutdown marker
#    (see §9.1 above), so the next start replays an empty WAL. A SIGKILL
#    still recovers via WAL replay on the next start of any instance
#    pointed at this data dir.
sudo systemctl stop xyzdb     # or: kill -TERM <pid>

# 4. Verify the data dir is in a "re-openable" state. This is the
#    decommission acceptance check — if a fresh server cannot open
#    the dir cleanly, you are NOT decommissioned, you are corrupted.
xyzdb-server --path /var/lib/xyzdb --port 19999 &
PID=$!
sleep 3
echo "/ready" | xyzdb-cli --host 127.0.0.1 --port 19999
# expect: STATUS_OK + {"ready": true}
kill $PID
```

Step 4 is the `subprocess_crash_recovery`-style proof that the WAL replay produced a consistent state. Do not skip it.

### 9.3 Snapshot retention before disposal

Before deleting the data dir, copy the final snapshot off the host. The snapshot directory contains:

- Hard-linked SSTs from the live data dir.
- `MANIFEST` per keyspace.
- `journal.wal` capturing sealed-but-unflushed writes at snapshot time.
- `snapshot.meta` JSON sidecar with provenance.

```bash
tar -C /var/lib/xyzdb/snapshots/decommission-<timestamp> \
    -czf /backups/xyzdb-decommission-<host>-<timestamp>.tar.gz .
# Verify integrity off-host before continuing.
```

A snapshot tarball is **portable across mount points** (the hard-linked SSTs are dereferenced when packed). A live data dir is NOT (cf. §4 same-filesystem requirement).

### 9.4 Final disposal

After step 9.3, the on-disk data dir is no longer needed. Standard secure-delete practices apply for the host filesystem. xyzDB does NOT encrypt at rest; the data dir contains plaintext SSTs and the WAL contains plaintext mutations — deploy on an encrypted volume if at-rest confidentiality is required. If your deployment regulations require zeroisation, use the OS tool of choice (`shred -uvz`, encrypted-volume wipe, or destroy the underlying device).

```bash
# Standard removal — sufficient for most internal deployments.
sudo rm -rf /var/lib/xyzdb

# Higher assurance — overwrite then remove (slow on large dirs).
sudo find /var/lib/xyzdb -type f -exec shred -uvz {} +
sudo rm -rf /var/lib/xyzdb
```

The `--auth-token` file (if used) lives outside the data dir at the path passed to `--auth-token`. Treat it as a secret and remove it explicitly:

```bash
sudo shred -uvz /etc/xyzdb/token
```

TLS certs and keys (`--tls-cert`, `--tls-key`) follow your existing PKI rotation/disposal policy; xyzDB does not own that lifecycle.

### 9.5 What is NOT a decommission contract

- **Bounded, not unbounded, in-flight drain.** On `SIGINT`/`SIGTERM` the server drains in-flight connections for up to 5 s, then aborts stragglers before flushing. Committed writes are WAL-durable, so an aborted straggler loses only its in-flight response, not acknowledged data. Under `batched` or `async` durability a signalled stop can still lose up to `--batch-interval ms` of not-yet-fsync'd writes; `--durability durable` fsyncs per group commit and bounds the loss to < 100 ms.
- **No "preserve cache state across restart".** The block cache, page cache, and bloom-load warm-up are rebuilt from scratch on next open. Plan first-query latency accordingly when re-pointing a fresh instance at the data dir.
- **Graceful signal handling is present.** `SIGINT`/`SIGTERM` run `engine.graceful_shutdown()` (drain → flush → clean marker → `exit(0)`), not just the `Drop` chain on normal return. `SIGKILL` still bypasses everything and recovers via WAL replay.

---

> **Document policy**: each block of the v0.4 cycle that adds a flag, endpoint, or operational behaviour must update the relevant section in the same commit. Sections marked `[Bloque N — TBD]` will be filled by their owning block; do not pre-fill.

> **Source of truth for technical detail**: this runbook summarises operational semantics. For internals consult [`docs/architecture.md`](docs/architecture.md). For language surface consult [`docs/xytalk-spec.md`](docs/xytalk-spec.md). When this doc and architecture/spec disagree, architecture/spec wins.
