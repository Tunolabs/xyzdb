// SPDX-License-Identifier: BUSL-1.1
use std::fmt;
use std::io;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    ChecksumMismatch,
    InvalidMagic,
    InvalidHeader,
    Decompress(String),
    InvalidEntry(String),
    Corruption(String),
    /// On-disk format predates the running build. Distinct from `Corruption` so
    /// operators see a migration instruction rather than a "your data is broken"
    /// message when they try to open a v0.1 data directory with a v0.2 binary.
    IncompatibleFormat {
        found: u8,
        expected: u8,
    },
    /// Write backpressure: too many sealed memtables or L0 tables pending.
    Overloaded,
    /// Snapshot with this name already exists in `snapshots/<name>/`.
    /// v0.4 cp 3.2.1.
    SnapshotExists(String),
    /// Restore failed because the named snapshot dir does not exist or
    /// is not a valid snapshot (missing `snapshot.meta`). v0.4 cp 3.2.2.
    SnapshotNotFound(String),
    /// Snapshot name is not a single safe path component — it contains a
    /// separator, a `..`/`.` component, or is empty/absolute. Rejected
    /// BEFORE any filesystem join so a crafted name cannot escape the
    /// `snapshots/` directory (path-traversal hardening, S3).
    InvalidSnapshotName(String),
    /// Restore failed because source and target are on different
    /// filesystems and hard-linking is required. v0.4 cp 3.2.2.
    SnapshotCrossFilesystem {
        src: String,
        dst: String,
    },
    /// Engine configuration rejected at startup
    /// (e.g. a knob out of range). The message is the operator-facing
    /// `ConfigError::Display` formatted in `EngineConfig::validate`.
    Config(String),
    /// A `major_compact` run exceeded its write-amplification ceiling
    /// (`LeveledConfig::max_compaction_amplification`) without converging —
    /// the compaction kept consuming inputs without draining the level
    /// structure. Surfaced (rather than spinning silently) so a
    /// non-convergent COMPACT fails fast with a level-by-level diagnosis
    /// instead of hanging for hours. The message carries the tree label,
    /// iteration, inputs/outputs counters, and the per-level table histogram.
    CompactionStalled(String),
    /// `rotate_journal` refused to truncate the WAL because a keyspace still
    /// holds acked writes not yet in an SSTable. `rotate()` truncates
    /// unconditionally, so truncating here would silently drop that keyspace's
    /// tail on the next crash. Surfaced (rather than losing data) so a caller
    /// that flushed only a SUBSET of keyspaces before rotating fails loudly —
    /// the guard against the compact-skips-vectors class of bug.
    WalRotatePrecondition(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::ChecksumMismatch => write!(f, "checksum mismatch"),
            Self::InvalidMagic => write!(f, "invalid magic bytes"),
            Self::InvalidHeader => write!(f, "invalid block header"),
            Self::Decompress(msg) => write!(f, "decompression error: {msg}"),
            Self::InvalidEntry(msg) => write!(f, "invalid entry: {msg}"),
            Self::Corruption(msg) => write!(f, "data corruption: {msg}"),
            Self::IncompatibleFormat { found, expected } => write!(
                f,
                "incompatible on-disk format: found version {found}, this build expects version {expected}. \
                 The spatial key layout changed, so keys written under the older version cannot be \
                 decoded by this build and there is no in-place migration. Recreate the dataset from \
                 source into a fresh data directory. Every xyzDB release from 1.0 onwards writes \
                 version 5, so a released build never produces a directory this build refuses."
            ),
            Self::Overloaded => write!(f, "write backpressure: engine overloaded"),
            Self::SnapshotExists(name) => {
                write!(
                    f,
                    "snapshot '{name}' already exists; pick a different name or delete the existing one"
                )
            }
            Self::SnapshotNotFound(name) => {
                write!(
                    f,
                    "snapshot '{name}' not found or missing snapshot.meta sidecar"
                )
            }
            Self::InvalidSnapshotName(name) => write!(
                f,
                "invalid snapshot name {name:?}: must be a single path component \
                 (no '/', '\\', '..', '.', or leading separator)"
            ),
            Self::SnapshotCrossFilesystem { src, dst } => write!(
                f,
                "snapshot source {src} and target {dst} are on different filesystems; hard-link not supported across mounts. \
                 Restore must target a directory on the same filesystem as the snapshot."
            ),
            Self::Config(msg) => write!(f, "{msg}"),
            Self::CompactionStalled(msg) => write!(f, "compaction did not converge: {msg}"),
            Self::WalRotatePrecondition(msg) => {
                write!(f, "WAL rotate precondition violated: {msg}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
