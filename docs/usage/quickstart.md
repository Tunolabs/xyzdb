# xyzDB — Quickstart

Get xyzDB running and write your first six xyTalk statements in five minutes. The tutorial below is sequential — each step builds on the previous.

For detailed operational guidance see `usage/reference.md`. For the complete language surface see `xytalk-spec.md` (organized by usage tier; this quickstart covers Tier 1).

---

## Prerequisites

- Rust stable (`cargo`, `rustc`).
- Optional for containerised benchmarks: Docker (OrbStack on macOS works out of the box).

## Build

```bash
git clone https://github.com/Tunolabs/xyzdb.git
cd xyzdb
cargo build --release
```

Four binaries are produced:
- `xyzdb-server` — the TCP server.
- `xyzdb-cli` — rustyline REPL that speaks the V1 text protocol. Also exposes the `xyzdb-cli admin <verb>` subcommand for operator-grade commands (`compact` / `analyze` / `bulkmode` / `migrate` — single-shot, exits after one round-trip).
- `xyzdb-mcp` — Model Context Protocol server (see *Use from an MCP-aware agent* below).
- `xyzdb-bench` — internal bench (one-off workloads).

The cross-engine benchmark harness (`native-bench`) lives in a separate workspace under `benchmarks/native`; build it there with `cargo build --release -p native-orchestrator`.

## Start the server

```bash
mkdir -p ./data/xyzdata
./target/release/xyzdb-server \
  --path ./data/xyzdata \
  --port 2505 \
  --storage-profile ssd \
  --durability durable
```

The server binds `127.0.0.1:2505` by default; a non-loopback bind requires `--auth-token` (or an explicit `--insecure-allow-no-auth` for a trusted network). `--storage-profile` accepts `ssd` or `hdd` (tunes block size + bloom bits). `--durability` accepts `durable` (fsync per write), `batched` (fsync every N ms), or `async` (OS decides).

`--memory-budget-mb <MB>` (env `XYZDB_MEMORY_BUDGET_MB`) is the primary memory knob: the block cache is derived from it (`budget / 4`, clamped to `[32 MiB, 2 GiB]`), and ingest self-limits against it — under a tight budget, writes stall for background flush instead of ballooning past the container's memory. Left unset, it falls back to the process's cgroup memory limit on Linux, otherwise a 1 GiB default. (`--record-cache-size` — legacy alias `--hot-cache-size` — is a separate, opt-in budget for the `INCACHE` / `OUTCACHE` RecordCache; see the tutorial below.)

All CLI flags: `xyzdb-server --help`.

---

## Tutorial — your first six statements

Open another terminal. The six steps below introduce the core CRUD + DDL surface (Tier 1 in the spec). Run them in order in the REPL — each builds on the previous.

### Step 1 — Connect

```bash
./target/release/xyzdb-cli --host 127.0.0.1 --port 2505
```

You'll get a `xyzdb> ` prompt. Everything below is typed at that prompt.

### Step 2 — Declare your space

```text
LOBE "clients" HINT="Customer records"
```

A **lobe** is a logical bucket for records that share a domain. Lobes are auto-created on first `PUT`, but explicit declaration with a `HINT` helps you (and your team) document intent before any record exists.

### Step 3 — Add identity

```text
ANCHOR "rfc" UNIQUE IN "clients"
```

An **anchor** is a uniqueness constraint integrated with an O(1) dictionary lookup. After this declaration, every `PUT` writes the `rfc` value into the dictionary keyspace, and `FIND ... WHERE rfc = X` resolves through it instead of scanning the lobe.

Anchor is *constraint + lookup* in a single primitive. You'll see the speed payoff in Step 5.

### Step 4 — Insert

```text
PUT {rfc: "ACME-001", name: "Acme Corp", region: "US-West"} IN "clients"
PUT {rfc: "ACME-002", name: "Acme Subsidiary", region: "EU"} IN "clients"
```

xyzDB auto-generates a LID (128-bit local identifier) for each record. `created_at` and `updated_at` timestamps are stamped automatically. The lobe gets the records, and the dictionary gets the `rfc` entries (because of Step 3).

### Step 5 — Find a single record

```text
FIND "clients" WHERE rfc = "ACME-001"
```

Because `rfc` is an anchor, this is an O(1) dictionary lookup. No lobe scan, no bloom filter — direct hit. Same query without the anchor would fall through to a full lobe scan.

### Step 6 — List many

```text
SCAN "clients" WHERE region = "US-West"
```

`SCAN` iterates the lobe and applies the filter. The first time this query runs, the engine takes the slow path and starts tracking the pattern. If you keep running similar queries, an automatic optimization (a "ghost") materialises in the background and subsequent runs become fast — you do nothing.

For SCAN against very large lobes, see *Paginate large result with cursor* in **Common patterns** below.

---

## How xyTalk routes queries

When you write `FIND`, the engine selects a path automatically:

- `FIND "lobe" WHERE anchor_field = X` → direct dictionary lookup, O(1).
- `FIND "lobe" WHERE gravity_field = X` → bounded range scan over the gravity bucket, ~0.5 ms even on millions of records (the engine uses gravity to skip most of the lobe).
- `FIND "lobe" WHERE other_field = X` → no fast path: `FIND` falls through to a full lobe scan and returns the matches. It works, but without the anchor/gravity speed — declare an `ANCHOR` for O(1) lookups, or use `SCAN` for full-lobe iteration.

When you write `SCAN`, the engine picks the optimal scan source:

- `SCAN "lobe" WHERE gravity = X [...]` → bounded range scan (the same fast path, exposed under a different verb for the iteration use case).
- `SCAN "lobe" WHERE gravity = X AND satellite = Y [...]` → **narrower still**, if the lobe declared `SATELLITE BY <field>`. The satellite axis sub-divides each gravity bucket, so pinning both fields reads one sub-range instead of the whole bucket. Same rows, same order — it only changes how much is read. Opt-in per lobe and declared on an empty lobe; see `reference.md` §2.1.
- `SCAN "lobe" WHERE [...]` matched by an existing ghost → ghost-driven scan, transparent to you.
- `SCAN "lobe" WHERE [...]` not matched by any ghost → full primary scan, with telemetry tracking. If you keep running the same slow query, the engine notices and builds an automatic optimization ("ghost") in the background. Subsequent runs become fast — you do nothing.

You don't choose between `SCAN` and `SCAN GHOST` manually. The engine routes. `SCAN GHOST "name"` is an explicit override available in the Power User tier when you want to force a specific ghost (useful for diagnostics or for benchmarks).

The engine adapts to your usage. You write what you want; the routing happens underneath.

---

## Common patterns

Once you're past the tutorial, these are the recipes that cover ~80% of real workloads. Each one is a single statement. The remaining language surface (PULL, ghosts, pinning, telemetry inspection) is documented in `xytalk-spec.md` Tier 2 and Tier 3.

### Get all related records of one entity

When records share a `*gravity` field, they're co-located on disk. A single `FIND` returns the entire entity in a bounded range scan — no JOIN, no point lookups.

```text
-- Setup: a credit lifecycle co-located by *rfc
PUT {*rfc: "ACME-001", _type: "Credit", monto: 50000, status: "active"} IN "creditos"
PUT {*rfc: "ACME-001", _type: "Installment", amount: 1000, due_date: @"2026-01-15"} IN "creditos"
PUT {*rfc: "ACME-001", _type: "Payment", amount: 1000, paid_at: @"2026-01-14"} IN "creditos"

-- Fetch the entire credit history for one client in one range scan
FIND "creditos" WHERE rfc = "ACME-001"
```

The `*` prefix on `rfc` declares it a gravity field. Records sharing the same gravity value land in the same physical block range. The query is ~0.5 ms even on millions of records.

### Top-N by metric

```text
SCAN "creditos" WHERE status = "active" ORDER BY monto DESC LIMIT 10
```

`ORDER BY` always requires `LIMIT`. The engine uses a bounded min-heap (O(n) scan, O(k) memory where k = LIMIT) — no full sort.

### Paginate large result with cursor

For lobes with more than 1 000 records matching a filter, plain `SCAN` returns the first page plus an opaque cursor token. Pass the token back to fetch the next page.

```text
-- First page
SCAN "creditos" WHERE rfc = "ACME-001" LIMIT 1000
-- → records (1000), cursor = "AQEAAQ...", has_more = true

-- Next page: round-trip the token unchanged
SCAN "creditos" WHERE rfc = "ACME-001" LIMIT 1000 CURSOR "AQEAAQ..."
-- → records (next 1000), cursor = "...", has_more = true | false
```

The `CURSOR` token is opaque — postcard-encoded + URL-safe base64. Don't parse it; round-trip it verbatim. See `xytalk-spec.md` §2.6 for the format details and constraints (cursor + `ORDER BY` and cursor + ghost routing are not yet implemented).

### Count by group

```text
SCAN "creditos" WHERE _type = "Credit" | GROUP BY status | AGGREGATE count(), sum(monto)
```

Aggregates always come at the end of a pipeline starting with `SCAN`. The engine streams: O(1) memory for plain `AGGREGATE`, O(num_groups) for `GROUP BY | AGGREGATE`. If a matching ghost exists, the result is served from pre-computed group state without scanning the lobe at all.

### Update a record in place

```text
FIND "clients" WHERE rfc = "ACME-001" | SET status = "inactive", updated_by = "admin"
```

The pipeline form is the idiomatic update: `FIND` selects, `SET` writes. `updated_at` is stamped automatically.

### Delete records matching a filter

```text
DELETE "creditos" WHERE status = "cancelled"
```

Standalone form available in v0.2.5.1+. The pipeline form `FIND ... | DELETE` also works and is preferred when you want to log or transform the records before deletion.

### Keep hot data in memory

For lobes that need single-digit-microsecond reads (sessions, configuration, hot operational data), load them into the RecordCache:

```text
INCACHE "configuracion"
INCACHE "creditos" WHERE status = "active"      -- subset of a large lobe
OUTCACHE "configuracion"                         -- evict when no longer hot
```

The server must be started with `--record-cache-size N` (MiB) for `INCACHE` / `OUTCACHE` to work. Without it both statements error with `RecordCache not enabled`. The older `--hot-cache-size` flag still works as a deprecated alias.

---

## Use from an MCP-aware agent

Since v0.2.6, xyzDB ships an MCP server (`xyzdb-mcp`) that exposes the database to MCP-compatible clients without those clients needing to learn xyTalk syntax or implement a TCP driver. The agent gets five tools (`stats`, `query`, `list_lobes`, `describe_lobe`, `snapshot`) and three resources (`xyzdb://lobes`, `xyzdb://stats`, `xyzdb://lobes/{name}`). Tool descriptions advertise the xyTalk surface so the model can compose statements directly.

Two deployment modes: `--embed <PATH>` is the canonical single-process subprocess pattern (the MCP process owns the data dir; no separate `xyzdb-server` runs). `--connect <HOST:PORT>` is the multi-process / TUNO-Pro shape (MCP is a TCP client of an existing `xyzdb-server`). Telemetry is privacy-clean by default — statement text never appears in logs, only an `xxh3-64` fingerprint plus first verb. A `--log-statements` development flag adds full TRACE-level logging, gated by a startup guard that refuses non-loopback `--connect` targets.

Bring up against a desktop MCP client:

```bash
cargo build --release -p xyzdb-mcp
```

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "xyzdb": {
      "command": "/absolute/path/to/xyzdb/target/release/xyzdb-mcp",
      "args": ["--embed", "/absolute/path/to/your/data/dir"]
    }
  }
}
```

Restart the MCP client, then prompt the agent with *"List the lobes you can see and describe the first one."* The full reference — every tool's schema, every failure mode, the privacy contract, troubleshooting — lives in [`docs/mcp-integration.md`](../mcp-integration.md).

---

## What to read next

- **`usage/reference.md`** — the full operator manual: server configuration, CLI flags, operational gotchas, tuning.
- **`xytalk-spec.md`** — the complete language specification, organized by usage tier:
  - **Tier 1 — Quickstart**: the 8 verbs covered above (LOBE, ANCHOR, PUT, PUT BATCH, FIND, SCAN, SET, DELETE).
  - **Tier 2 — Common**: LINK (relationships), INCACHE/OUTCACHE (caching), AGGREGATE, SHOW (introspection).
  - **Tier 3 — Power user**: PULL (graph traversal), CREATE GHOST (manual materialised views), PIN/UNPIN, AUTOANCHOR APPLY, SHOW (tuning).
  - **Tier 4 — Operator**: COMPACT / ANALYZE / BULKMODE / MIGRATE (prefer `xyzdb-cli admin <verb>`; the language form stays as a permanent alias).
- **`architecture.md`** — how the engine works internally. Useful before tuning or extending.
- **`releases/v1.1.0.md`** — the current release notes.

## Troubleshooting quick hits

- **`error: Failed to bind to 127.0.0.1:2505`**: another process is already listening. `lsof -i :2505`, kill it, or pass `--port <other>`.
- **`incompatible on-disk format: found version X, this build expects version Y`**: the data directory was written by a different xyzDB version. Either delete and re-ingest, or use a matching binary. See `architecture.md` for the version table.
- **`SCAN` returns only 1 000 records and a `default LIMIT applied` warning in the server logs**: v0.2.5.1+ caps unbounded SCAN at 1 000 rows by default. Add an explicit `LIMIT N` (≤ 10 000) or paginate with `CURSOR` (see *Paginate large result with cursor* in Common patterns above, or `xytalk-spec.md` §2.6).
- **`SCAN ... LIMIT 100000` is rejected**: the hard ceiling is 10 000. Paginate with `CURSOR` for larger result sets.
- **`CURSOR pagination is not supported`** when combined with `ORDER BY` or `AGGREGATE`: `ORDER BY`, `GROUP BY` and `AGGREGATE` each work on their own — it's only their *combination with cursor pagination* that isn't implemented yet. For now paginate with plain `SCAN ... LIMIT N CURSOR "<token>"`.
- **`RecordCache not enabled`** on `INCACHE` / `OUTCACHE`: restart the server with `--record-cache-size N` (in MiB). The legacy `--hot-cache-size` flag still works as a deprecated alias.

---

*For anything beyond the patterns above, read `xytalk-spec.md` (organized by tier) or `usage/reference.md`.*
