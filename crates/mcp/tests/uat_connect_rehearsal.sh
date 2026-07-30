#!/usr/bin/env bash
# --connect (multi-process) stack rehearsal.
#
# Purpose: prove that --connect mode works end-to-end against a real
# xyzdb-server. Mirrors the production deployment shape of:
#   xyzdb-server (data-dir holder, TCP) + xyzdb-mcp --connect (MCP
#   subprocess of an MCP client like Claude Desktop).
#
# Failure modes intentionally NOT covered here (those land in the
# Día 20–21 mock JSON-RPC suite):
#   - server crash mid-call
#   - cursor invalid across upgrades
#   - WAL replay failures
#   - client-side malformed JSON
#
# Output: exit 0 on full PASS; non-zero with the first failing
# assertion on FAIL.

set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
MCP="$ROOT/xyzdb/target/release/xyzdb-mcp"
SERVER="$ROOT/xyzdb/target/release/xyzdb-server"

if [ ! -x "$MCP" ]; then echo "missing: $MCP — run 'cargo build --release -p xyzdb-mcp'" >&2; exit 2; fi
if [ ! -x "$SERVER" ]; then echo "missing: $SERVER — run 'cargo build --release -p xyzdb-server'" >&2; exit 2; fi

# Ports reserved for this rehearsal. Avoid 2505 (xyzdb default) so a
# concurrent dev server is not disturbed.
PORT="${PORT:-22506}"
DATA_DIR="${DATA_DIR:-/tmp/xyzdb-uat-d19}"
SERVER_LOG="/tmp/xyzdb-uat-d19.server.log"
MCP_LOG="/tmp/xyzdb-uat-d19.mcp.err"
MCP_OUT="/tmp/xyzdb-uat-d19.mcp.out"
FIFO="/tmp/xyzdb-uat-d19.fifo"

echo "== --connect rehearsal =="
echo "  MCP    : $MCP"
echo "  SERVER : $SERVER"
echo "  PORT   : $PORT"
echo "  DATA   : $DATA_DIR"

# ── Cleanup helpers ────────────────────────────────────────────────
cleanup() {
    [ -n "${MCP_PID:-}" ] && kill "$MCP_PID" 2>/dev/null
    [ -n "${SRV_PID:-}" ] && kill "$SRV_PID" 2>/dev/null
    wait 2>/dev/null
    rm -f "$FIFO"
}
trap cleanup EXIT

rm -rf "$DATA_DIR"
rm -f "$FIFO" "$SERVER_LOG" "$MCP_LOG" "$MCP_OUT"
mkdir -p "$DATA_DIR"

# ── Bring up xyzdb-server ──────────────────────────────────────────
echo "-- starting xyzdb-server on port $PORT"
"$SERVER" --path "$DATA_DIR" --port "$PORT" --bind 127.0.0.1 > "$SERVER_LOG" 2>&1 &
SRV_PID=$!

# Wait for server to bind. xyzdb-server prints "listening" once ready.
for i in $(seq 1 30); do
    if grep -qE "listening|Listening|started|Started" "$SERVER_LOG" 2>/dev/null; then break; fi
    if ! kill -0 "$SRV_PID" 2>/dev/null; then
        echo "FAIL: xyzdb-server died before becoming ready"
        echo "--- server log ---"; cat "$SERVER_LOG"
        exit 1
    fi
    sleep 0.2
done
echo "   server up (pid $SRV_PID)"

# ── Bring up xyzdb-mcp --connect ───────────────────────────────────
mkfifo "$FIFO"
echo "-- starting xyzdb-mcp --connect 127.0.0.1:$PORT"
"$MCP" --connect "127.0.0.1:$PORT" --no-probe < "$FIFO" > "$MCP_OUT" 2> "$MCP_LOG" &
MCP_PID=$!
exec 9>"$FIFO"

# ── JSON-RPC drive helpers ─────────────────────────────────────────
send() {
    printf '%s\n' "$1" >&9
}

wait_for_id() {
    local id="$1"
    for i in $(seq 1 50); do
        if grep -q "\"id\":$id" "$MCP_OUT" 2>/dev/null; then return 0; fi
        sleep 0.1
    done
    echo "TIMEOUT waiting for response id=$id" >&2
    return 1
}

assert_contains() {
    local id="$1" needle="$2" desc="$3"
    local line
    line=$(grep "\"id\":$id" "$MCP_OUT" | head -1)
    if echo "$line" | grep -qF -- "$needle"; then
        echo "PASS: $desc"
    else
        echo "FAIL: $desc"
        echo "  expected substring: $needle"
        echo "  got: $line"
        exit 1
    fi
}

# ── Phase 1: handshake ─────────────────────────────────────────────
echo "-- handshake"
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"uat","version":"0"}}}'
wait_for_id 1
send '{"jsonrpc":"2.0","method":"notifications/initialized"}'
sleep 0.2
assert_contains 1 "xyzdb-mcp" "initialize returned serverInfo"
assert_contains 1 "tools" "initialize advertised tools capability"
assert_contains 1 "resources" "initialize advertised resources capability"

# ── Phase 2: data set-up via query ─────────────────────────────────
echo "-- set up sample data"
send '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"query","arguments":{"statement":"LOBE \"uat19\" HINT=\"D19 rehearsal\""}}}'
wait_for_id 2
assert_contains 2 "Lobe 'uat19' created" "LOBE created via --connect"

send '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"query","arguments":{"statement":"ANCHOR \"id\" UNIQUE IN \"uat19\""}}}'
wait_for_id 3
assert_contains 3 "Anchor 'id' registered" "ANCHOR registered via --connect"

send '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"query","arguments":{"statement":"PUT {id: 1, label: \"first\"} IN \"uat19\""}}}'
wait_for_id 4
assert_contains 4 "1 record inserted" "PUT round-tripped through xyzdb-server"

send '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"query","arguments":{"statement":"PUT {id: 2, label: \"second\"} IN \"uat19\""}}}'
wait_for_id 5
assert_contains 5 "1 record inserted" "PUT (2nd) round-tripped"

# ── Phase 3: read paths ────────────────────────────────────────────
echo "-- read paths"
send '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"list_lobes"}}'
wait_for_id 6
assert_contains 6 "uat19" "list_lobes surfaced the new lobe"
assert_contains 6 "D19 rehearsal" "list_lobes preserved the hint"

send '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"describe_lobe","arguments":{"lobe":"uat19"}}}'
wait_for_id 7
# Embed-mode helpers serialise via to_string_pretty (whitespace
# between colon and value); the connect-mode path forwards the
# upstream server JSON which is compact. Match on the field value
# only, which is identical across both shapes.
assert_contains 7 "uat19" "describe_lobe returned the lobe name"
assert_contains 7 "unique" "describe_lobe surfaced the UNIQUE anchor"

send '{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"query","arguments":{"statement":"SCAN \"uat19\""}}}'
wait_for_id 8
assert_contains 8 "first" "SCAN returned both records (first)"
assert_contains 8 "second" "SCAN returned both records (second)"

send '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"query","arguments":{"statement":"FIND \"uat19\" WHERE id = 1"}}}'
wait_for_id 9
assert_contains 9 "first" "FIND by anchor returned the right record"

send '{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"stats"}}'
wait_for_id 10
assert_contains 10 "keyspaces" "stats returned full snapshot"

# ── Phase 4: resources surface ─────────────────────────────────────
echo "-- resources"
send '{"jsonrpc":"2.0","id":11,"method":"resources/list"}'
wait_for_id 11
assert_contains 11 "xyzdb://lobes" "resources/list contains xyzdb://lobes"
assert_contains 11 "xyzdb://stats" "resources/list contains xyzdb://stats"

send '{"jsonrpc":"2.0","id":12,"method":"resources/read","params":{"uri":"xyzdb://lobes/uat19"}}'
wait_for_id 12
assert_contains 12 "uat19" "resources/read xyzdb://lobes/uat19 returned schema"
assert_contains 12 "anchors" "resources/read returned anchors field"

# ── Phase 5: error paths ───────────────────────────────────────────
echo "-- error paths"
send '{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"describe_lobe","arguments":{"lobe":"does-not-exist"}}}'
wait_for_id 13
assert_contains 13 "not found" "missing lobe → INVALID_PARAMS"
assert_contains 13 "-32602" "missing lobe → wire code -32602"

send '{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"query","arguments":{"statement":"NOTAVALIDVERB \"x\""}}}'
wait_for_id 14
assert_contains 14 "error" "garbled statement → error response"

# ── Phase 6: shutdown ──────────────────────────────────────────────
echo "-- shutdown"
exec 9>&-
sleep 0.3

echo ""
echo "== Día 19 PASS — --connect mode end-to-end against real xyzdb-server =="
exit 0
