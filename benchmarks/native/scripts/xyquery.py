#!/usr/bin/env python3
"""V1 text-protocol client for xyzDB diagnose. Sends one query, prints response.

Used for cycle diagnose sessions where we need to issue arbitrary xyTalk
queries (`SHOW GHOSTS`, `SCAN GHOST ...`, manual `SCAN ... | GROUP BY ...`)
that the bench harness driver doesn't expose. xyzdb-cli admin only handles
Compact/Analyze/Bulkmode/Migrate; this fills the introspection gap.

V1 protocol contract (xyzdb-server: see `xyzdb/crates/xyzdb-server/src/`
and the `execute_on` helper in `benchmarks/native/drivers/xyzdb/src/lib.rs`):
- Request: 1 byte 0x01 + 4 bytes BE length + UTF-8 query.
- Response: 1 byte status (0x00 = OK, !=0 = error) + 4 bytes BE length +
  UTF-8 payload.

Exit code: 0 on status=0x00, 1 otherwise (errors propagate so the script
is composable in pipelines / acceptance gates).

Usage:
    python3 xyquery.py 'SHOW GHOSTS'
    python3 xyquery.py --host 127.0.0.1 --port 2505 'SCAN GHOST "overdue_by_empresa"'
    python3 xyquery.py --timeout 60 'SCAN "creditos" WHERE _type = "Credit" LIMIT 1'
"""
import argparse
import socket
import struct
import sys


def main() -> int:
    p = argparse.ArgumentParser(description="xyzDB V1-protocol single-shot client")
    p.add_argument("--host", default="127.0.0.1", help="server host (default: 127.0.0.1)")
    p.add_argument("--port", type=int, default=2505, help="server port (default: 2505)")
    p.add_argument(
        "--timeout",
        type=int,
        default=300,
        help="socket timeout seconds (default: 300; bump for slow scans)",
    )
    p.add_argument("query", help="xyTalk query string")
    args = p.parse_args()

    try:
        s = socket.create_connection((args.host, args.port), timeout=args.timeout)
    except OSError as e:
        print(f"# connect error: {e}", file=sys.stderr)
        return 2

    try:
        payload = args.query.encode("utf-8")
        s.sendall(b"\x01" + struct.pack(">I", len(payload)) + payload)

        status = s.recv(1)
        if not status:
            print("# server closed connection before status byte", file=sys.stderr)
            return 2
        status_byte = status[0]

        len_buf = b""
        while len(len_buf) < 4:
            chunk = s.recv(4 - len(len_buf))
            if not chunk:
                print("# server closed connection mid-length", file=sys.stderr)
                return 2
            len_buf += chunk
        length = struct.unpack(">I", len_buf)[0]

        data = b""
        while len(data) < length:
            chunk = s.recv(min(65536, length - len(data)))
            if not chunk:
                break
            data += chunk
    finally:
        s.close()

    print(f"# status=0x{status_byte:02x} len={length}")
    print(data.decode("utf-8", errors="replace"))
    return 0 if status_byte == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
