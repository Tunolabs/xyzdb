//! v0.2.5.1 — Opaque pagination cursor for plain SCAN.
//!
//! A cursor token is the engine's way of resuming a SCAN where the
//! previous page left off. The wire form is intentionally opaque: clients
//! receive a string, pass it back unchanged on the next request, and never
//! introspect the bytes. This keeps the encoding free to evolve under a
//! `format_ver` byte without breaking client code.
//!
//! ## Wire format
//!
//! Postcard-encoded `CursorPayload` → `base64::URL_SAFE_NO_PAD`. URL-safe
//! base64 because cursors round-trip through HTTP query strings, JSON
//! responses, and CLI arguments without further escaping.
//!
//! ## Scope (v0.2.5.1)
//!
//! - **Plain SCAN only.** Cursor + ORDER BY and cursor + ghost routing
//!   land in v0.3 with a dedicated payload variant. The current `execute_scan`
//!   forces `ScanSource::Primary` whenever a cursor is present.
//! - **Filter checksum binds the cursor to its query.** Re-using a cursor
//!   with a different `WHERE` clause returns an explicit error rather than
//!   silently producing an inconsistent page (see `filter_checksum`).
//! - **Cursor tokens are version-bound.** The filter checksum is derived
//!   from `format!("{:?}", filter_expr)`, which depends on the AST's `Debug`
//!   impl. A future release that adds or renames a `FilterExpr` variant
//!   will invalidate in-flight cursors. This is acceptable for ephemeral
//!   pagination sessions; document any FilterExpr changes in the
//!   corresponding release entry.

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;
use xytalk_parser::ast::FilterExpr;
use xyzdb_core::error::{Result, XyzError};
use xyzdb_core::key::SPATIAL_KEY_SIZE;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Current cursor payload format version. Bump on any change to the
/// on-the-wire shape; tokens with a different `format_ver` are rejected
/// with an explicit error (module docs, Rule 3).
///
/// 0.9.4: bumped v1 (=1) → v2 (=2). The `SpatialKey` widened 22 → 24 bytes
/// (the reserved satellite axis, `key::SPATIAL_KEY_SIZE`), so `last_spatial_key`
/// in a v1 token is 2 bytes short — decoding it against the 24-byte field would
/// misread the tail. The version bump invalidates every in-flight v1 cursor
/// outright rather than mis-deserializing one.
pub const CURSOR_FORMAT_V2: u8 = 2;

/// Decoded cursor payload. Stable serialisation under postcard; new
/// fields require a `format_ver` bump (Rule 3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CursorPayload {
    pub format_ver: u8,
    pub lobe_id: u16,
    /// Raw 24-byte SpatialKey of the last record returned in the previous
    /// page. The next-page seek uses `last_spatial_key ++ [0x00]` as the
    /// open lower bound to skip strictly past it.
    pub last_spatial_key: [u8; SPATIAL_KEY_SIZE],
    /// xxh3-64 of `format!("{:?}", filter_expr)`. See module docs.
    pub filter_checksum: u64,
}

/// Encode a cursor payload to its opaque wire form.
pub fn encode_cursor(payload: &CursorPayload) -> Result<String> {
    let bytes = postcard::to_allocvec(payload)
        .map_err(|e| XyzError::Internal(format!("cursor encode failed: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(&bytes))
}

/// Decode an opaque cursor token. Errors are surfaced as `InvalidQuery`
/// so the client sees a clear `cursor invalid: ...` message.
pub fn decode_cursor(token: &str) -> Result<CursorPayload> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .map_err(|e| XyzError::InvalidQuery(format!("cursor invalid: base64 decode: {e}")))?;
    let payload: CursorPayload = postcard::from_bytes(&bytes)
        .map_err(|e| XyzError::InvalidQuery(format!("cursor invalid: postcard decode: {e}")))?;
    if payload.format_ver != CURSOR_FORMAT_V2 {
        return Err(XyzError::InvalidQuery(format!(
            "cursor invalid: unsupported format version {} (expected {CURSOR_FORMAT_V2}); \
             token was issued by an incompatible engine build",
            payload.format_ver
        )));
    }
    Ok(payload)
}

/// Stable hash of a `FilterExpr` for cursor binding. Uses the AST's
/// `Debug` representation under xxh3-64. See module-level docs for the
/// version-bound caveat.
pub fn filter_checksum(filter_expr: &Option<FilterExpr>) -> u64 {
    let s = format!("{filter_expr:?}");
    xxh3_64(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload() -> CursorPayload {
        CursorPayload {
            format_ver: CURSOR_FORMAT_V2,
            lobe_id: 7,
            last_spatial_key: [
                // lobe_id u16 BE
                0x00, 0x07, // gravity_hash u48 BE (low 48 bits of 0xABCDEF012345)
                0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, // sat u16 BE (reserved, 0)
                0x00, 0x00, // z_order_2d u48 BE
                0x67, 0x89, 0x00, 0x00, 0x00, 0x00, // seq u64 BE
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x42,
            ],
            filter_checksum: 0xDEAD_BEEF_CAFE_BABE,
        }
    }

    #[test]
    fn cursor_roundtrip_present_filter() {
        let p = sample_payload();
        let token = encode_cursor(&p).unwrap();
        // URL-safe base64: no `+`, `/`, or `=` characters.
        assert!(!token.contains('+'));
        assert!(!token.contains('/'));
        assert!(!token.contains('='));
        let decoded = decode_cursor(&token).unwrap();
        assert_eq!(decoded, p);
    }

    #[test]
    fn cursor_decode_rejects_corrupted_base64() {
        let r = decode_cursor("not!valid!base64!");
        let err = r.expect_err("invalid base64 should reject");
        let msg = format!("{err}");
        assert!(msg.contains("cursor invalid"), "got: {msg}");
    }

    #[test]
    fn cursor_decode_rejects_wrong_format_ver() {
        let mut p = sample_payload();
        p.format_ver = 99;
        let token = encode_cursor(&p).unwrap();
        let err = decode_cursor(&token).expect_err("format_ver=99 should reject");
        let msg = format!("{err}");
        assert!(msg.contains("unsupported format version 99"), "got: {msg}");
    }

    #[test]
    fn cursor_decode_rejects_legacy_v1() {
        // 0.9.4: v1 tokens were issued against the 22-byte SpatialKey. The
        // version bump (22→24) invalidates them: decoding must fail cleanly
        // on the format_ver check, never mis-deserialize the shifted tail.
        let mut p = sample_payload();
        p.format_ver = 1; // the legacy v1 version
        let token = encode_cursor(&p).unwrap();
        let err = decode_cursor(&token).expect_err("legacy v1 cursor must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("unsupported format version 1"), "got: {msg}");
    }

    #[test]
    fn filter_checksum_stable_for_same_filter() {
        // Two identical FilterExpr values produce the same checksum.
        let f1: Option<FilterExpr> = None;
        let f2: Option<FilterExpr> = None;
        assert_eq!(filter_checksum(&f1), filter_checksum(&f2));
    }

    #[test]
    fn filter_checksum_differs_for_different_filters() {
        use xytalk_parser::ast::{Filter, FilterOp, Literal};
        let a = FilterExpr::Condition(Filter {
            field: "rfc".into(),
            op: FilterOp::Eq,
            value: Literal::Text("X".into()),
        });
        let b = FilterExpr::Condition(Filter {
            field: "rfc".into(),
            op: FilterOp::Eq,
            value: Literal::Text("Y".into()),
        });
        assert_ne!(filter_checksum(&Some(a)), filter_checksum(&Some(b)));
    }
}
