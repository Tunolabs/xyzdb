# xyzDB MCP integration guide

`xyzdb-mcp` is a Model Context Protocol server that exposes xyzDB to MCP-compatible clients over stdio. It speaks JSON-RPC 2.0 framed by line-delimited messages — the same surface every other DB MCP server uses today.

The server is single-tenant by design (one client process per server process). Multi-actor TLS+auth deployments (real auth — mTLS or JWT with rotation, over HTTP+SSE) are not yet available.

This document describes the MCP surface as of **1.0**. The tools/resources contract is stable — **5 tools** (`stats`, `query`, `snapshot`, `list_lobes`, `describe_lobe`) + 3 resources (`xyzdb://lobes`, `xyzdb://stats`, `xyzdb://lobes/{name}`).

> **`/stats` schema delta in v0.5.0**: `scheduler.compaction_blocked_us_total` is removed (DEC-V5-12). Consumers parsing it must drop the field. All other `/stats.scheduler.*` fields unchanged (per-lane `p50_us`, `ewma_p50_us`, `slo_breach_count`, `cross_lane_outstanding_peak`).

> **Auth**: `xyzdb-mcp --connect <HOST:PORT>` reads `XYZDB_TOKEN` from its environment and sends it as the bearer-token preamble on every query, so it works against an upstream `xyzdb-server` started with `--auth-token`. Set `XYZDB_TOKEN` in the MCP process to the same token the server was given; leave it unset when the server is open.

---

## Quickstart — desktop MCP client

1. Build the binary:
   ```bash
   cargo build --release -p xyzdb-mcp
   ```

2. Edit `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

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

3. Restart the MCP client. The `xyzdb` server appears in the tools panel; the agent can invoke `stats`, `query`, `snapshot`, `list_lobes`, `describe_lobe`, and read the resources at `xyzdb://lobes`, `xyzdb://stats`, `xyzdb://lobes/{name}`.

4. Try a first prompt: *"List the lobes you can see and describe the first one."* The agent should call `list_lobes` followed by `describe_lobe` on the first lobe.

## Modes

`xyzdb-mcp` runs in exactly one of two modes per invocation; clap rejects passing both.

### `--embed <PATH>`

The MCP process opens the data directory itself (LSM lock holder). Single-process deployment. **This is the canonical single-process (`--embed`) mode.** No separate `xyzdb-server` is needed; the agent's process owns the data.

```json
"args": ["--embed", "/Users/you/xyzdb-data"]
```

### `--connect <HOST:PORT>`

The MCP process is a TCP client of an external `xyzdb-server` already serving the data dir. Multi-process deployment. Use this when the server runs in a separate container or when several MCP processes need to share one data dir.

```json
"args": ["--connect", "127.0.0.1:2505"]
```

The startup probe issues a single `STATS` query to verify reachability. Pass `--no-probe` to skip it (useful in CI when the upstream is intentionally not yet available).

`xyzdb-mcp` warns at startup when `--connect` targets a non-private IP — see [Privacy & telemetry](#privacy--telemetry) below for the full guard list.

## Docker image

A prebuilt image ships the `xyzdb-mcp` binary, so you can run the MCP server
without a Rust toolchain. It is published to the GitHub Container Registry and
listed on the official MCP registry under the name
`io.github.Tunolabs/xyzdb`.

```
ghcr.io/tunolabs/xyzdb-mcp:1.1.0
```

The image is Business Source License 1.1, same as the engine — see
[`LICENSE`](../LICENSE).

The MCP transport is **JSON-RPC 2.0 over stdio**, so the container must run
with an open, attached stdin: `docker run -i` (no `-t`, no `-d`). All logs go
to stderr; stdout carries only MCP framing. There is no default command — pass
`--embed <PATH>` or `--connect <HOST:PORT>` at run time.

### Docker — `--embed`

Bind-mount your data directory to `/data` and open it in embedded mode:

```bash
docker run -i --rm -v /absolute/path/to/your/data/dir:/data \
  ghcr.io/tunolabs/xyzdb-mcp:1.1.0 --embed /data
```

MCP client config:

```json
{
  "mcpServers": {
    "xyzdb": {
      "command": "docker",
      "args": [
        "run", "-i", "--rm",
        "-v", "/absolute/path/to/your/data/dir:/data",
        "ghcr.io/tunolabs/xyzdb-mcp:1.1.0",
        "--embed", "/data"
      ]
    }
  }
}
```

### Docker — `--connect`

Forward to an external `xyzdb-server`. When the server was started with
`--auth-token`, pass the bearer token through `XYZDB_TOKEN` (the driver sends
it as the auth preamble; a non-loopback bind always enforces auth):

```bash
docker run -i --rm -e XYZDB_TOKEN="$XYZDB_TOKEN" \
  ghcr.io/tunolabs/xyzdb-mcp:1.1.0 --connect host.docker.internal:2505
```

MCP client config (the empty-valued `-e XYZDB_TOKEN` passes the variable
through from the client's environment):

```json
{
  "mcpServers": {
    "xyzdb": {
      "command": "docker",
      "args": [
        "run", "-i", "--rm",
        "-e", "XYZDB_TOKEN",
        "ghcr.io/tunolabs/xyzdb-mcp:1.1.0",
        "--connect", "host.docker.internal:2505"
      ]
    }
  }
}
```

`host.docker.internal` reaches a server on the Docker host; use the service
name when the server runs as a container on the same Docker network.

## Tools reference

All five tools return JSON in the MCP standard `CallToolResult.content[0].text` slot. Schemas are advertised via `tools/list`.

### `stats`

Snapshot of engine internals: keyspace stats (memtables, SSTables, compaction counters), block cache, ghosts, sync thread health, process and cgroup memory. Same shape as the `/stats` endpoint on `xyzdb-server`'s TCP port.

No arguments. Sub-50 ms latency on every realistic data dir size.

### `query`

Execute an arbitrary xyTalk statement. Arguments:

| Field | Type | Required | Notes |
|---|---|---|---|
| `statement` | string | yes | Full xyTalk grammar including writes (PUT/SET/DELETE/LINK). |
| `cursor` | string | no | Opaque pagination cursor returned by a previous call. Round-trip verbatim. Ignored if `statement` already contains its own `CURSOR` clause. |
| `params` | object | no | Bound parameters `{"name": value}` substituted for `$name` placeholders before execution (anti-injection — untrusted text never enters the statement as syntax). Supported in `--embed` only; passing non-empty `params` on `--connect` returns `INVALID_PARAMS`. |

Read-only and no-destructive postures can be enforced **at the MCP layer** via `--query-policy` (see below), in addition to any trust-boundary controls (filesystem permissions on `--embed`, server-side role on `--connect`). The tool description includes the xyTalk surface the agent should know about.

Verb policy: `--query-policy <VALUE>` restricts which xyTalk verbs the `query` tool accepts, enforced at the MCP layer **before** the statement reaches the engine. Statements are classified by parsed AST (not substring match); a statement that cannot be parsed is refused under a restricted policy rather than forwarded unverified. Values:

- `full` (default) — all verbs allowed.
- `no-destructive` — block `DELETE` / `DROP`; other writes (PUT/SET/LINK) still allowed.
- `read-only` — block every mutation.

A forbidden or unparseable statement under a restricted policy returns `INVALID_PARAMS`. Only the `query` tool is gated; the introspection tools are read-only by construction and `snapshot` only creates a backup.

Wall-clock budget: per-call timeout via `--query-timeout-ms <MS>` (default 30 000). Excess returns `INTERNAL_ERROR` with message `"query timed out after <N>ms"` and telemetry label `TIMEOUT`. The other tools are not bounded by this flag — the introspection tools wrap one or three SHOW calls and run < 50 ms; `snapshot` does brief filesystem I/O.

### `list_lobes`

Returns the count and an array of `{name, anchor_count, hint?}`. No arguments. Equivalent to `xyTalk SHOW LOBES`, parsed into structured form. Use as the first discovery call.

### `describe_lobe`

Composite schema introspection for a single lobe — anchors, ghosts (filtered by `source_lobe = name`), profile (pinned fields, learned scan patterns, active ghost count), and the two declared axes hoisted to the top level, `vector` and `satellite`. One argument: `lobe: string` (validated non-empty pre-engine; missing lobe → `INVALID_PARAMS` top-level).

`satellite` is `null` unless the lobe declares a sub-gravity axis (`SATELLITE BY <field>`), in which case it names that field. It is contract, not decoration: an equality on the axis reads one sub-range of the gravity bucket while a range sweeps the parent, so it tells an agent which query shape is cheap. `vector` is `null` when the lobe has no searchable embedding field, else `{"field": ..., "dim": ...}`. A `null` `dim` means the dimension is not fixed yet (declared, but no embedding written): an agent may choose it on the first write. A set `dim` means every `NEAREST` query vector must match it — the engine never embeds, so the caller supplies the vector.

Per-field independent fallibility: each of the three sub-results lands either as its parsed payload OR as `{"error": "..."}` if its SHOW call failed. The agent receives explicit error markers for whatever did not succeed, rather than total failure or silent omission.

### `snapshot`

Create a hot, point-in-time backup of the data dir without stopping the engine: it hard-links the live SSTables and copies the WAL into `snapshots/<name>/` under a milliseconds-short lock window. One argument: `name: string` (the snapshot label). Restore is offline via the CLI (`xyzdb-cli admin snapshot restore …`) — the MCP tool only creates.

## Resources reference

Three URIs surface the same data as the corresponding tools, for MCP clients that prefer URI-tree navigation. The underlying engine path is identical.

| URI | Returns | Surface |
|---|---|---|
| `xyzdb://lobes` | List of lobes (same shape as `list_lobes`) | Concrete URI in `resources/list` |
| `xyzdb://stats` | Stats snapshot (same shape as `stats`) | Concrete URI in `resources/list` |
| `xyzdb://lobes/{name}` | Full lobe schema (same shape as `describe_lobe`) | URI template in `resources/templates/list` |

`resources/read` of a non-existent lobe under the template → `INVALID_PARAMS` (same semantics as the `describe_lobe` tool). Unknown URI → `INVALID_PARAMS`.

## Failure mode catalog

The seven failure modes documented in design doc §10 with reproducer, observed code, and recovery action. The integration test suite at [`crates/mcp/tests/uat_failure_modes.sh`](../crates/mcp/tests/uat_failure_modes.sh) reproduces each one and exits 0 on PASS.

### 1. Engine panic mid-call

**Trigger**: a panic in `Engine::run` (rare; engine is panic-disciplined). The MCP wrapper's `tokio::task::spawn_blocking` reports the join failure.

**Wire**: `INTERNAL_ERROR` (`-32603`), message `"engine join failed: ..."`.
**Recovery**: the MCP process stays alive; future tool calls are unaffected. The agent re-issues the call.

### 2. Data dir corrupted at `--embed` startup

**Trigger**: `Engine::open` fails on a non-empty but malformed data dir.

**Wire**: `xyzdb-mcp` exits with non-zero rc before the MCP handshake completes. The MCP client sees subprocess failure (the MCP client surfaces this in its log panel).
**Recovery**: operator inspects stderr, fixes or wipes the data dir, restarts. There is no in-band recovery — a corrupt data dir is not a partial failure.

### 3. Engine WAL replay fails at startup

**Trigger**: `Engine::open` finds WAL frames whose CRC doesn't validate. Some xyzDB builds tolerate the failure by truncating to the last valid frame; others refuse to open.

**Wire**: same as mode 2 — non-zero rc before handshake.
**Recovery**: depends on the build's WAL policy. The bench fixtures at `tests/uat_failure_modes.sh` Mode 3 illustrate both outcomes.

### 4. Cursor invalid mid-query

**Trigger**: agent passes a `cursor` argument that does not decode (corrupted base64, FilterExpr shape mismatch across xyzDB upgrade).

**Wire**: `INTERNAL_ERROR` from the engine, redacted via Pillar 1's error.rs rules. Message clarifies the cursor was rejected.
**Recovery**: agent drops the cursor and re-issues the SCAN/FIND from the start, or paginates from a known-good cursor.

### 5. Tool-call timeout (`--query-timeout-ms` exceeded)

**Trigger**: a query — typically an unbounded SCAN over a multi-million-record lobe — exceeds the configured wall-clock budget (default 30 000 ms).

**Wire**: `INTERNAL_ERROR` (`-32603`), message `"query timed out after <N>ms"`. Telemetry label: `TIMEOUT`.
**Recovery**: the spawned blocking task continues to completion in the runtime's blocking pool (tokio cannot pre-empt blocking work) but the MCP-side future is freed. The agent re-issues with a more selective filter or a `LIMIT` clause.

### 6. Malformed JSON from MCP client

**Trigger**: the MCP client sends a frame that is not parseable JSON-RPC.

**Wire**: rmcp 1.5 closes the stream after logging the parse error to stderr. JSON-RPC §5.1 allows either responding with `PARSE_ERROR` or closing — rmcp chooses close + log.
**Recovery**: the parent process detects subprocess EOF and may relaunch. The exit is graceful (rc=0); no crash, no orphan engine state (the LSM lock is released via Drop).

### 7. `--connect` mode: TCP connection drops mid-call

**Trigger**: upstream `xyzdb-server` is restarted or the network drops between the MCP and the server.

**Wire**: `INTERNAL_ERROR`, message `"connect-mode <STMT> failed: ..."` with the underlying TCP error chained.
**Recovery**: the MCP process stays alive (the connection is per-call). The agent retries; if the server is back the call succeeds. The MCP-side telemetry preserves the latency of the failed attempt for outage analysis.

## Privacy & telemetry

`xyzdb-mcp` emits one structured `tracing` event per tool call to stderr. Default-on redaction means **statement text and cursor tokens are never logged**:

- `statement` → `query_hash` (xxh3-64, first 8 hex chars) + `query_kind` (first verb).
- `cursor` → `cursor_present: bool`.
- result records → `records_returned: u64` (count only).
- error messages → `error_code` enum (no message body — already redacted at the wire by Pillar 1).

Every event additionally carries `caller_id="stdio"` and `request_id` (UUIDv7 generated per call, stable for span correlation when v0.5 sub-cycle C multi-actor auth lands).

### `--log-statements` (development only)

Adds full statement + cursor logging at TRACE level on a dedicated target (`xyzdb_mcp::statements`). The boot warning emitted at startup makes the opt-in posture explicit:

```
WARN xyzdb_mcp::telemetry: --log-statements is ACTIVE.
Full xyTalk statements and cursor tokens will be recorded
at TRACE level on target xyzdb_mcp::statements. Do not use
this flag in production deployments. PII contained in
statement literals will appear in stderr and any sink
capturing it (journald, Docker logs, MCP client
diagnostics).
```

### Cross-actor leak guard

`--log-statements` is **rejected at startup** when `--connect` targets a non-loopback host. Statements from *other actors* sharing the same upstream `xyzdb-server` would otherwise land in this MCP process's stderr — a privacy leak across actors.

Loopback recognised: `127.0.0.0/8`, `::1`, `[::1]`, `localhost` (case-insensitive). Anything else is refused with exit code 2 and an explicit error pointing the operator to `--connect 127.0.0.1` or `--embed`.

## Concurrent dispatch note

rmcp 1.5 dispatches incoming `tools/call` requests **concurrently** by design — the MCP server does not serialize requests. For workflows where the agent expects strict ordering (e.g. `LOBE → ANCHOR → PUT`), the agent itself must wait for each response before issuing the next request. Production MCP clients implement this awaiting pattern; bespoke JSON-RPC clients may not.

This is a design property of rmcp, not a bug. The per-call telemetry events in stderr are stamped with `request_id` so out-of-order completion can be reconstructed if needed.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| The MCP client logs `subprocess exited with code 2` immediately on launch | `--log-statements` + `--connect <non-loopback>` | Use `127.0.0.1` / `localhost`, or drop `--log-statements`, or switch to `--embed`. |
| `subprocess exited with code 1` on launch | Data dir corrupted, lock held by another process, or invalid path | Inspect the `xyzdb-mcp` stderr in the MCP client's diagnostics panel; common fixes are wiping the dir, killing the conflicting process, or correcting the path. |
| `query timed out after 30000ms` on every SCAN | Unbounded SCAN over a large lobe | Add `LIMIT n` or a more selective filter; re-issue. Optionally bump `--query-timeout-ms`. |
| `connect-mode STATS failed` at startup | `--connect` host:port unreachable | Verify the upstream `xyzdb-server` is running on the expected port; check firewalls and ACLs. |
| Tool calls return out of order in non-standard clients | rmcp concurrent dispatch | Agent must `await` each response before issuing the next request; see [Concurrent dispatch note](#concurrent-dispatch-note). |
| `describe_lobe` returns `{"error": "..."}` for one of `anchors`/`ghosts`/`profile` | One of the three SHOW calls failed (engine internal error) | The other two fields still carry data; check the error string. The agent can retry just the failing SHOW via the `query` tool. |

## Versioning

Tools ship unversioned names today. The first breaking evolution will introduce `<name>_v2` alongside the original; the deprecation window is two minor releases. Agents that auto-discover via `tools/list` get the latest by default.

## Reference

- Integration tests: [`crates/mcp/tests/uat_*.sh`](../crates/mcp/tests/)
- Source: [`crates/mcp/`](../crates/mcp/)
- xyTalk reference: [`docs/xytalk-spec.md`](xytalk-spec.md)
