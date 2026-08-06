//! Hot snapshot/restore. v0.4 cp 3.2.1 + 3.2.2.
//!
//! ## Design
//!
//! A snapshot is a point-in-time view of the database that survives
//! independently of the live data dir. It is created by:
//!
//!   1. Acquiring the journal mutex (writers contending now block; this
//!      is the start of the writer-blocking window the gate measures).
//!   2. Disabling background compaction on every tree (atomic flag).
//!      In-flight compactions complete; new ones don't start. This
//!      keeps SST files referenced by the current MANIFEST alive while
//!      we hard-link them.
//!   3. Sealing the active memtable on every tree. Atomic version swap;
//!      microseconds. New writes after the seal go to a new active
//!      memtable; existing in-flight writes have already passed the
//!      WAL write and will land in the snapshot via the WAL.
//!   4. Forcing a `journal.sync()` so the snapshot's captured WAL bytes
//!      are durable on disk before we copy them.
//!   5. For each tree: hard-linking every SST in the live SuperVersion
//!      into `snapshots/<name>/<keyspace>/`, and copying that tree's
//!      MANIFEST file. Hard-link survives the original being unlinked
//!      by a future compaction (POSIX inode reference counting).
//!   6. Copying the WAL file (`journal.wal`) to
//!      `snapshots/<name>/journal.wal`. Typical WAL is < 16 MB so the
//!      copy is sub-ms; the snapshot is point-in-time consistent
//!      because the journal mutex is held throughout.
//!   7. Writing `snapshots/<name>/snapshot.meta` with provenance + the
//!      list of captured SSTs and per-tree manifest paths.
//!   8. Re-enabling compaction. Releasing the journal mutex.
//!
//! ## Restore
//!
//! [`restore_snapshot`] is offline (no engine handle): it copies the
//! snapshot directory into a target dir (hard-link if same FS, copy
//! otherwise — but cross-FS for SST files errors explicitly because
//! it would multiply disk usage by 2). The next `Engine::open(target)`
//! does normal WAL replay + SST manifest reading and the engine ends
//! up at the same logical state the snapshot captured.
//!
//! ## BULKMODE caveat
//!
//! When `--throttle-profile bulk` (or any state where compaction is
//! disabled at snapshot start), `WriteBatch::commit` SKIPS the WAL
//! write. The captured WAL would then be missing recent writes. To
//! preserve consistency, [`Engine::create_snapshot`] forces
//! `flush_sealed()` on each tree when it observes
//! `compaction_enabled == false` BEFORE the snapshot — accepting a
//! longer writer-blocking window in this case in exchange for a
//! consistent snapshot. The OPERATIONS.md "Backup" section
//! recommends pausing bulk loads before snapshotting.

// SPDX-License-Identifier: BUSL-1.1
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Filename of the per-snapshot metadata sidecar inside `snapshots/<name>/`.
pub const SNAPSHOT_META_FILE: &str = "snapshot.meta";

/// Filename of the captured WAL inside the snapshot dir.
pub const SNAPSHOT_WAL_FILE: &str = "journal.wal";

/// Format version of the `snapshot.meta` sidecar. Bumped on breaking
/// schema changes.
const SNAPSHOT_META_FORMAT: u8 = 1;

const SNAPSHOT_META_MAGIC: &[u8; 4] = b"XYSN";

/// Per-snapshot provenance + capture envelope. Serialised as JSON via
/// serde with a 5-byte prefix: `XYSN` magic + `[u8 = SNAPSHOT_META_FORMAT]`.
/// JSON is chosen over postcard for human readability — operators
/// inspect snapshot.meta during incidents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMeta {
    /// Operator-supplied name (also the directory name).
    pub name: String,
    /// Unix epoch milliseconds at snapshot capture.
    pub created_at_ms: u64,
    /// Engine schema version captured (currently
    /// `MANIFEST_VERSION` = 3, surfaced for forward-compat).
    pub manifest_version: u8,
    /// Per-keyspace SST hard-link inventory. Each entry:
    /// `(keyspace, sst_filename)`. Used by `restore_snapshot` to discover
    /// what to re-link/copy without re-reading the manifest.
    #[serde(default)]
    pub keyspaces: Vec<KeyspaceCapture>,
    /// Length of `journal.wal` at capture time. Useful as a sanity
    /// check during restore (the file copied must match this size).
    pub wal_bytes: u64,
    /// Whether the snapshot was taken while any tree had
    /// compaction_enabled == false (BULKMODE). True means the
    /// snapshot path forced flush_sealed() before capture and the
    /// writer-blocking window was extended. False is the fast path.
    pub bulkmode_at_capture: bool,
    /// Wall-clock duration of the snapshot lock window in
    /// microseconds. Operator metric: should be < 100 000 (= 100 ms)
    /// per cycle plan §3 Bloque 3 acceptance gate.
    pub lock_window_us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyspaceCapture {
    /// Keyspace name (e.g. `"spatial"`, `"identity"`, `"dictionary"`,
    /// `"ghosts"`).
    pub keyspace: String,
    /// SST filenames, relative to `snapshots/<name>/<keyspace>/`.
    pub sst_filenames: Vec<String>,
}

/// Write the snapshot.meta sidecar at `snapshot_dir/snapshot.meta`.
pub fn write_snapshot_meta(snapshot_dir: &Path, meta: &SnapshotMeta) -> Result<()> {
    let path = snapshot_dir.join(SNAPSHOT_META_FILE);
    let payload = serde_json::to_vec_pretty(meta)
        .map_err(|e| Error::Corruption(format!("snapshot meta serialize: {e}")))?;
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(SNAPSHOT_META_MAGIC);
    out.push(SNAPSHOT_META_FORMAT);
    out.extend_from_slice(&payload);

    let tmp = snapshot_dir.join(format!("{SNAPSHOT_META_FILE}.tmp"));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(&out)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, &path)?;

    #[cfg(unix)]
    if let Ok(d) = fs::File::open(snapshot_dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Read and validate the snapshot.meta sidecar.
pub fn read_snapshot_meta(snapshot_dir: &Path) -> Result<SnapshotMeta> {
    let path = snapshot_dir.join(SNAPSHOT_META_FILE);
    if !path.exists() {
        return Err(Error::SnapshotNotFound(
            snapshot_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("<unknown>")
                .to_string(),
        ));
    }
    let mut f = fs::File::open(&path)?;
    let mut buf = Vec::with_capacity(1024);
    f.read_to_end(&mut buf)?;
    if buf.len() < 5 {
        return Err(Error::Corruption("snapshot.meta too small".into()));
    }
    if &buf[0..4] != SNAPSHOT_META_MAGIC {
        return Err(Error::InvalidMagic);
    }
    if buf[4] != SNAPSHOT_META_FORMAT {
        return Err(Error::IncompatibleFormat {
            found: buf[4],
            expected: SNAPSHOT_META_FORMAT,
        });
    }
    serde_json::from_slice::<SnapshotMeta>(&buf[5..])
        .map_err(|e| Error::Corruption(format!("snapshot meta parse: {e}")))
}

/// Restore a snapshot into `target_dir`. Offline (no engine handle).
///
/// Behaviour:
/// - Reads `snapshot.meta` from `snapshot_dir` (errors with
///   [`Error::SnapshotNotFound`] if missing).
/// - Creates `target_dir` if it does not exist; errors if it exists
///   AND is non-empty (refuses to clobber an existing data dir — the
///   operator must explicitly clear it first).
/// - Hard-links every SST file from `snapshot_dir/<keyspace>/*.sst` to
///   `target_dir/<keyspace>/*.sst` when both are on the same
///   filesystem; errors with [`Error::SnapshotCrossFilesystem`]
///   otherwise (there is no copy fallback: a cross-filesystem snapshot
///   would silently cost a full data copy, so the operator is told
///   instead of charged for it).
/// - Copies each per-keyspace MANIFEST and the captured `journal.wal`
///   into the target dir.
///
/// After [`restore_snapshot`] returns Ok, the operator runs
/// `Engine::open(target_dir)` to bring the engine online; the open
/// path does normal WAL replay (recovering any sealed-but-unflushed
/// writes captured at snapshot time) and surface checks (manifest +
/// SST decoding). No special restore-mode in `Engine::open`.
pub fn restore_snapshot(snapshot_dir: &Path, target_dir: &Path) -> Result<()> {
    let meta = read_snapshot_meta(snapshot_dir)?;

    // target_dir must be empty (or not exist).
    if target_dir.exists() {
        let mut entries = fs::read_dir(target_dir)?;
        if entries.next().is_some() {
            return Err(Error::Corruption(format!(
                "target dir {} is not empty; refusing to clobber. Move or clear it first.",
                target_dir.display()
            )));
        }
    } else {
        fs::create_dir_all(target_dir)?;
    }

    // Helper: hard-link with cross-FS detection.
    let link_or_err = |src: &Path, dst: &Path| -> Result<()> {
        match fs::hard_link(src, dst) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                Err(Error::SnapshotCrossFilesystem {
                    src: src.display().to_string(),
                    dst: dst.display().to_string(),
                })
            }
            Err(e) => Err(Error::Io(e)),
        }
    };

    // Per-keyspace: create dir, hard-link SSTs, copy MANIFEST.
    for ks in &meta.keyspaces {
        let src_ks_dir = snapshot_dir.join(&ks.keyspace);
        let dst_ks_dir = target_dir.join(&ks.keyspace);
        fs::create_dir_all(&dst_ks_dir)?;
        for sst in &ks.sst_filenames {
            link_or_err(&src_ks_dir.join(sst), &dst_ks_dir.join(sst))?;
        }
        // MANIFEST is small and getting copied; could hard-link too,
        // but copying is safer (engine writes a new MANIFEST during
        // open which would corrupt the snapshot if hard-linked).
        let src_manifest = src_ks_dir.join("MANIFEST");
        let dst_manifest = dst_ks_dir.join("MANIFEST");
        if src_manifest.exists() {
            fs::copy(&src_manifest, &dst_manifest)?;
        }
    }

    // Copy the WAL into the target. Same reasoning: engine will write
    // to it during open recovery, so a hard-link would propagate writes
    // back into the snapshot dir.
    let src_wal = snapshot_dir.join(SNAPSHOT_WAL_FILE);
    let dst_wal = target_dir.join(SNAPSHOT_WAL_FILE);
    if src_wal.exists() {
        let copied_bytes = fs::copy(&src_wal, &dst_wal)?;
        if copied_bytes != meta.wal_bytes {
            return Err(Error::Corruption(format!(
                "WAL size mismatch: snapshot.meta says {} bytes, copied {} bytes",
                meta.wal_bytes, copied_bytes
            )));
        }
    }

    Ok(())
}

/// Read a directory entry list, returning relative SST filenames sorted
/// alphabetically. Helper for capture; the order is irrelevant for
/// correctness because the manifest is the source of truth — the
/// captured filename list in `snapshot.meta` exists only as an
/// inventory.
pub(crate) fn list_sst_files(dir: &Path) -> Result<Vec<String>> {
    let mut out: Vec<String> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".sst") {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Append `Path` for the snapshots root inside a data dir.
pub(crate) fn snapshots_root(data_dir: &Path) -> PathBuf {
    data_dir.join("snapshots")
}

/// Reject a snapshot name that is not a single, safe path component, BEFORE it
/// is ever joined onto `snapshots/` (path-traversal hardening, S3). A crafted
/// name like `../../etc/x`, `a/b`, `..`, `/abs`, or `""` would otherwise let an
/// untrusted or automated caller escape the snapshots directory and create or
/// overwrite files elsewhere.
///
/// # Errors
/// Returns [`Error::InvalidSnapshotName`] unless `name` is exactly one
/// `Component::Normal` with no separator, `..`/`.`, or root prefix.
pub(crate) fn validate_snapshot_name(name: &str) -> Result<()> {
    use std::path::Component;
    let mut comps = Path::new(name).components();
    let single_normal =
        matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none();
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || !single_normal
    {
        return Err(Error::InvalidSnapshotName(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn snapshot_meta_roundtrip() {
        let dir = TempDir::new().unwrap();
        let original = SnapshotMeta {
            name: "backup-001".into(),
            created_at_ms: 1_715_000_000_000,
            manifest_version: 3,
            keyspaces: vec![KeyspaceCapture {
                keyspace: "spatial".into(),
                sst_filenames: vec!["00000001.sst".into(), "00000002.sst".into()],
            }],
            wal_bytes: 4096,
            bulkmode_at_capture: false,
            lock_window_us: 12_345,
        };
        write_snapshot_meta(dir.path(), &original).unwrap();
        let parsed = read_snapshot_meta(dir.path()).unwrap();
        assert_eq!(parsed.name, original.name);
        assert_eq!(parsed.created_at_ms, original.created_at_ms);
        assert_eq!(parsed.keyspaces.len(), 1);
        assert_eq!(parsed.keyspaces[0].sst_filenames.len(), 2);
        assert_eq!(parsed.wal_bytes, 4096);
        assert!(!parsed.bulkmode_at_capture);
        assert_eq!(parsed.lock_window_us, 12_345);
    }

    #[test]
    fn missing_snapshot_meta_returns_not_found() {
        let dir = TempDir::new().unwrap();
        match read_snapshot_meta(dir.path()) {
            Err(Error::SnapshotNotFound(_)) => {}
            other => panic!("expected SnapshotNotFound, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_magic_returns_invalid_magic() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(SNAPSHOT_META_FILE), b"NOPE\x01{}").unwrap();
        match read_snapshot_meta(dir.path()) {
            Err(Error::InvalidMagic) => {}
            other => panic!("expected InvalidMagic, got {other:?}"),
        }
    }
}
