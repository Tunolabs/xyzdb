// SPDX-License-Identifier: BUSL-1.1
// unwrap()/expect() are enforced on production code only. Test code — inline
// #[cfg(test)] modules and the integration tests under tests/ — may unwrap
// freely, since a panic there is the failure signal, not a defect. Gating on
// not(test) keeps `cargo clippy --all-targets` on real production debt.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

use clap::{Parser, Subcommand, ValueEnum};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Parser)]
#[command(name = "xyzdb-cli", about = "xyzDB interactive client")]
struct Args {
    /// Server host
    #[arg(long, default_value = "localhost", global = true)]
    host: String,

    /// Server port
    #[arg(long, default_value_t = 2505, global = true)]
    port: u16,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run an admin command against the server. Admin verbs are the
    /// preferred surface for COMPACT / ANALYZE / BULKMODE / MIGRATE, so
    /// that housekeeping stays out of application query paths. Those
    /// statements also remain accepted by the language as permanent
    /// aliases; the server logs a notice pointing here.
    Admin {
        #[command(subcommand)]
        verb: AdminVerb,
    },
}

#[derive(Subcommand)]
enum AdminVerb {
    /// Run major compaction across every keyspace.
    Compact,
    /// Run ANALYZE on a lobe (offline field analysis + dictionary encoding).
    Analyze {
        /// Lobe name (no quoting needed; the CLI quotes for the wire).
        lobe: String,
    },
    /// Toggle BULKMODE auto-compaction.
    Bulkmode {
        /// `on` disables auto-compaction; `off` re-enables it.
        #[arg(value_enum)]
        state: BulkmodeState,
    },
    /// Migrate one lobe — or every lobe if `--all` — to the latest
    /// on-disk record format.
    Migrate {
        /// Single lobe to migrate. Mutually exclusive with `--all`.
        lobe: Option<String>,
        /// Migrate every lobe in the database.
        #[arg(long, conflicts_with = "lobe")]
        all: bool,
    },
    /// Snapshot operations. v0.4 cp 3.2.2.
    Snapshot {
        #[command(subcommand)]
        op: SnapshotOp,
    },
}

#[derive(Subcommand)]
enum SnapshotOp {
    /// Create a hot snapshot via the running server. Requires the
    /// server to be reachable and (if configured) an authenticated
    /// session via XYZDB_TOKEN. Snapshot lands at
    /// `<server data dir>/snapshots/<name>/`.
    Create {
        /// Snapshot name. Becomes the directory name under
        /// `snapshots/`. No path separators or ".." components.
        name: String,
    },
    /// Restore a snapshot OFFLINE (server must NOT be running against
    /// the source data dir while restoring). Hard-links SSTs and
    /// copies MANIFEST + WAL into the target dir; fails with a
    /// cross-filesystem error if source and target are on different
    /// mounts. After restore, point a fresh xyzdb-server at the
    /// target with `--path <target>` and the engine recovers normally.
    Restore {
        /// Source data dir (the dir containing `snapshots/<name>/`).
        #[arg(long)]
        source: PathBuf,
        /// Snapshot name within `<source>/snapshots/`.
        name: String,
        /// Target directory. Must not exist or be empty (refuses to
        /// clobber). Will be on the same filesystem as `<source>`
        /// (hard-link requirement).
        #[arg(long)]
        target: PathBuf,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum BulkmodeState {
    On,
    Off,
}

const PROTOCOL_VERSION: u8 = 1;
const STATUS_OK: u8 = 0x00;
/// Bearer-token auth preamble marker (server side: protocol::AUTH_MAGIC).
/// Sent before the protocol version byte when XYZDB_TOKEN is set.
const AUTH_MAGIC: u8 = 0x41;

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let addr = format!("{}:{}", args.host, args.port);

    match args.command {
        None => run_repl(&addr).await,
        Some(Command::Admin { verb }) => run_admin(&addr, verb).await,
    }
}

async fn run_repl(addr: &str) {
    let mut stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to connect to {addr}: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = send_auth_if_set(&mut stream).await {
        eprintln!("Auth frame error: {e}");
        std::process::exit(1);
    }

    println!("Connected to xyzDB at {addr}\n");

    let mut editor = DefaultEditor::new().expect("failed to create editor");
    let history_path = dirs_home().join(".xyzdb_history");
    let _ = editor.load_history(&history_path);

    loop {
        let line = match editor.readline("xyzdb> ") {
            Ok(line) => line,
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                println!("Bye.");
                break;
            }
            Err(e) => {
                eprintln!("Input error: {e}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let _ = editor.add_history_entry(trimmed);

        // Handle local commands
        if trimmed.eq_ignore_ascii_case("quit") || trimmed.eq_ignore_ascii_case("exit") {
            println!("Bye.");
            break;
        }

        // Send request
        if let Err(e) = send_query(&mut stream, trimmed).await {
            eprintln!("Send error: {e}");
            break;
        }

        // Read response
        match read_response(&mut stream).await {
            Ok((status, payload)) => {
                if status == STATUS_OK {
                    if !payload.is_empty() {
                        println!("{payload}");
                    }
                } else {
                    eprintln!("{payload}");
                }
            }
            Err(e) => {
                eprintln!("Receive error: {e}");
                break;
            }
        }
    }

    let _ = editor.save_history(&history_path);
}

/// Single-shot admin command: connect, send the equivalent xyTalk,
/// print response, exit. The CLI does NOT implement any admin logic
/// itself — it is a thin operator-grade wrapper that routes to the
/// running server's existing admin paths. This is the canonical entry
/// point; the language-statement form (`COMPACT`, `ANALYZE "x"`, …) stays
/// accepted as a permanent alias.
async fn run_admin(addr: &str, verb: AdminVerb) {
    // v0.4 cp 3.2.2: snapshot restore is offline — no server contact.
    if let AdminVerb::Snapshot {
        op:
            SnapshotOp::Restore {
                source,
                name,
                target,
            },
    } = &verb
    {
        let snapshot_dir = source.join("snapshots").join(name);
        if !snapshot_dir.exists() {
            eprintln!(
                "xyzdb-cli admin snapshot restore: snapshot dir not found: {}",
                snapshot_dir.display()
            );
            std::process::exit(2);
        }
        match turba_engine::snapshot::restore_snapshot(&snapshot_dir, target) {
            Ok(()) => {
                println!(
                    "Restored snapshot '{}' to {}.\nNext: start a server with --path {} to bring the engine online.",
                    name,
                    target.display(),
                    target.display()
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("xyzdb-cli admin snapshot restore: {e}");
                std::process::exit(1);
            }
        }
    }

    let xytalk = match verb {
        AdminVerb::Compact => "COMPACT".to_string(),
        AdminVerb::Analyze { lobe } => format!("ANALYZE {}", quote(&lobe)),
        AdminVerb::Bulkmode { state } => match state {
            BulkmodeState::On => "BULKMODE ON".to_string(),
            BulkmodeState::Off => "BULKMODE OFF".to_string(),
        },
        AdminVerb::Migrate { lobe, all } => match (lobe, all) {
            (Some(name), false) => format!("MIGRATE {}", quote(&name)),
            (None, true) => "MIGRATE".to_string(),
            (None, false) => {
                eprintln!(
                    "xyzdb-cli admin migrate: provide a lobe name or pass --all to migrate every lobe."
                );
                std::process::exit(2);
            }
            (Some(_), true) => {
                // clap's `conflicts_with` rejects this case before we get here;
                // unreachable in practice but kept defensive.
                eprintln!("xyzdb-cli admin migrate: --all conflicts with a lobe name.");
                std::process::exit(2);
            }
        },
        AdminVerb::Snapshot {
            op: SnapshotOp::Create { name },
        } => {
            // v0.4 cp 3.2.2: snapshot create goes through the running
            // server (hot snapshot). Wire form: `SNAPSHOT CREATE <name>`.
            // The server-side short-circuit calls Engine::create_snapshot
            // and returns a JSON SnapshotMeta on success.
            format!("SNAPSHOT CREATE {}", quote(&name))
        }
        // The Restore arm is handled offline above — clap exhaustiveness
        // requires this match arm even though it is unreachable here.
        AdminVerb::Snapshot {
            op: SnapshotOp::Restore { .. },
        } => unreachable!(),
    };

    let mut stream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to connect to {addr}: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = send_auth_if_set(&mut stream).await {
        eprintln!("Auth frame error: {e}");
        std::process::exit(1);
    }

    if let Err(e) = send_query(&mut stream, &xytalk).await {
        eprintln!("Send error: {e}");
        std::process::exit(1);
    }
    match read_response(&mut stream).await {
        Ok((status, payload)) => {
            if status == STATUS_OK {
                if !payload.is_empty() {
                    println!("{payload}");
                }
                std::process::exit(0);
            } else {
                eprintln!("{payload}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Receive error: {e}");
            std::process::exit(1);
        }
    }
}

/// Quote a lobe name for the wire. Lobe names commonly contain only
/// `[A-Za-z0-9_]`, so a naive `"{name}"` wrapper is correct; the inner
/// xyTalk parser also accepts unquoted identifiers but we always quote
/// to keep the wire string unambiguous.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

async fn send_query(stream: &mut TcpStream, query: &str) -> std::io::Result<()> {
    let payload = query.as_bytes();
    stream.write_u8(PROTOCOL_VERSION).await?;
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

/// v0.4 cp 2.2.2: send the bearer-token preamble if `XYZDB_TOKEN` is set.
/// No-op when unset. Wire format mirrors `xyzdb-server::protocol`:
/// `[AUTH_MAGIC=0x41][token_len: u16 BE][token: UTF-8]`.
async fn send_auth_if_set(stream: &mut TcpStream) -> std::io::Result<()> {
    let token = match std::env::var("XYZDB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return Ok(()),
    };
    let bytes = token.as_bytes();
    if bytes.len() > 4096 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "XYZDB_TOKEN exceeds 4096 bytes",
        ));
    }
    stream.write_u8(AUTH_MAGIC).await?;
    stream.write_u16(bytes.len() as u16).await?;
    stream.write_all(bytes).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_response(stream: &mut TcpStream) -> std::io::Result<(u8, String)> {
    let status = stream.read_u8().await?;
    let length = stream.read_u32().await?;

    let mut buf = vec![0u8; length as usize];
    stream.read_exact(&mut buf).await?;

    let payload = String::from_utf8(buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}")))?;

    Ok((status, payload))
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}
