# `examples/mcp/` — Sample sessions and configuration templates

Concrete artefacts for adopters bringing `xyzdb-mcp` up against an MCP client.

| File | Purpose |
|---|---|
| `claude_desktop_config.embed.json` | Drop-in template for `--embed` mode (single-process subprocess pattern). |
| `claude_desktop_config.connect.json` | Drop-in template for `--connect` mode (TUNO-Pro multi-process pattern). |
| `sample_session.md` | Annotated transcript of a realistic agent session: schema bring-up, data ingestion, introspection, query. Shows what the wire looks like for each tool and resource. |
| `troubleshooting.md` | Concrete error snapshots → diagnosis → fix. Pulled from real first-launch failure modes. |

The full integration guide is at [`docs/mcp-integration.md`](../../docs/mcp-integration.md).

## How to use the templates

1. Edit the chosen template, replacing the placeholder paths with absolute paths on your system.
2. Place at the OS-specific location:
   - macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
   - Windows: `%APPDATA%\Claude\claude_desktop_config.json`
3. Restart the MCP client.
4. The `xyzdb` server appears in the tools panel.

## Smoke verification before connecting an agent

Before pointing an MCP client at the MCP, the two integration scripts at [`crates/mcp/tests/`](../../crates/mcp/tests/) verify the server is healthy on your machine:

```bash
cargo build --release -p xyzdb-mcp -p xyzdb-server
./crates/mcp/tests/uat_connect_rehearsal.sh   # 22 assertions, --connect mode
./crates/mcp/tests/uat_failure_modes.sh        # 11 assertions, all 7 failure modes
```

Both exit 0 on PASS; non-zero with the first failing assertion.
