# xyzDB wire protocol

**Audience**: someone implementing a client against `xyzdb-server` without
reading the engine source.
**Surface**: the bytes on the TCP socket — framing, the auth preamble, format
bytes, response bodies and their fields. Not the query language (that is
`docs/xytalk-spec.md`), not operator flags (`OPERATIONS.md`), and not the HTTP
page multiplexed onto the same port (§11, explicitly out of scope).

This document specifies the binary wire protocol that an `xyzdb-server` speaks
over TCP. It is written for people building third-party clients. Everything here
is derived from the server source; each constant and behaviour cites the file
and line it comes from. The reference implementation is the single-file client
[`examples/client/python/xyzdb_minimal.py`](examples/client/python/xyzdb_minimal.py).

The protocol carries **xyTalk** statements (the query language, specified in
[`docs/xytalk-spec.md`](docs/xytalk-spec.md)) and returns their results. It is a
length-prefixed binary frame protocol; it is not HTTP (an HTTP surface is
multiplexed onto the same port for operators — see §11 — but is not part of this
protocol).

All integers are **big-endian**. All source references are to
`crates/server/src/`.

---

## 1. Transport and default port

- **TCP.** The server binds one listening socket (`main` in `main.rs`).
- **Default port `2505`** (`--port`, `main.rs`), default bind address
  **`127.0.0.1`** (`--bind`, `main.rs`) — so a default server is not reachable
  off-host and a client on another machine cannot connect until the operator
  changes it. Binding a non-loopback address with no `--auth-token` refuses to
  start unless `--insecure-allow-no-auth` is passed
  (`refuse_unauthenticated_bind` in `main.rs`), so any server a third-party
  client reaches over a network either has a token or was opened deliberately.
  The container image commands `0.0.0.0`, which is why a plain `docker run`
  without a token fails to start rather than exposing an open server.
- **Framing.** Every message is length-prefixed. A single connection may carry
  many request/response frames in sequence (except the bulk-load and HTTP modes,
  which own the whole connection).

## 2. TLS

TLS is not negotiated in-band. The socket is **either** plain TCP **or** TLS —
the server chooses at startup:

- When both `--tls-cert` and `--tls-key` are given, the server accepts **TLS
  1.3** connections and the TLS layer wraps the socket (`main.rs`,
  `build_tls_config` in `main.rs`). Passing only one of the two is a startup error
  (`main` in `main.rs`).
- Otherwise the server serves plain TCP and logs a warning at boot
  (`main` in `main.rs`).

Everything below (the auth preamble, the version byte, all frames) happens
**inside** the TLS session when TLS is enabled — i.e. after the TLS handshake
completes (`AUTH_MAGIC` in `protocol.rs`).

## 3. Connection lifecycle — first-byte dispatch

The server reads the **first byte** of the connection and dispatches on it
(`handle_connection` and `auth_handshake` in `connection.rs`):

| First byte | Meaning |
|---|---|
| `0x41` (`'A'`, `AUTH_MAGIC`) | Bearer-token auth preamble (§4) | 
| `0x01` / `0x02` / `0x03` / `0x04` | Protocol version V1 / V2 / V3 / V4 (§5) |
| `G` `H` `P` `O` `D` `T` `C` | An HTTP/1.x request line — handed to the HTTP surface (§11) |

The three spaces are disjoint by construction: the version bytes `0x01-0x04` are
control bytes, `AUTH_MAGIC` is `0x41` (`'A'`), and HTTP methods never begin with
`A` (`is_http_method_first_byte` in `http.rs`). `AUTH_MAGIC` was chosen as `'A'`
specifically to sit outside the version space and to be printable
(`protocol.rs`).

## 4. Authentication preamble (bearer token)

Constants: `AUTH_MAGIC = 0x41` (`protocol.rs`), `MAX_AUTH_TOKEN_LEN = 4096`
(a `u16`, `protocol.rs`).

A client that has a token sends, as the very first bytes of the connection
(after the TLS handshake, if any):

```
[0x41: u8][token_len: u16 BE][token: token_len bytes, UTF-8]
```

Layout from `write_auth_frame` (`protocol.rs`) and
`read_auth_frame_body` (`protocol.rs`). `token_len` greater than
`MAX_AUTH_TOKEN_LEN` is rejected on both send and receive
(`write_auth_frame` and `read_auth_frame_body` in `protocol.rs`).

Server behaviour (`auth_handshake` in `connection.rs`):

- **Server started with `--auth-token`** (`main.rs`): the first byte
  must be `AUTH_MAGIC`. The presented token is compared to the configured value.
  On mismatch the server writes an error frame (§8) and closes
  (`auth_handshake` in `connection.rs`). After a successful auth it reads the actual
  protocol-version byte and proceeds (`auth_handshake` in `connection.rs`). A connection
  whose first byte is neither `AUTH_MAGIC` nor an allowlisted probe (§11) is
  rejected with `ERROR: server requires auth (Authorization: Bearer <token>);
  send AUTH_MAGIC frame first` (`auth_handshake` in `connection.rs`).
- **Server started without `--auth-token`** (open server): it still **accepts
  and silently consumes** an auth frame if the client sends one, then reads the
  version byte — this lets a client always set its token even against a dev
  server (`AUTH_MAGIC` in `protocol.rs`, `auth_handshake` in `connection.rs`, `main.rs`). If no
  auth frame is sent, the first byte is the version byte.

The token is read from a UTF-8 file named by `--auth-token`; leading and
trailing whitespace is trimmed, and an empty token file is refused at startup
(`--auth-token`, `main` in `main.rs`).

**The frame carries the token in the clear.** There is no challenge, nonce or
hash — the bytes on the wire are the token. On a plain-TCP server (§2) anything
that can read the connection can replay it, so a client that authenticates over
an untrusted network needs the server to be running with TLS. This is a property
of the protocol, not of any one client.

## 5. Protocol versions and request framing

The version byte selects the request shape (`read_request`,
`protocol.rs`; `main.rs` clients use `write_request_v1/v2`,
`protocol.rs`).

| Version | Byte | Adds | Request frame |
|---|---|---|---|
| V1 | `0x01` (`PROTOCOL_V1`, `protocol.rs`) | text only | `[0x01][len: u32 BE][query: UTF-8]` — format is forced to TEXT (`read_request` in `protocol.rs`) |
| V2 | `0x02` (`PROTOCOL_V2`, `protocol.rs`) | a format byte | `[0x02][format: u8][len: u32 BE][query: UTF-8]` (`read_request` in `protocol.rs`) |
| V3 | `0x03` (`PROTOCOL_V3`, `protocol.rs`) | binary bulk load | own connection mode — see §7 |
| V4 | `0x04` (`PROTOCOL_V4`, `protocol.rs`) | bound parameters | `[0x04][format: u8][query_len: u32 BE][query: UTF-8][params_len: u32 BE][params: JSON]` (`read_request` in `protocol.rs`) |

- `query` is UTF-8 xyTalk (`read_request` in `protocol.rs`).
- **V4 params** is a JSON object mapping `$name` placeholders to values,
  substituted server-side before execution so untrusted text never enters the
  statement as syntax (`PROTOCOL_V4` in `protocol.rs`). V4 is backward compatible: a client
  with no parameters simply speaks V2 (`PROTOCOL_V4` in `protocol.rs`).
- An unsupported version byte is rejected with `Unsupported protocol version`
  (`read_request` in `protocol.rs`).

## 6. Payload formats (the format byte)

The format byte (V2/V4) selects how the server serialises the response body
(`FORMAT_*` in `protocol.rs`):

| Format | Byte | Response body |
|---|---|---|
| TEXT | `0x00` (`FORMAT_TEXT`, `protocol.rs`) | Human-readable text (`format_result` in `response.rs`, e.g. `OK: ...`, record boxes, `N record(s) found`) |
| BINARY | `0x01` (`FORMAT_BINARY`, `protocol.rs`) | bincode-serialized `QueryResult` (`protocol.rs`) |
| JSON | `0x02` (`FORMAT_JSON`, `protocol.rs`) | JSON object (§8) |
| JSON_CHUNKED | `0x03` (`FORMAT_JSON_CHUNKED`, `protocol.rs`) | Chunked JSON stream (§9) |
| BINARY_CHUNKED | `0x04` (`FORMAT_BINARY_CHUNKED`, `protocol.rs`) | Chunked binary stream (§9) |

The reference client always uses **JSON** (`0x02`)
(`examples/client/python/xyzdb_minimal.py:47`). The **BINARY** format (`0x01`) is
defined but not exercised by any first-party client and is outside the 1.0
compatibility guarantees; request `JSON` (`0x02`) instead.

## 7. V3 — binary bulk load (optional)

An advanced, connection-owning mode for high-throughput ingestion. After the
`0x03` version byte the client sends a header and a stream of batch frames
(`handle_v3_bulk_load` in `connection.rs`, `read_v3_header`/`read_v3_batch_frame` in `protocol.rs`):

```
header:  [flags: u8][lobe_name_len: u16 BE][lobe_name: UTF-8]     (read_v3_header, protocol.rs)
batch:   [frame_type: u8][record_count: u32 BE][payload_len: u32 BE][payload]  (read_v3_batch_frame, protocol.rs)
         frame_type 0x01 = data (V3_FRAME_DATA), 0x00 = end-of-stream (V3_FRAME_END)  (protocol.rs)
end:     a batch frame with frame_type 0x00 terminates the stream   (read_v3_batch_frame, protocol.rs)
```

`flags`: `V3_FLAG_SORTED = 0x01`, `V3_FLAG_LZ4 = 0x02` (`protocol.rs`).
The server replies with a batch response
`[status: u8][count: u32 BE][first_lid: u128 BE][last_lid: u128 BE]`
(`write_v3_batch_response`, `protocol.rs`). Clients that do not need bulk
load can ignore V3 entirely.

## 8. Response framing, status codes, and errors

Every non-chunked response is (`write_response_bytes`, `protocol.rs`):

```
[status: u8][len: u32 BE][body: len bytes]
```

Status bytes (`protocol.rs`): `STATUS_OK = 0x00`, `STATUS_ERROR = 0x01`,
`STATUS_CHUNKED = 0x02` (see §9).

**JSON success** bodies are a JSON object with `"status": "ok"` plus the result
payload — `"records"` for FIND/SCAN, `"aggregation"` for AGGREGATE, etc.
(`serialize_json` in `json_response.rs`).

### 8.1 Fields a client MUST NOT drop

A result can be **partial**, and it says so in the body rather than in the status
byte: a truncated answer is `STATUS_OK`. A client that reads only `records` cannot
tell a complete result from an incomplete one, and this section exists because that
is not hypothetical — the 1.0.x reference clients dropped these fields in their
fluent terminal, so a partial arrived looking exactly like a full answer.

| Field | When present | Meaning |
|---|---|---|
| `records` | FIND / SCAN / pipelines returning rows | the rows |
| `has_more` | a truncated result | more rows exist beyond this frame |
| `cursor` | a resumable page | opaque token for the next `SCAN … CURSOR` |
| `budget_stop` | **only** a `NEAREST` cut by `--nearest-budget-ms` | the cut, described below |

`has_more = true` with `cursor = null` is a legitimate combination and is **not** a
bug: a `NEAREST` cut by the latency airbag has no resumable page, because resuming
would repeat the whole scoring pass. Treat it as "these are the best found, more may
exist", not as "call again".

`budget_stop` has four members:

| Member | Meaning |
|---|---|
| `candidates` | the whole **scored** set, before the residual filter — **not** the number of matches |
| `examined` | how many had the residual **checked** before the cut |
| `found` | how many **passed** |
| `strategy` | `"score_order"` or `"key_order"` — which traversal produced it |

`strategy` is load-bearing, not decoration. Under `"score_order"` the rows are a
prefix of the true answer, so what was not reached is *worse*. Under `"key_order"`
they are the best of a contiguous key region and the unwalked part may hold
**better** rows. A client that reports "these are the closest" is correct under the
first and wrong under the second. 1.1.0 emits only `"score_order"`; read the field
rather than assuming it.

`budget_stop` is absent from every non-truncated response, so ordinary frames are
byte-identical to a client that never looks for it. Full semantics:
`docs/xytalk-spec.md` §2.20.

**JSON error** bodies (`serialize_json_error`, `json_response.rs`):

```json
{ "status": "error", "error": "<message>", "code": "<CODE>" }
```

`code` is derived from the message (`error_code`, `json_response.rs`) and
is one of: `PARSE_ERROR`, `LOBE_NOT_FOUND`, `RECORD_NOT_FOUND`,
`DUPLICATE_ANCHOR`, `TYPE_ERROR`, `INVALID_QUERY`, `STORAGE_ERROR`, `THROTTLED`,
`INTERNAL_ERROR` (fallback).

Errors raised **before** a format is known (auth failures, an unsupported
version) are sent as a `STATUS_ERROR` frame whose body is a plain-text
`ERROR: ...` string (`auth_handshake` and `process_request_sync` in `connection.rs`). Clients
should treat any `status == 0x01` frame as an error and attempt a JSON decode of
the body, falling back to the raw bytes as the message
(`examples/client/python/xyzdb_minimal.py:135-141`).

## 9. Chunked streaming (optional)

Requested by the `JSON_CHUNKED` (`0x03`) or `BINARY_CHUNKED` (`0x04`) format byte
(`is_chunked_format`, `protocol.rs`). The server writes
(`write_chunked_header`/`write_chunk_sync`/`write_end_marker_sync` in `protocol.rs`):

```
header:  [status = 0x02: u8][reserved: u32 BE = 0]        (write_chunked_header, protocol.rs)
chunk:   [len: u32 BE][payload: len bytes]                (write_chunk_sync, protocol.rs)   (repeated)
end:     [len: u32 BE = 0]                                (write_end_marker_sync, protocol.rs)
```

A client that does not request a chunked format never sees this shape.

**Chunked formats are plain-TCP only.** Over TLS the server refuses both chunked
format bytes with a `STATUS_ERROR` frame reading `ERROR: chunked streaming format
unsupported on a TLS connection; use a non-chunked request format over TLS`
(`handle_tls_connection` in `connection.rs`). The streaming writer takes the raw
file descriptor and would bypass TLS record framing, so the refusal is structural
rather than a missing feature flag. A client that wants both TLS and large results
pages with a cursor (§8.1) instead.

## 10. Frame size limit

`MAX_FRAME_SIZE = 16 * 1024 * 1024` (16 MiB) (`protocol.rs`). It bounds each
length-prefixed field. A request `len` (or a V4 `params_len`, or a V3
`payload_len`, or a response `len`) that exceeds it is rejected with an
`InvalidData` error — `Frame too large: <n> bytes` on the request path
(`read_request`, `read_v3_batch_frame`, `read_response_raw` in `protocol.rs`). Clients should refuse to send an over-size frame rather
than have the server close the connection mid-send
(`examples/client/python/xyzdb_minimal.py:113-118`).

## 11. Unauthenticated probes and the HTTP surface (not part of this protocol)

**Unauthenticated allowlist — liveness only.** When `--auth-token` is set, the
only queries that bypass authentication are the liveness probes `/health`,
`/ready`, `HEALTH`, `READY` (case-insensitive) — a load balancer or Kubernetes
probe reaches them without the token. `STATS`, `SHOW STATS` and `/metrics` return
the engine stats snapshot and **require the token** when one is configured; they
are served only on an authenticated connection (or when the server is open with
no token). **Authentication applies to everything except the liveness probes.**
The `/ready` probe answers with a JSON `{"ready": <bool>, "reason": "..."}` body.

**HTTP surface.** A minimal **HTTP/1.x** server is multiplexed onto the **same
TCP port** via the first-byte dispatch of §3 (`http.rs`). It serves `GET /` (an
operator HTML page) and `GET /stats` (the same JSON as the wire) — both gated by
the token like the wire path (a matching `xyzdb_token` cookie, `Authorization:
Bearer`, or `?token=`; otherwise `401`). `/health`, `/ready` and `/metrics` are
served on the wire path, not over HTTP. Other methods return `405`. It is
GET-only, one request per connection (`Connection: close`), and is a **separate
surface for operators — it is not part of this binary protocol** and third-party
clients do not use it.

## 12. Rules for implementers

**Mandatory**
- Big-endian for every multi-byte integer.
- Send exactly one version byte per connection before any request (or an auth
  preamble first, then the version byte).
- Honour the 16 MiB frame limit (§10) on every length-prefixed field.
- Read responses as `[status][len][body]` and treat `status == 0x01` as an
  error (§8).

**Optional**
- The auth preamble (§4) — only needed against a server with `--auth-token`, but
  safe to always send.
- V4 bound parameters (§5) — recommended for untrusted input; otherwise V2 is
  sufficient.
- The BINARY / chunked formats (§6, §9) and the V3 bulk-load mode (§7). The
  chunked formats are available on plain TCP only (§9).

**Version compatibility.** V1 → V2 → V4 are additive and a single server accepts
all of them simultaneously; the version byte selects the shape per connection
(`read_request` in `protocol.rs`). A minimal, forward-compatible client only needs **V2 (or
V4) with the JSON format** and the optional auth preamble — which is exactly what
the reference client implements
([`examples/client/python/xyzdb_minimal.py`](examples/client/python/xyzdb_minimal.py)).

---

## License

This specification may be implemented freely, by anyone, under any license.
Implementing this protocol, writing a client, or interoperating with an xyzDB
server does not require a commercial license from xyzDB and is not covered by the
Business Source License. The Business Source License governs the xyzDB engine
source code; it does not govern this protocol.
