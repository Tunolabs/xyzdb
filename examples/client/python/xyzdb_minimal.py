# xyzDB — minimal reference client.
# Copyright (c) 2026 Iván Moreno Mendoza
# SPDX-License-Identifier: BUSL-1.1
#
# Licensed under the Business Source License 1.1. See the LICENSE file at the
# repository root for the full terms and the Change Date.

"""Minimal single-file Python client for xyzDB — a reference implementation of
the wire protocol.

Its job is to ILLUSTRATE the protocol specified in PROTOCOL.md. That
specification may be implemented freely, by anyone, in any language, under any
license — so write your own client straight from PROTOCOL.md; reading this file
does not tie your client to its license. For a ready-to-install client (fluent
query builder, typed records) use the Apache-2.0 packages: ``pip install
xyzdb``, ``npm i xyzdb``, or ``cargo add xyzdb``.

This file itself, like everything else in this repository, is under the Business
Source License 1.1 (see LICENSE); its Additional Use Grant still lets you run it
for your own internal purposes.

Wire protocol (v2/v4): a request is ``[version: u8][format: u8][len: u32 BE]
[payload]``; binding ``$name`` parameters appends ``[len: u32 BE][params-json]``
and switches the version byte to V4 (server-side substitution — untrusted text
never enters the statement as syntax). Responses are ``[status: u8][len: u32 BE]
[body]``. If the ``XYZDB_TOKEN`` env var is set, a bearer-token preamble
``[0x41][len: u16 BE][token]`` is sent right after connecting; servers without
``--auth-token`` silently consume it.

Example:
    >>> import xyzdb_minimal as xyzdb
    >>> with xyzdb.connect("127.0.0.1", 2505) as db:
    ...     db.execute('LOBE "notes"')
    ...     db.put_batch("notes", [{"id": "n1", "text": "hello"}])
    ...     rows = db.execute('SCAN "notes" WHERE id = $i', {"i": "n1"})
"""

import json
import os
import socket
import struct

PROTOCOL_V2 = 0x02
PROTOCOL_V4 = 0x04  # query + bound params (anti-injection)
FORMAT_JSON = 0x02
STATUS_ERROR = 0x01
AUTH_MAGIC = 0x41
MAX_AUTH_TOKEN_LEN = 4096
# The server rejects frames larger than this; checked client-side so an
# oversized request raises a clear error instead of a BrokenPipe.
MAX_FRAME_SIZE = 16 * 1024 * 1024


class XyzDBError(Exception):
    """Server-reported error (status frame) or protocol violation."""

    def __init__(self, message, code="UNKNOWN"):
        super().__init__(message)
        self.code = code


def connect(host="127.0.0.1", port=2505, timeout=30.0):
    """Open a connection and return a :class:`Client`.

    Args:
        host: Server hostname.
        port: Server TCP port.
        timeout: Socket connect/read timeout in seconds.

    Returns:
        A connected :class:`Client` (usable as a context manager).

    Raises:
        ConnectionError: If the socket cannot be opened.
    """
    return Client(host, port, timeout)


class Client:
    """Minimal xyzDB client: ``execute`` xyTalk statements + ``put_batch``."""

    def __init__(self, host, port, timeout=30.0):
        try:
            self.sock = socket.create_connection((host, port), timeout=timeout)
            self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError as e:
            raise ConnectionError(f"Failed to connect to {host}:{port}: {e}")
        token = os.environ.get("XYZDB_TOKEN", "")
        if token:
            tb = token.encode("utf-8")
            if len(tb) > MAX_AUTH_TOKEN_LEN:
                raise XyzDBError(f"XYZDB_TOKEN exceeds {MAX_AUTH_TOKEN_LEN} bytes")
            self.sock.sendall(struct.pack(">BH", AUTH_MAGIC, len(tb)) + tb)

    def execute(self, query, params=None):
        """Run one xyTalk statement and return the parsed JSON response.

        Args:
            query: A single xyTalk statement.
            params: Optional ``{name: value}`` bindings for ``$name``
                placeholders (protocol V4; prefer these for untrusted input).

        Returns:
            The decoded JSON response as a dict.

        Raises:
            XyzDBError: On an error status frame or an oversized frame.
            ConnectionError: On transport failure.
        """
        payload = query.encode("utf-8")
        if len(payload) > MAX_FRAME_SIZE:
            raise XyzDBError(
                f"statement is {len(payload)} bytes, over the 16 MiB frame limit; "
                "split the statement",
                code="FRAME_TOO_LARGE",
            )
        if params is not None:
            pb = json.dumps(params).encode("utf-8")
            if len(pb) > MAX_FRAME_SIZE:
                raise XyzDBError("params block over the 16 MiB frame limit",
                                 code="FRAME_TOO_LARGE")
            frame = (struct.pack(">BBI", PROTOCOL_V4, FORMAT_JSON, len(payload))
                     + payload + struct.pack(">I", len(pb)) + pb)
        else:
            frame = struct.pack(">BBI", PROTOCOL_V2, FORMAT_JSON, len(payload)) + payload
        try:
            self.sock.sendall(frame)
        except OSError as e:
            raise ConnectionError(f"Send failed: {e}")
        header = self._recv_exact(5)
        status, length = header[0], struct.unpack(">I", header[1:5])[0]
        # `length` arrived over the socket, so bound it BEFORE reading that many
        # bytes. The server never emits a frame past MAX_FRAME_SIZE, so a larger
        # value is a protocol error, not a big-but-valid response — and reading it
        # anyway is how one malformed length turns into unbounded memory.
        if length > MAX_FRAME_SIZE:
            raise XyzDBError(
                f"server announced a {length}-byte frame, over the 16 MiB limit; "
                "refusing to read it",
                code="FRAME_TOO_LARGE",
            )
        body = self._recv_exact(length)
        if status == STATUS_ERROR:
            try:
                err = json.loads(body.decode("utf-8"))
                raise XyzDBError(err.get("error", "Unknown error"),
                                 err.get("code", "UNKNOWN"))
            except (json.JSONDecodeError, UnicodeDecodeError):
                raise XyzDBError(body.decode("utf-8", errors="replace"))
        return json.loads(body.decode("utf-8"))

    def put_batch(self, lobe, records):
        """Insert many records in one atomic batch (one WAL write).

        The engine caps a batch at 10,000 records; chunk larger loads.

        Args:
            lobe: Target lobe name.
            records: List of field dicts, one per record. Prefix a field name
                with ``*`` to declare it the gravity field (e.g. ``"*bucket"``).

        Returns:
            The JSON response for the batch.

        Raises:
            XyzDBError: If the batch is rejected.
        """
        items = ", ".join(
            "{" + ", ".join(f"{k}: {_literal(v)}" for k, v in rec.items()) + "}"
            for rec in records
        )
        return self.execute(f'PUT BATCH IN "{lobe}" [{items}]')

    def close(self):
        """Close the connection."""
        try:
            self.sock.close()
        except OSError:
            pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def _recv_exact(self, n):
        """Receive exactly n bytes or raise on a closed connection."""
        data = b""
        while len(data) < n:
            chunk = self.sock.recv(n - len(data))
            if not chunk:
                raise ConnectionError("Connection closed by server")
            data += chunk
        return data


def _literal(value):
    """Format a Python value as an xyTalk literal (strings quoted+escaped,
    bools lowercased, lists as vectors). For untrusted input use ``$params``."""
    if isinstance(value, str):
        return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, (list, tuple)):
        return "[" + ", ".join(str(v) for v in value) + "]"
    return '"' + str(value) + '"'
