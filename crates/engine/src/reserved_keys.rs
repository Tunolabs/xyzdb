//! Single source of truth for every reserved dictionary-keyspace prefix.
//!
//! The dictionary `Tree` is shared by several internal keyspaces, each
//! claiming a 2-byte prefix in the `[0xFF, 0xXX]` block. Historically these
//! prefixes were declared as private consts scattered across `engine.rs`,
//! `ghost.rs`, `field_registry.rs` and `dict_encoding.rs`. That dispersion let
//! a "fix" silently move one keyspace onto another's prefix *twice*:
//! `PIN → ghost-meta` (pre-0.7.6) and then `PIN → field-registry` (0.7.6). Each
//! collision corrupted data invisibly because the colliding keyspaces also
//! share the on-disk value shape `[MAGIC][0x01][postcard]` — there is no type
//! byte to tell them apart at decode time, so the loser is read as the winner.
//!
//! Every keyspace now references its prefix from here, and
//! [`reserved_prefixes_do_not_collide`] enumerates the whole table and fails the
//! build if two keyspaces share a prefix without a mutually-exclusive
//! disambiguator. That test closes the class: it would have caught both past
//! collisions.

/// Pinned-fields keyspace, keyed by `lobe_id` (`[PIN][lobe_id:2]`). Value is
/// `[MAGIC][0x01][postcard(Vec<String>)]` — the same shape as [`FIELD_REGISTRY`],
/// so the two MUST never share a prefix. Moved here from `[0xFF,0xFB]` in 0.7.6
/// (which was [`FIELD_REGISTRY`]'s) and from `[0xFF,0xFD]` before that (which is
/// [`PIN_LEGACY`]/[`GHOST_META`]'s).
pub(crate) const PIN: [u8; 2] = [0xFF, 0xF8];

/// Ghost aggregate rollups, keyed by `[ROLLUP][ghost_id:2][group_key]`. One
/// canonical entry per (ghost, group). See `ghost.rs::rollup_key`.
pub(crate) const ROLLUP: [u8; 2] = [0xFF, 0xF9];

/// Metric-ordered rollups for a ghost declared with `ORDER BY <metric>`, keyed
/// by `[METRIC_ORDER][ghost_id:2][enc(metric):8][tiebreak]`. Sorts groups by an
/// aggregate metric (not by group key), so `TOP n BY <metric>` reads only the
/// first N entries — O(N) instead of the O(M) full rollup scan. Rebuilt in full
/// on each REFRESH/CREATE (blind-insert, then compaction sorts); the write path
/// is never touched. See `ghost::metric_order`.
pub(crate) const METRIC_ORDER: [u8; 2] = [0xFF, 0xF6];

/// Per-lobe gravity-field registry, keyed by `lobe_id`
/// (`[GRAVITY][lobe_id:2]`). See `engine.rs` Finding 13.
pub(crate) const GRAVITY: [u8; 2] = [0xFF, 0xFA];

/// Per-lobe searchable-vector-field registry, keyed by `lobe_id`
/// (`[VECTOR_FIELD][lobe_id:2]`). Value is `[MAGIC][0x01][postcard(VectorSpec)]`.
/// Sibling axis to [`GRAVITY`] (placement) — this one names the embedding field
/// hoisted to the V3 record prefix for exact NEAREST. Not an index/IVF.
pub(crate) const VECTOR_FIELD: [u8; 2] = [0xFF, 0xF7];

/// Per-lobe V2 field-name registry, keyed by `lobe_id`
/// (`[FIELD_REGISTRY][lobe_id:2]`). Value shape is identical to [`PIN`].
pub(crate) const FIELD_REGISTRY: [u8; 2] = [0xFF, 0xFB];

/// Global boot-epoch counter. The key is *exactly* these two bytes — no
/// suffix — which is what length-disambiguates it from [`DICT`], whose keys
/// share the same prefix but are always longer. See `engine.rs::BOOT_EPOCH`.
pub(crate) const BOOT_EPOCH: [u8; 2] = [0xFF, 0xFC];

/// User dictionary keyspace, keys `[DICT][...]` (always longer than 2 bytes).
/// Shares its prefix with [`BOOT_EPOCH`] but is length-disambiguated.
pub(crate) const DICT: [u8; 2] = [0xFF, 0xFC];

/// Pre-0.7.6 pin prefix, kept read-only for boot migration. Shares its prefix
/// with [`GHOST_META`]; pin values carry format byte `0x01`, ghost-meta values
/// `0x03`, so they are value-format-byte disambiguated.
pub(crate) const PIN_LEGACY: [u8; 2] = [0xFF, 0xFD];

/// Ghost metadata, keyed by `ghost_id`. Shares its prefix with [`PIN_LEGACY`];
/// see that const for the format-byte disambiguation.
pub(crate) const GHOST_META: [u8; 2] = [0xFF, 0xFD];

/// Ghost write counters, keyed by `ghost_id` (`[GHOST_WRITES][ghost_id:2]`).
pub(crate) const GHOST_WRITES: [u8; 2] = [0xFF, 0xFE];

/// How a keyspace that shares a 2-byte prefix with another tells its entries
/// apart at decode time. Two keyspaces may legitimately share a prefix only if
/// each declares a disambiguator and the pair is mutually exclusive (see
/// `reserved_prefixes_do_not_collide`). Test-only scaffolding for that
/// invariant; the runtime contract is the prefix consts above.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Disambiguator {
    /// No other keyspace may share this prefix.
    Exclusive,
    /// Distinguished by the key being *exactly* the 2-byte prefix (no suffix).
    ExactTwoByteKey,
    /// Distinguished by the key being *longer* than the 2-byte prefix.
    LongerKey,
    /// Distinguished by the value's format byte at offset 2.
    ValueFormatByte(u8),
}

/// One reserved keyspace: its name (for diagnostics), prefix, and the rule that
/// keeps it distinct from any other keyspace sharing the same prefix.
#[cfg(test)]
pub(crate) struct ReservedKeyspace {
    pub name: &'static str,
    pub prefix: [u8; 2],
    pub disambiguator: Disambiguator,
}

/// The complete reserved-prefix table. Adding a keyspace means adding a row
/// here; `reserved_prefixes_do_not_collide` enforces non-collision.
#[cfg(test)]
pub(crate) const RESERVED: &[ReservedKeyspace] = &[
    ReservedKeyspace {
        name: "PIN",
        prefix: PIN,
        disambiguator: Disambiguator::Exclusive,
    },
    ReservedKeyspace {
        name: "ROLLUP",
        prefix: ROLLUP,
        disambiguator: Disambiguator::Exclusive,
    },
    ReservedKeyspace {
        name: "METRIC_ORDER",
        prefix: METRIC_ORDER,
        disambiguator: Disambiguator::Exclusive,
    },
    ReservedKeyspace {
        name: "GRAVITY",
        prefix: GRAVITY,
        disambiguator: Disambiguator::Exclusive,
    },
    ReservedKeyspace {
        name: "VECTOR_FIELD",
        prefix: VECTOR_FIELD,
        disambiguator: Disambiguator::Exclusive,
    },
    ReservedKeyspace {
        name: "FIELD_REGISTRY",
        prefix: FIELD_REGISTRY,
        disambiguator: Disambiguator::Exclusive,
    },
    ReservedKeyspace {
        name: "BOOT_EPOCH",
        prefix: BOOT_EPOCH,
        disambiguator: Disambiguator::ExactTwoByteKey,
    },
    ReservedKeyspace {
        name: "DICT",
        prefix: DICT,
        disambiguator: Disambiguator::LongerKey,
    },
    ReservedKeyspace {
        name: "PIN_LEGACY",
        prefix: PIN_LEGACY,
        disambiguator: Disambiguator::ValueFormatByte(0x01),
    },
    ReservedKeyspace {
        name: "GHOST_META",
        prefix: GHOST_META,
        disambiguator: Disambiguator::ValueFormatByte(0x03),
    },
    ReservedKeyspace {
        name: "GHOST_WRITES",
        prefix: GHOST_WRITES,
        disambiguator: Disambiguator::Exclusive,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns `true` if two keyspaces sharing a prefix can be told apart.
    fn mutually_exclusive(a: Disambiguator, b: Disambiguator) -> bool {
        use Disambiguator::*;
        match (a, b) {
            // An Exclusive owner sharing a prefix with anyone is the bug.
            (Exclusive, _) | (_, Exclusive) => false,
            // Exactly-2-bytes vs longer-than-2-bytes never overlap.
            (ExactTwoByteKey, LongerKey) | (LongerKey, ExactTwoByteKey) => true,
            // Two length rules of the same kind don't distinguish.
            (ExactTwoByteKey, ExactTwoByteKey) | (LongerKey, LongerKey) => false,
            // Distinct format bytes distinguish; equal ones don't.
            (ValueFormatByte(x), ValueFormatByte(y)) => x != y,
            // Length rule vs format-byte rule is not a reliable discriminator.
            _ => false,
        }
    }

    /// Enumerates every reserved keyspace and asserts that any two sharing a
    /// 2-byte prefix declare a mutually-exclusive disambiguator. This is the
    /// root-cause guard for the dictionary-namespace collision class: it fails
    /// the build the moment a keyspace is moved onto another's prefix. It would
    /// have caught both historical regressions (PIN→ghost-meta, PIN→field-registry).
    #[test]
    fn reserved_prefixes_do_not_collide() {
        for (i, a) in RESERVED.iter().enumerate() {
            for b in &RESERVED[i + 1..] {
                if a.prefix == b.prefix {
                    assert!(
                        mutually_exclusive(a.disambiguator, b.disambiguator),
                        "reserved prefix {:02X?} is shared by '{}' and '{}' \
                         without a mutually-exclusive disambiguator \
                         ({:?} vs {:?}) — this is the silent-corruption class \
                         (see reserved_keys.rs)",
                        a.prefix,
                        a.name,
                        b.name,
                        a.disambiguator,
                        b.disambiguator
                    );
                }
            }
        }
    }
}
