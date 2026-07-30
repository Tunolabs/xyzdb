//! Sort key encoding for Ghost V2.
//!
//! Encodes field values into bytes that preserve natural ordering in the LSM.
//! The ghost_id prefix ensures entries from different ghosts don't mix.
//! Optional inversion flips byte order for DESC queries via normal prefix_iter.
//!
//! A `tiebreak` suffix (the record's spatial key) makes every entry's key
//! unique: without it, two records sharing the same ORDER BY value collapse to
//! one LSM key and the second silently overwrites the first, so a covering
//! ghost returns a subset (engine audit P0-2). The value encoding is
//! prefix-free precisely so this suffix can never bleed into the comparison
//! between two distinct sort values.

use xyzdb_core::value::Value;

/// Encode a sort key: `[ghost_id:2][type_tag:1][value_bytes][tiebreak]`.
///
/// `tiebreak` is a per-record uniqueness suffix (the spatial key) that
/// disambiguates entries sharing the same sort value; pass `&[]` only when
/// uniqueness is not required (e.g. ordering-only unit tests). The value
/// encoding is prefix-free, so the tiebreak never affects ordering between
/// different sort values. With `inverted=true`, all bytes after `ghost_id`
/// (value AND tiebreak) are bitwise negated, making ASC iteration in the LSM
/// yield DESC order; insert and remove must pass the same `tiebreak` and
/// `inverted` so the exact key can be reconstructed for deletion.
pub fn encode_sort_key(
    ghost_id: u16,
    sort_value: Option<&Value>,
    inverted: bool,
    tiebreak: &[u8],
) -> Vec<u8> {
    let mut key = Vec::with_capacity(12 + tiebreak.len());
    key.extend_from_slice(&ghost_id.to_be_bytes());
    encode_value_ordered(&mut key, sort_value);
    key.extend_from_slice(tiebreak);

    if inverted {
        for byte in key[2..].iter_mut() {
            *byte = !*byte;
        }
    }

    key
}

/// Decode a sort key back to (ghost_id, raw_value_bytes).
/// Does NOT reconstruct the Value — only needed for debugging/tests.
#[cfg(test)]
pub fn decode_ghost_id(key: &[u8]) -> Option<u16> {
    if key.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([key[0], key[1]]))
}

fn encode_value_ordered(buf: &mut Vec<u8>, value: Option<&Value>) {
    match value {
        None | Some(Value::Null) => {
            buf.push(0x00); // Null sorts first
        }
        Some(Value::Int(i)) => {
            buf.push(0x01);
            // XOR sign bit for natural order: -5 < 0 < 42
            let encoded = (*i as u64) ^ (1u64 << 63);
            buf.extend_from_slice(&encoded.to_be_bytes());
        }
        Some(Value::Float(f)) => {
            buf.push(0x02);
            let bits = f.to_bits();
            // IEEE 754 order-preserving: flip sign for positive, flip all for negative
            let encoded = if bits & (1u64 << 63) != 0 {
                !bits
            } else {
                bits ^ (1u64 << 63)
            };
            buf.extend_from_slice(&encoded.to_be_bytes());
        }
        Some(Value::Text(s)) => {
            buf.push(0x03);
            // Prefix-free, order-preserving encoding so a tiebreak suffix can
            // follow safely. Without a terminator, "a"+suffix vs "ab" would
            // compare suffix[0] against 'b' and mis-order. Escape 0x00 as
            // 0x00 0xFF and terminate with 0x00 0x00: 0x00 0x00 sorts before
            // both an escaped zero (0x00 0xFF) and any real byte >= 0x01, so a
            // shorter string still sorts before a longer one that extends it,
            // and no encoded value is a prefix of another.
            for &b in s.as_bytes() {
                if b == 0x00 {
                    buf.push(0x00);
                    buf.push(0xFF);
                } else {
                    buf.push(b);
                }
            }
            buf.push(0x00);
            buf.push(0x00);
        }
        _ => {
            buf.push(0xFF); // Other types sort last
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ordering tests use an empty tiebreak: they assert the value ordering,
    // which the suffix must never perturb between distinct values.
    const NO_TB: &[u8] = &[];

    #[test]
    fn sort_key_null_first() {
        let null_key = encode_sort_key(1, None, false, NO_TB);
        let int_key = encode_sort_key(1, Some(&Value::Int(0)), false, NO_TB);
        assert!(null_key < int_key, "null should sort before any value");
    }

    #[test]
    fn sort_key_i64_ordering() {
        let k_neg = encode_sort_key(1, Some(&Value::Int(-100)), false, NO_TB);
        let k_zero = encode_sort_key(1, Some(&Value::Int(0)), false, NO_TB);
        let k_pos = encode_sort_key(1, Some(&Value::Int(42)), false, NO_TB);
        let k_big = encode_sort_key(1, Some(&Value::Int(100_000)), false, NO_TB);
        assert!(k_neg < k_zero);
        assert!(k_zero < k_pos);
        assert!(k_pos < k_big);
    }

    #[test]
    fn sort_key_f64_ordering() {
        let k_neg = encode_sort_key(1, Some(&Value::Float(-100.5)), false, NO_TB);
        let k_nsmall = encode_sort_key(1, Some(&Value::Float(-0.001)), false, NO_TB);
        let k_zero = encode_sort_key(1, Some(&Value::Float(0.0)), false, NO_TB);
        let k_pos = encode_sort_key(1, Some(&Value::Float(1.5)), false, NO_TB);
        let k_big = encode_sort_key(1, Some(&Value::Float(99999.99)), false, NO_TB);
        assert!(k_neg < k_nsmall);
        assert!(k_nsmall < k_zero);
        assert!(k_zero < k_pos);
        assert!(k_pos < k_big);
    }

    #[test]
    fn sort_key_text_ordering() {
        let k_a = encode_sort_key(1, Some(&Value::Text("a".into())), false, NO_TB);
        let k_b = encode_sort_key(1, Some(&Value::Text("b".into())), false, NO_TB);
        let k_z = encode_sort_key(1, Some(&Value::Text("z".into())), false, NO_TB);
        assert!(k_a < k_b);
        assert!(k_b < k_z);
    }

    #[test]
    fn sort_key_inverted_reverses() {
        let k1 = encode_sort_key(1, Some(&Value::Int(10)), false, NO_TB);
        let k2 = encode_sort_key(1, Some(&Value::Int(20)), false, NO_TB);
        assert!(k1 < k2, "ASC: 10 < 20");

        let k1_inv = encode_sort_key(1, Some(&Value::Int(10)), true, NO_TB);
        let k2_inv = encode_sort_key(1, Some(&Value::Int(20)), true, NO_TB);
        assert!(k1_inv > k2_inv, "DESC inverted: 10 > 20");
    }

    #[test]
    fn sort_key_ghost_id_prefix() {
        let k_g1 = encode_sort_key(1, Some(&Value::Int(42)), false, NO_TB);
        let k_g2 = encode_sort_key(2, Some(&Value::Int(42)), false, NO_TB);
        assert!(k_g1 < k_g2, "ghost 1 entries sort before ghost 2");
        assert_eq!(decode_ghost_id(&k_g1), Some(1));
        assert_eq!(decode_ghost_id(&k_g2), Some(2));
    }

    #[test]
    fn sort_key_cross_type_ordering() {
        let k_null = encode_sort_key(1, None, false, NO_TB);
        let k_int = encode_sort_key(1, Some(&Value::Int(1)), false, NO_TB);
        let k_float = encode_sort_key(1, Some(&Value::Float(1.0)), false, NO_TB);
        let k_text = encode_sort_key(1, Some(&Value::Text("a".into())), false, NO_TB);
        assert!(k_null < k_int);
        assert!(k_int < k_float);
        assert!(k_float < k_text);
    }

    #[test]
    fn sort_key_f64_negative_edge_cases() {
        let k_neg_inf = encode_sort_key(1, Some(&Value::Float(f64::NEG_INFINITY)), false, NO_TB);
        let k_neg = encode_sort_key(1, Some(&Value::Float(-1.0)), false, NO_TB);
        let k_pos = encode_sort_key(1, Some(&Value::Float(1.0)), false, NO_TB);
        let k_inf = encode_sort_key(1, Some(&Value::Float(f64::INFINITY)), false, NO_TB);
        assert!(k_neg_inf < k_neg);
        assert!(k_neg < k_pos);
        assert!(k_pos < k_inf);
    }

    // ── Tiebreak (audit P0-2) ────────────────────────────────────────────

    /// The core P0-2 ordering fix: a value that is a prefix of another must
    /// sort first regardless of the tiebreak. Here `"a"` carries a tiebreak of
    /// `0xFF` (greater than `'b'`) and `"ab"` a tiebreak of `0x00`; without the
    /// prefix-free terminator, `0xFF` would beat `'b'` and put `"a"` after
    /// `"ab"`. The terminator keeps `"a" < "ab"`.
    #[test]
    fn tiebreak_never_reorders_prefix_text_values() {
        let k_a = encode_sort_key(1, Some(&Value::Text("a".into())), false, &[0xFF]);
        let k_ab = encode_sort_key(1, Some(&Value::Text("ab".into())), false, &[0x00]);
        assert!(
            k_a < k_ab,
            "\"a\" must sort before \"ab\" whatever the tiebreak"
        );
    }

    /// Two records with the same sort value but different spatial keys produce
    /// distinct, deterministically-ordered keys — the uniqueness P0-2 needs so
    /// the second no longer overwrites the first.
    #[test]
    fn tiebreak_disambiguates_equal_values() {
        let v = Value::Int(7);
        let k1 = encode_sort_key(1, Some(&v), false, &[0x00, 0x01]);
        let k2 = encode_sort_key(1, Some(&v), false, &[0x00, 0x02]);
        assert_ne!(
            k1, k2,
            "equal values with different spatial keys must differ"
        );
        assert!(k1 < k2, "ties order by the spatial-key suffix");
    }

    /// Text with an embedded NUL is escaped, stays ordered, and remains
    /// distinct from a string that would otherwise alias it.
    #[test]
    fn tiebreak_text_with_embedded_nul_is_prefix_free() {
        let k_a = encode_sort_key(1, Some(&Value::Text("a".into())), false, &[0x01]);
        let k_a_nul = encode_sort_key(1, Some(&Value::Text("a\u{0}".into())), false, &[0x01]);
        let k_ab = encode_sort_key(1, Some(&Value::Text("ab".into())), false, &[0x01]);
        assert!(k_a < k_a_nul, "\"a\" < \"a\\0\"");
        assert!(k_a_nul < k_ab, "\"a\\0\" < \"ab\"");
        assert_ne!(k_a, k_a_nul);
    }

    /// Under inversion (DESC), the value still drives the primary order while
    /// the tiebreak keeps same-value entries distinct.
    #[test]
    fn tiebreak_under_inversion_keeps_value_primary_and_unique() {
        let v10 = Value::Int(10);
        let v20 = Value::Int(20);
        let k10_a = encode_sort_key(1, Some(&v10), true, &[0x00]);
        let k10_b = encode_sort_key(1, Some(&v10), true, &[0x01]);
        let k20 = encode_sort_key(1, Some(&v20), true, &[0x00]);
        // DESC: 20 before 10 regardless of the 10-group's tiebreaks.
        assert!(k20 < k10_a, "DESC: 20 sorts before 10");
        assert!(k20 < k10_b, "DESC: 20 sorts before either 10 entry");
        assert_ne!(k10_a, k10_b, "same value, different tiebreak → distinct");
    }
}
