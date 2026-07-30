# Clients

Two ways to talk to an xyzDB server.

**Install a client (recommended).** The published packages are **Apache-2.0** and
carry the full API (fluent query builder, typed records):

```
pip install xyzdb      # Python
npm i xyzdb            # TypeScript / JavaScript
cargo add xyzdb        # Rust
```

**Reference client (this repo).** [`xyzdb_minimal.py`](xyzdb_minimal.py) is a
single file, stdlib-only (`connect` / `execute` with `$param` binding /
`put_batch` / `close`). Its job is to **illustrate the wire protocol** specified
in [`PROTOCOL.md`](../../../PROTOCOL.md) — which anyone may implement freely, in
any language, under any license. Like everything in this repository it is
**BUSL-1.1**, so for your own client implement straight from `PROTOCOL.md` or
install an Apache package above; the Additional Use Grant still lets you run this
file for your own internal purposes.

The wire protocol both speak: request `[version: u8][format: u8][len: u32 BE]
[payload]` (V4 appends a bound-params JSON block), response `[status: u8]
[len: u32 BE][body]`, optional bearer-token preamble from `XYZDB_TOKEN`, 16 MiB
frame cap. The CLI (`crates/cli`) speaks the same protocol interactively.

```python
import xyzdb_minimal as xyzdb

with xyzdb.connect("127.0.0.1", 2505) as db:
    db.execute('LOBE "notes"')
    db.put_batch("notes", [{"*topic": "greetings", "id": "n1", "text": "hello"}])
    rows = db.execute('SCAN "notes" WHERE id = $i', {"i": "n1"})
```
