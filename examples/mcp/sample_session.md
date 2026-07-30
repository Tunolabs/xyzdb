# Sample agent session — annotated wire transcript

Realistic walk through what an MCP-aware agent sees when it connects to a fresh `xyzdb-mcp --embed` and brings a small dataset up. Frames are JSON-RPC 2.0; line numbers are illustrative.

This is the actual wire shape rmcp emits — not a sketch. Reproducible end-to-end via `crates/mcp/tests/uat_connect_rehearsal.sh` (which exercises the same flow against `--connect` mode).

## 1. Handshake

The client sends `initialize`; the server responds with the protocol version, capabilities, and a description of itself.

```json
→ {"jsonrpc":"2.0","id":1,"method":"initialize",
   "params":{"protocolVersion":"2024-11-05","capabilities":{},
   "clientInfo":{"name":"my-agent","version":"0.1"}}}

← {"jsonrpc":"2.0","id":1,"result":{
     "protocolVersion":"2024-11-05",
     "capabilities":{"resources":{},"tools":{}},
     "serverInfo":{"name":"xyzdb-mcp","version":"0.8.13"},
     "instructions":"xyzDB MCP server. Tools: stats, query, list_lobes, describe_lobe, snapshot. Resources: xyzdb://lobes, xyzdb://stats, and template xyzdb://lobes/{name}. Two modes: --embed and --connect. See docs/mcp-integration.md for tools, resources, and modes."
   }}

→ {"jsonrpc":"2.0","method":"notifications/initialized"}
```

## 2. Discovery — what data exists?

The agent picks `list_lobes` first to see what is registered.

```json
→ {"jsonrpc":"2.0","id":2,"method":"tools/call",
   "params":{"name":"list_lobes"}}

← {"jsonrpc":"2.0","id":2,"result":{
     "content":[{"type":"text","text":
       "{\n  \"count\": 0,\n  \"lobes\": []\n}"}],
     "isError":false}}
```

A fresh data dir reports `count: 0`. The agent decides to set up a small schema.

## 3. Schema bring-up via the `query` tool

xyTalk for *"create a lobe called creditos with rfc as anchor"*:

```json
→ {"jsonrpc":"2.0","id":3,"method":"tools/call",
   "params":{"name":"query","arguments":{
     "statement":"LOBE \"creditos\" HINT=\"Credit lifecycle\""}}}

← {"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":
     "{\n  \"message\": \"Lobe 'creditos' created (id=0)\",\n  \"status\": \"ok\"\n}"
   }],"isError":false}}

→ {"jsonrpc":"2.0","id":4,"method":"tools/call",
   "params":{"name":"query","arguments":{
     "statement":"ANCHOR \"rfc\" UNIQUE IN \"creditos\""}}}

← {"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":
     "{\n  \"message\": \"Anchor 'rfc' registered in 'creditos'\",\n  \"status\": \"ok\"\n}"
   }],"isError":false}}
```

Behind the scenes the rmcp server dispatches each `tools/call` concurrently — the agent must wait for each response before issuing the next request to preserve order. Production MCP clients do this by default.

## 4. Data ingestion

A single PUT, then a batch:

```json
→ {"jsonrpc":"2.0","id":5,"method":"tools/call",
   "params":{"name":"query","arguments":{
     "statement":"PUT {rfc: \"AAAA111\", monto: 1500, status: \"active\"} IN \"creditos\""}}}

← {"jsonrpc":"2.0","id":5,"result":{"content":[{"type":"text","text":
     "{\n  \"lid\": \"0000:0000:508AD75B4F1A:00000000:0000\",\n  \"message\": \"1 record inserted (LID: 0000:0000:508AD75B4F1A:00000000:0000)\",\n  \"status\": \"ok\"\n}"
   }],"isError":false}}
```

The `lid` is xyzDB's permanent record identifier. Agents that need to reference the record later store it.

## 5. Introspection — `describe_lobe`

```json
→ {"jsonrpc":"2.0","id":6,"method":"tools/call",
   "params":{"name":"describe_lobe","arguments":{"lobe":"creditos"}}}

← {"jsonrpc":"2.0","id":6,"result":{"content":[{"type":"text","text":
     "{\n  \"name\": \"creditos\",\n  \"anchors\": [\n    {\n      \"name\": \"rfc\",\n      \"unique\": true\n    }\n  ],\n  \"ghosts\": [],\n  \"profile\": {\n    \"pinned_fields\": [],\n    \"learned_patterns\": [],\n    \"active_ghosts_count\": 0\n  },\n  \"vector\": null\n}"
   }],"isError":false}}
```

The three sub-fields (`anchors`, `ghosts`, `profile`) are independently fallible. If `SHOW GHOSTS` had failed mid-call, that field would carry `{"error": "..."}` while `anchors` and `profile` returned data normally.

## 6. Query — anchor-bound point lookup

```json
→ {"jsonrpc":"2.0","id":7,"method":"tools/call",
   "params":{"name":"query","arguments":{
     "statement":"FIND \"creditos\" WHERE rfc = \"AAAA111\""}}}

← {"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":
     "{\n  \"records\": [{\"rfc\": \"AAAA111\", \"monto\": 1500, \"status\": \"active\"}],\n  \"elapsed_ms\": 0.31,\n  \"status\": \"ok\"\n}"
   }],"isError":false}}
```

xyzDB's anchor lookup is O(1); the latency is sub-millisecond on warm cache.

## 7. Resource navigation — `xyzdb://lobes/creditos`

Some MCP clients (via their resource tree) prefer URI navigation over imperative tool calls. The same data:

```json
→ {"jsonrpc":"2.0","id":8,"method":"resources/list"}

← {"jsonrpc":"2.0","id":8,"result":{
     "resources":[
       {"uri":"xyzdb://lobes","name":"lobes","title":"All lobes","mimeType":"application/json","description":"List of every lobe registered in xyzDB. Same shape as the list_lobes tool."},
       {"uri":"xyzdb://stats","name":"stats","title":"Engine stats snapshot","mimeType":"application/json","description":"Live snapshot of xyzDB internals: memtables, SSTables, compaction counters, block cache, ghosts, sync thread, process and cgroup memory. Same shape as the stats tool."}
     ]}}

→ {"jsonrpc":"2.0","id":9,"method":"resources/read",
   "params":{"uri":"xyzdb://lobes/creditos"}}

← {"jsonrpc":"2.0","id":9,"result":{"contents":[{
     "uri":"xyzdb://lobes/creditos",
     "mimeType":"application/json",
     "text":"{\n  \"name\": \"creditos\",\n  \"anchors\": [...],\n  ...\n}"
   }]}}
```

Resources are duplicates of tool responses by design — same engine path, different access pattern.

## 8. Error surface — missing lobe

```json
→ {"jsonrpc":"2.0","id":10,"method":"tools/call",
   "params":{"name":"describe_lobe","arguments":{"lobe":"nonexistent"}}}

← {"jsonrpc":"2.0","id":10,
   "error":{"code":-32602,"message":"describe_lobe: lobe 'nonexistent' not found"}}
```

`describe_lobe` runs a SHOW LOBES pre-flight; on miss it returns top-level `INVALID_PARAMS` rather than a partial body with all-error fields. The wire-level code is `-32602` (JSON-RPC's `Invalid params`).

## What you don't see in the transcript

Every successful tool call emits one `tracing::info` event to stderr with `caller_id`, `request_id` (UUIDv7), `tool`, `latency_ms`, `query_hash`, `query_kind`, `cursor_present`, `records_returned`. No raw statement text. No record content. No cursor token. The full statement only appears on stderr if the operator passed `--log-statements`, and even then only at TRACE level on a dedicated target so the EnvFilter can route it elsewhere. See `docs/mcp-integration.md` §Privacy & telemetry.
