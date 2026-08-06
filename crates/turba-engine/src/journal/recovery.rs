//! Journal recovery: read WAL segments in seqno order, parse valid batches,
//! replay into memtables.

// SPDX-License-Identifier: BUSL-1.1
use crate::error::Result;
use crate::journal::entry::{RecoveredBatch, decode_batches};
use std::fs;
use std::path::{Path, PathBuf};

/// Read and decode valid batches from the WAL: every archived segment
/// (`journal.<seqno>.wal`, in ascending seqno order) followed by the active
/// segment (`path`, i.e. `journal.wal`). Seqnos are globally monotonic and
/// segments roll in order, so this yields the full ordered batch stream.
/// A single legacy `journal.wal` with no archived segments recovers exactly
/// as before. Returns empty vec if nothing exists.
pub fn recover_journal(path: &Path) -> Result<Vec<RecoveredBatch>> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));

    // Collect archived segments (journal.<n>.wal), excluding the active journal.wal.
    let mut archived: Vec<(u64, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if let Some(seq) = name
                    .strip_prefix("journal.")
                    .and_then(|s| s.strip_suffix(".wal"))
                    .and_then(|m| m.parse::<u64>().ok())
                {
                    archived.push((seq, p));
                }
            }
        }
    }
    archived.sort_by_key(|(s, _)| *s);

    let mut out: Vec<RecoveredBatch> = Vec::new();
    for (_, seg) in &archived {
        let data = fs::read(seg)?;
        if !data.is_empty() {
            out.extend(decode_batches(&data));
        }
    }
    // Active segment last.
    if path.exists() {
        let data = fs::read(path)?;
        if !data.is_empty() {
            out.extend(decode_batches(&data));
        }
    }
    Ok(out)
}
