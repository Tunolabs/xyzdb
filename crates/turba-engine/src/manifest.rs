//! Manifest: atomic persistence of the LSM version (which SSTables exist at each level).
//!
//! Single file MANIFEST, atomically updated via write-temp → fsync → rename.
//!
//! Format:
//! ```text
//! [magic: "XYZM" (4 bytes)]
//! [version: u8 = 1]
//! [level_count: u8]
//! [next_table_id: u64 LE]
//! [next_seqno: u64 LE]
//! for each level:
//!   [table_count: u32 LE]
//!   for each table:
//!     [table_id: u64 LE]
//!     [path_len: u16 LE]
//!     [path_bytes: path_len]
//!     [key_min_len: u16 LE]
//!     [key_min: key_min_len]
//!     [key_max_len: u16 LE]
//!     [key_max: key_max_len]
//!     [seqno_min: u64 LE]
//!     [seqno_max: u64 LE]
//!     [item_count: u64 LE]
//!     [file_size: u64 LE]
//! [checksum: u128 LE (XXH3-128 of everything above)]
//! ```

// SPDX-License-Identifier: BUSL-1.1
use crate::error::{Error, Result};
use crate::tree::version::Version;
use byteorder_lite::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;

const MANIFEST_MAGIC: &[u8; 4] = b"XYZM";
/// Bumped from 1 to 2 in v0.2 to force operators to delete v0.1 data dirs.
/// v0.2 introduces GhostType + TTL metadata on ghost entries and a router
/// schema change that is not readable by v0.1 persistence code. There are no
/// v0.1 production users, so no migration path is provided.
/// Manifest format version.
///
/// History:
///   1 → 2 (v0.2.0-alpha): rejected v0.1 data directories.
///   2 → 3 (v0.2.1-dev):   rejected v0.2.0-alpha data directories.
///                          Required because `SSTableMeta` encoding for
///                          tag 12 (zone_maps) changed its length width
///                          from u16 to u32 to fix silent truncation on
///                          compacted L1/L2+ SSTables with ≥ ~2 K blocks
///                          (Finding 4). On-disk artifacts written by
///                          v0.2.0-alpha cannot be correctly decoded by
///                          this build, and vice versa.
/// MANIFEST format version. Bumped 3 → 4 in v0.6.0-pre to mark the
/// `gravity_hash` 21 → 48 bit width change inside `SpatialKey`.
///   4 → 5 (0.9.4): the `SpatialKey` widened 22 → 24 bytes to reserve a
///                   2-byte satellite (sub-gravity) axis at bytes 8..10
///                   (`crates/core/src/key.rs`). The axis is always 0 today
///                   — behaviour is identical to the 22-byte layout — but
///                   the byte offsets of `z_order_2d`/`seq` moved, so v4
///                   keys are not decodable by this build.
/// Data dirs on any earlier version are rejected at open with a clear
/// migration message; recreate the dataset from source (there is no
/// in-place widening — reserving the axis while there are no users is the
/// whole point). No incremental migration tool is provided.
const MANIFEST_VERSION: u8 = 5;
const MANIFEST_FILE: &str = "MANIFEST";

/// Persist the current version to disk atomically.
pub fn write_manifest(
    dir: &Path,
    version: &Version,
    next_table_id: u64,
    next_seqno: u64,
) -> Result<()> {
    let data = encode_manifest(version, next_table_id, next_seqno);

    // Checksum the entire payload
    let checksum = xxhash_rust::xxh3::xxh3_128(&data);

    let tmp_path = dir.join("MANIFEST.tmp");
    let final_path = dir.join(MANIFEST_FILE);

    // Write to temp file
    let mut file = fs::File::create(&tmp_path)?;
    file.write_all(&data)?;
    file.write_all(&checksum.to_le_bytes())?;
    file.sync_all()?; // fsync

    // Atomic rename
    fs::rename(&tmp_path, &final_path)?;

    // fsync the directory so the rename is durable. 3g: a failed dir-fsync
    // means the rename may not survive power loss — propagate it instead of
    // swallowing (the old `let _` left a stale-MANIFEST window on recovery).
    fsync_dir(dir)?;

    Ok(())
}

/// Test-only: force [`fsync_dir`] to fail, to exercise the 3g
/// error-propagation path without a real disk fault.
#[cfg(feature = "durability-test-hooks")]
pub static FORCE_DIR_FSYNC_ERROR: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// fsync the directory `dir` so a prior `rename` into it is durable. On
/// filesystems that do not journal renames (ext4 `data=writeback`, xfs
/// default) the rename can be lost on power loss until the directory entry
/// is itself synced.
///
/// # Errors
/// Propagates the open / fsync error. A failed directory fsync means the
/// rename may not survive a crash, so the caller MUST treat it as a write
/// failure (3g) — silently ignoring it was the original bug.
// Returns `io::Result` so both callers (manifest → `crate::error::Error`,
// placement → `PlacementMapError`) convert the error via their own `?`.
#[cfg(unix)]
pub(crate) fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(feature = "durability-test-hooks")]
    if FORCE_DIR_FSYNC_ERROR.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "injected directory fsync failure (test hook)",
        ));
    }
    let dir_fd = fs::File::open(dir)?;
    dir_fd.sync_all()?;
    Ok(())
}

/// Non-Unix: directory fsync is not portably available; no-op.
#[cfg(not(unix))]
pub(crate) fn fsync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Read and validate the manifest. Returns (Version skeleton, next_table_id, next_seqno).
/// The caller must open each SSTable's reader separately.
pub fn read_manifest(dir: &Path) -> Result<Option<ManifestData>> {
    let path = dir.join(MANIFEST_FILE);
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read(&path)?;
    if raw.len() < 4 + 1 + 1 + 8 + 8 + 16 {
        return Err(Error::Corruption("manifest too small".into()));
    }

    // Validate checksum (last 16 bytes)
    let payload = &raw[..raw.len() - 16];
    let stored_checksum = u128::from_le_bytes(raw[raw.len() - 16..].try_into().unwrap());
    let computed = xxhash_rust::xxh3::xxh3_128(payload);
    if stored_checksum != computed {
        return Err(Error::ChecksumMismatch);
    }

    decode_manifest(payload)
}

pub struct ManifestData {
    pub next_table_id: u64,
    pub next_seqno: u64,
    pub levels: Vec<Vec<ManifestTableEntry>>,
}

#[derive(Debug, Clone)]
pub struct ManifestTableEntry {
    pub table_id: u64,
    pub path: String,
    pub key_min: Vec<u8>,
    pub key_max: Vec<u8>,
    pub seqno_min: u64,
    pub seqno_max: u64,
    pub item_count: u64,
    pub file_size: u64,
}

fn encode_manifest(version: &Version, next_table_id: u64, next_seqno: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4096);

    buf.extend_from_slice(MANIFEST_MAGIC);
    buf.push(MANIFEST_VERSION);
    buf.push(version.levels.len() as u8);
    buf.write_u64::<LittleEndian>(next_table_id).unwrap();
    buf.write_u64::<LittleEndian>(next_seqno).unwrap();

    for level in &version.levels {
        buf.write_u32::<LittleEndian>(level.len() as u32).unwrap();
        for table in level {
            buf.write_u64::<LittleEndian>(table.meta().table_id)
                .unwrap();

            let path_str = table.path.to_string_lossy();
            let path_bytes = path_str.as_bytes();
            buf.write_u16::<LittleEndian>(path_bytes.len() as u16)
                .unwrap();
            buf.extend_from_slice(path_bytes);

            buf.write_u16::<LittleEndian>(table.meta().key_min.len() as u16)
                .unwrap();
            buf.extend_from_slice(&table.meta().key_min);
            buf.write_u16::<LittleEndian>(table.meta().key_max.len() as u16)
                .unwrap();
            buf.extend_from_slice(&table.meta().key_max);

            buf.write_u64::<LittleEndian>(table.meta().seqno_min)
                .unwrap();
            buf.write_u64::<LittleEndian>(table.meta().seqno_max)
                .unwrap();
            buf.write_u64::<LittleEndian>(table.meta().item_count)
                .unwrap();
            buf.write_u64::<LittleEndian>(table.meta().file_size)
                .unwrap();
        }
    }

    buf
}

fn decode_manifest(data: &[u8]) -> Result<Option<ManifestData>> {
    let mut c = Cursor::new(data);

    let mut magic = [0u8; 4];
    std::io::Read::read_exact(&mut c, &mut magic)?;
    if &magic != MANIFEST_MAGIC {
        return Err(Error::InvalidMagic);
    }

    let version = c.read_u8()?;
    if version != MANIFEST_VERSION {
        return Err(Error::IncompatibleFormat {
            found: version,
            expected: MANIFEST_VERSION,
        });
    }

    let level_count = c.read_u8()? as usize;
    let next_table_id = c.read_u64::<LittleEndian>()?;
    let next_seqno = c.read_u64::<LittleEndian>()?;

    let mut levels = Vec::with_capacity(level_count);
    for _ in 0..level_count {
        let table_count = c.read_u32::<LittleEndian>()? as usize;
        let mut tables = Vec::with_capacity(table_count);

        for _ in 0..table_count {
            let table_id = c.read_u64::<LittleEndian>()?;

            let path_len = c.read_u16::<LittleEndian>()? as usize;
            let mut path_bytes = vec![0u8; path_len];
            std::io::Read::read_exact(&mut c, &mut path_bytes)?;
            let path = String::from_utf8(path_bytes)
                .map_err(|_| Error::Corruption("invalid path in manifest".into()))?;

            let key_min_len = c.read_u16::<LittleEndian>()? as usize;
            let mut key_min = vec![0u8; key_min_len];
            std::io::Read::read_exact(&mut c, &mut key_min)?;

            let key_max_len = c.read_u16::<LittleEndian>()? as usize;
            let mut key_max = vec![0u8; key_max_len];
            std::io::Read::read_exact(&mut c, &mut key_max)?;

            let seqno_min = c.read_u64::<LittleEndian>()?;
            let seqno_max = c.read_u64::<LittleEndian>()?;
            let item_count = c.read_u64::<LittleEndian>()?;
            let file_size = c.read_u64::<LittleEndian>()?;

            tables.push(ManifestTableEntry {
                table_id,
                path,
                key_min,
                key_max,
                seqno_min,
                seqno_max,
                item_count,
                file_size,
            });
        }
        levels.push(tables);
    }

    Ok(Some(ManifestData {
        next_table_id,
        next_seqno,
        levels,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Hand-write a minimally valid manifest of a given version byte.
    /// Used by both the v1 and v2 rejection tests so the fixture logic
    /// stays identical.
    fn write_versioned_manifest(dir: &TempDir, version: u8) {
        let mut payload = Vec::new();
        payload.extend_from_slice(MANIFEST_MAGIC);
        payload.push(version);
        payload.push(0u8); // level_count = 0
        payload.write_u64::<LittleEndian>(0).unwrap(); // next_table_id
        payload.write_u64::<LittleEndian>(0).unwrap(); // next_seqno

        let checksum = xxhash_rust::xxh3::xxh3_128(&payload);
        let mut file_bytes = payload;
        file_bytes.extend_from_slice(&checksum.to_le_bytes());
        fs::write(dir.path().join(MANIFEST_FILE), &file_bytes).unwrap();
    }

    /// A v0.1 data directory (manifest version byte = 1) must fail to open with
    /// `IncompatibleFormat`, not `Corruption`. Tests the operator-facing contract
    /// promised in the v0.2 release notes: "delete the data dir and re-ingest."
    #[test]
    fn v1_manifest_fails_with_incompatible_format() {
        let dir = TempDir::new().unwrap();
        write_versioned_manifest(&dir, 1);
        match read_manifest(dir.path()) {
            Err(Error::IncompatibleFormat { found, expected }) => {
                assert_eq!(found, 1);
                assert_eq!(expected, MANIFEST_VERSION);
            }
            Err(e) => panic!("expected IncompatibleFormat, got different error: {e:?}"),
            Ok(_) => panic!("expected IncompatibleFormat, got Ok"),
        }
    }

    /// A v0.2.0-alpha data directory (manifest version byte = 2) must also
    /// fail to open: the SSTable meta encoding for tag 12 (zone_maps)
    /// changed length width from u16 to u32 between v0.2.0-alpha and
    /// v0.2.1 (Finding 4). A v2 manifest implies v0.2.0-alpha SSTables
    /// that the new decoder cannot correctly interpret.
    #[test]
    fn v2_manifest_fails_with_incompatible_format() {
        let dir = TempDir::new().unwrap();
        write_versioned_manifest(&dir, 2);
        match read_manifest(dir.path()) {
            Err(Error::IncompatibleFormat { found, expected }) => {
                assert_eq!(found, 2);
                assert_eq!(expected, MANIFEST_VERSION);
            }
            Err(e) => panic!("expected IncompatibleFormat, got different error: {e:?}"),
            Ok(_) => panic!("expected IncompatibleFormat, got Ok"),
        }
    }

    /// Missing manifest (fresh data dir) is NOT an error — opens as `Ok(None)`.
    #[test]
    fn missing_manifest_is_fresh_dir() {
        let dir = TempDir::new().unwrap();
        let result = read_manifest(dir.path()).unwrap();
        assert!(result.is_none());
    }

    /// A v0.5.x data directory (manifest version byte = 3) must fail to
    /// open under the v0.6.0-pre binary: the gravity_hash bump from 21 to
    /// 48 bits changed the SpatialKey size from 18 to 22 bytes (cycle plan
    /// C.1). No incremental migration tool is provided; the documented
    /// path is to recreate the dataset.
    #[test]
    fn v3_manifest_fails_with_incompatible_format() {
        let dir = TempDir::new().unwrap();
        write_versioned_manifest(&dir, 3);
        match read_manifest(dir.path()) {
            Err(Error::IncompatibleFormat { found, expected }) => {
                assert_eq!(found, 3);
                assert_eq!(expected, MANIFEST_VERSION);
            }
            Err(e) => panic!("expected IncompatibleFormat, got different error: {e:?}"),
            Ok(_) => panic!("expected IncompatibleFormat, got Ok"),
        }
    }

    /// 3g regression: a failed directory fsync must PROPAGATE (was a swallowed
    /// `let _`). A swallowed dir-fsync = non-durable rename = stale MANIFEST on
    /// recovery. Uses the injection hook (no real disk fault needed).
    #[cfg(all(unix, feature = "durability-test-hooks"))]
    #[test]
    fn fsync_dir_propagates_injected_failure() {
        use std::sync::atomic::Ordering;
        let dir = TempDir::new().unwrap();

        // Control: a real directory fsync succeeds.
        FORCE_DIR_FSYNC_ERROR.store(false, Ordering::Relaxed);
        assert!(
            fsync_dir(dir.path()).is_ok(),
            "control: a clean dir-fsync succeeds"
        );

        // Injected failure must propagate, not be swallowed.
        FORCE_DIR_FSYNC_ERROR.store(true, Ordering::Relaxed);
        assert!(
            fsync_dir(dir.path()).is_err(),
            "a failed dir-fsync must propagate (3g), not be silently ignored"
        );
        FORCE_DIR_FSYNC_ERROR.store(false, Ordering::Relaxed); // reset for other tests
    }
}
