# MCP integration troubleshooting

Real first-launch failure modes and their fixes. Symptoms map directly to entries in the MCP client's diagnostics panel or MCP server status output.

## Server fails to start

### Symptom — the MCP client logs `subprocess exited with code 2` immediately

```
[xyzdb] error: --log-statements is not allowed with --connect to non-loopback host
'192.168.1.10'. Statements from other MCP-side actors targeting the same xyzdb-server
would be logged cross-actor, which is a privacy leak. Use --connect 127.0.0.1 (loopback)
or --embed for development logging.
```

**Diagnosis**: cross-actor leak guard. `--log-statements` and `--connect <non-loopback>` cannot coexist by design.

**Fix**: drop `--log-statements`, OR change the connect target to `127.0.0.1` / `localhost` / `::1`, OR switch to `--embed`.

### Symptom — `subprocess exited with code 1` on launch

Stderr typically shows one of:

- `Error: failed to open xyzdb at /path/to/data — caused by: ...` — corrupt or wrong-format data dir.
- `Error: failed to open xyzdb at /path — caused by: lock file held by another process` — another xyzdb (server, REPL, or MCP) is already attached to the dir.
- `Error: failed to open xyzdb at /path/to/data — caused by: incompatible on-disk format: found version X, this build expects version Y` — the data dir was written by a different xyzDB version.

**Diagnosis**: `Engine::open` aborted before MCP handshake.

**Fix**: identify the cause, fix the path, kill the conflicting process, or migrate / wipe the data dir. The MCP layer surfaces the engine's error message verbatim.

## Server starts but tool calls fail

### Symptom — every `query` call returns `INTERNAL_ERROR "query timed out after 30000ms"`

**Diagnosis**: unbounded SCAN over a multi-million-record lobe. The default 30-second budget kicks in.

**Fix**: agent adds `LIMIT n` or a more selective `WHERE`. For known long-running batch operations bump the budget at server launch:

```json
"args": ["--embed", "/data", "--query-timeout-ms", "120000"]
```

Note: `stats`, `list_lobes`, `describe_lobe` are NOT bounded by `--query-timeout-ms`.

### Symptom — `INTERNAL_ERROR "connect-mode STATS failed: ..."` at startup, MCP keeps running

**Diagnosis**: the `--connect` upstream is not reachable yet. `xyzdb-mcp` issued the optional probe and got a TCP error.

**Fix**: ensure `xyzdb-server` is running and listening on the expected port. Verify with `nc -zv <host> <port>`. If the upstream is intentionally launched after the MCP, pass `--no-probe` to skip the startup check (the probe is informational only — tool calls work as soon as the upstream is up).

### Symptom — `INTERNAL_ERROR "describe_lobe: lobe 'creditos' not found"`

**Diagnosis**: `describe_lobe` is strict about lobe membership. The pre-flight `SHOW LOBES` did not find a match.

**Fix**: check the spelling — lobe names are case-sensitive at the engine layer. Use `list_lobes` to see what is registered.

### Symptom — `describe_lobe` returns `{"error": "..."}` for one of `anchors` / `ghosts` / `profile`

**Diagnosis**: that specific SHOW sub-call failed. The other two fields still carry data.

**Fix**: the agent can retry just the failing SHOW via the `query` tool. If the error message points to engine internals, capture the `request_id` from stderr telemetry and file an issue.

## Behaviour not matching expectation

### Symptom — tool calls return out of order in a custom JSON-RPC client

**Diagnosis**: rmcp 1.5 dispatches `tools/call` requests concurrently by design. Production MCP clients await each response before issuing the next. Bespoke JSON-RPC clients without that awaiting pattern see results arrive in completion order.

**Fix**: implement request awaiting on the client side. For sequence-sensitive workflows (LOBE → ANCHOR → PUT), the client must wait for each response before sending the next. The `request_id` field in stderr telemetry pairs requests with responses if reconstruction is needed.

### Symptom — sensitive data appears in stderr

**Diagnosis**: `--log-statements` is active.

**Fix**: drop the flag. Default-on redaction means raw statements never appear in INFO-level events; only the xxh3-64 fingerprint and first verb. The opt-in TRACE log on target `xyzdb_mcp::statements` is development-only.

### Symptom — `xyzdb-mcp` warns at startup about a "non-private host"

```
WARN xyzdb_mcp::connect: xyzdb-mcp connecting to a non-private host. Ensure
xyzdb-server has appropriate network ACLs and authentication. Public-facing
xyzdb-server without auth is a security risk.
```

**Diagnosis**: `--connect` host is neither loopback nor RFC1918 private. Could be intentional (TUNO-Pro deployment in a managed VPC with its own ACL) or accidental (typo, leaked test config).

**Fix**: if intentional, the warning is harmless — verify the upstream's ACL. If accidental, fix the host. For cross-network deployments, run `xyzdb-server` with TLS + bearer-token auth (`--tls-cert` / `--tls-key` / `--auth-token`).

## Diagnostic workflow

When a failure does not match anything above:

1. Check the MCP client's log panel for the full `xyzdb-mcp` stderr output. Every tool call emits one structured event with `request_id`, `tool`, `latency_ms`, `result`, `error_code`.
2. Reproduce the exact request via a direct MCP smoke. The integration scripts at `crates/mcp/tests/uat_*.sh` show the wire shape; copy the relevant frame and pipe it via FIFO.
3. Capture the matching `request_id` for an error and search stderr for that ID across the session.
4. If the issue points to engine internals, file with: `xyzdb-mcp --version`, `xyzdb-server --version` (if `--connect`), the OS, and the masked statement (use the `query_hash` reported in telemetry as a stable fingerprint).

The full MCP integration reference: [`docs/mcp-integration.md`](../../docs/mcp-integration.md).
