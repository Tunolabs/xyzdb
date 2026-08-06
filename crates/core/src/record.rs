// SPDX-License-Identifier: BUSL-1.1
use crate::error::XyzError;
use crate::field_dict::FieldDict;
use crate::lid::LID;
use crate::value::Value;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Magic prefix for xyzDB on-disk record format. "XY" = [0x58, 0x59].
/// Legacy bincode records start with LID u128 little-endian where byte[0] = 0x00
/// (reserved field), so collision is impossible.
pub const XYZDB_MAGIC: [u8; 2] = [0x58, 0x59];

/// On-disk format V1 (Fase 1). No lobe_name, field names as strings.
#[derive(Serialize, Deserialize)]
struct RecordOnDiskV1 {
    lid: LID,
    fields: BTreeMap<String, Value>,
    created_at: i64,
    updated_at: i64,
}

/// On-disk format V2 (Fase 2). No lobe_name, field IDs (u16) instead of strings.
#[derive(Serialize, Deserialize)]
struct RecordOnDiskV2 {
    lid: LID,
    fields: BTreeMap<u16, Value>,
    created_at: i64,
    updated_at: i64,
}

/// On-disk format V5 — the record blob carries NO searchable vector at all,
/// neither a hoisted prefix nor a copy inside `fields`. The vector lives only in
/// its own column value, written separately by [`serialize_record_v5`]. This
/// splits the cold text/scalar payload from the hot vector so a NEAREST scan
/// touches just the column and never drags the blob through cache.
///
/// The blob holds `lid, fields, created_at, updated_at` with no vector prefix;
/// `fields` excludes the designated search field (the vector is never stored in
/// the blob). [`deserialize_record`] decodes the blob without the vector; the
/// caller restores it with [`hydrate_vector`] from the column.
#[derive(Serialize, Deserialize)]
struct RecordOnDiskV5 {
    lid: LID,
    fields: BTreeMap<u16, Value>,
    created_at: i64,
    updated_at: i64,
}

/// Serialize a Record to V1 format: [MAGIC:2][0x01][postcard payload].
/// Used by ghost creation (ghosts stay V1 — no field IDs).
pub fn serialize_record(record: &Record) -> Vec<u8> {
    let on_disk = RecordOnDiskV1 {
        lid: record.lid,
        fields: record.fields.clone(),
        created_at: record.created_at,
        updated_at: record.updated_at,
    };
    let payload = postcard::to_allocvec(&on_disk).expect("postcard serialize");
    let mut buf = Vec::with_capacity(3 + payload.len());
    buf.extend_from_slice(&XYZDB_MAGIC);
    buf.push(0x01);
    buf.extend_from_slice(&payload);
    buf
}

/// Serialize a Record to V2 format: [MAGIC:2][0x02][postcard payload with field IDs].
/// Used by PUT/SET for spatial keyspace. May create new field IDs in the dict.
pub fn serialize_record_v2(record: &Record, field_dict: &mut FieldDict) -> Vec<u8> {
    let id_fields: BTreeMap<u16, Value> = record
        .fields
        .iter()
        .map(|(name, val)| {
            let (id, _new) = field_dict.get_or_create_id(name);
            (id, val.clone())
        })
        .collect();
    let on_disk = RecordOnDiskV2 {
        lid: record.lid,
        fields: id_fields,
        created_at: record.created_at,
        updated_at: record.updated_at,
    };
    let payload = postcard::to_allocvec(&on_disk).expect("postcard serialize v2");
    let mut buf = Vec::with_capacity(3 + payload.len());
    buf.extend_from_slice(&XYZDB_MAGIC);
    buf.push(0x02);
    buf.extend_from_slice(&payload);
    buf
}

/// Serialize a Record to V5 — a split layout returning `(blob, column_value)`.
///
/// V5: the searchable vector lives in the column, not the blob. The `blob` is
/// `[MAGIC:2][0x05][postcard(RecordOnDiskV5)]` where `fields` EXCLUDES
/// `search_field` and carries no vector prefix. The vector is emitted separately
/// as `column_value`.
///
/// `column_value` is `Some(..)` only when `search_field` names a present
/// `Value::Vector`; otherwise it is `None` (absent field or non-vector value,
/// or `search_field == None`). When present it is the **vector column**: a
/// mini-blob carrying ONLY `lid` + the vector prefix — no `fields` tail — read
/// back by [`read_vector_prefix_raw_norm`]. Its byte layout is:
/// `[MAGIC:2][0x04] ++ leb128(lid) ++ [tag=1] ++ leb128(field_id) ++
///  leb128(len) ++ (f32 LE bytes) ++ (norm_sq f64 LE 8 bytes)`. The `0x04`
/// leading byte marks a column that carries the stored norm; it is the kernel
/// contract of the column reader, not a record version.
///
/// `norm_sq` is `‖v‖²` via the canonical f32x8 reduction
/// ([`crate::distance::norm_sq`], no `sqrt`) — bit-identical to what the live
/// cosine path computes, so a column scores identically to computing the norm
/// live. May create a new field ID in the dict for `search_field`.
pub fn serialize_record_v5(
    record: &Record,
    field_dict: &mut FieldDict,
    search_field: Option<&str>,
) -> (Vec<u8>, Option<Vec<u8>>) {
    // Resolve the search field once; it is excluded from the blob and, when a
    // vector, emitted as the column value below.
    let search_vec: Option<(&str, u16, &Vec<f32>)> = match search_field {
        Some(name) => match record.fields.get(name) {
            Some(Value::Vector(v)) => {
                let (id, _new) = field_dict.get_or_create_id(name);
                Some((name, id, v))
            }
            _ => None, // search field absent or not a vector → no column value
        },
        None => None,
    };
    // Exclude from the blob ONLY the field that becomes the column (the search
    // vector). A present-but-non-vector search field STAYS in the blob — mirrors
    // V4, so declaring a non-vector field as search never silently drops it.
    let excluded_field: Option<&str> = search_vec.as_ref().map(|&(name, _, _)| name);
    let mut id_fields: BTreeMap<u16, Value> = BTreeMap::new();
    for (name, val) in &record.fields {
        if Some(name.as_str()) == excluded_field {
            continue; // lives in the column (vector) — never in the blob
        }
        let (id, _new) = field_dict.get_or_create_id(name);
        id_fields.insert(id, val.clone());
    }
    let on_disk = RecordOnDiskV5 {
        lid: record.lid,
        fields: id_fields,
        created_at: record.created_at,
        updated_at: record.updated_at,
    };
    let payload = postcard::to_allocvec(&on_disk).expect("postcard serialize v5");
    let mut blob = Vec::with_capacity(3 + payload.len());
    blob.extend_from_slice(&XYZDB_MAGIC);
    blob.push(0x05);
    blob.extend_from_slice(&payload);

    // Build the vector column value: MAGIC + 0x04 + lid + Some-tagged prefix.
    // Hand-rolled (not postcard) so it is byte-identical to the layout the
    // `read_vector_prefix_raw_norm` reader expects.
    let column_value = search_vec.map(|(_name, field_id, v)| {
        // Canonical f32x8 reduction — the same ‖v‖² the live cosine path computes.
        let norm_sq: f64 = crate::distance::norm_sq(v);
        let mut col = Vec::with_capacity(3 + 5 + 5 + v.len() * 4 + 8);
        col.extend_from_slice(&XYZDB_MAGIC);
        col.push(0x04); // column marker: carries the stored norm
        write_leb128(&mut col, record.lid.raw());
        col.push(0x01); // Option tag: Some(vector)
        write_leb128(&mut col, field_id as u128);
        write_leb128(&mut col, v.len() as u128);
        for x in v {
            col.extend_from_slice(&x.to_le_bytes());
        }
        col.extend_from_slice(&norm_sq.to_le_bytes());
        col
    });

    (blob, column_value)
}

/// Read one unsigned LEB128 varint (postcard's encoding for unsigned ints):
/// 7 payload bits per byte, low 7 bits little-endian, high bit = continuation.
/// Returns `(value, rest)` where `rest` is the slice after the varint, or `None`
/// on truncation (ran off the end) or overflow (more than 19 bytes for u128).
fn read_leb128(bytes: &[u8]) -> Option<(u128, &[u8])> {
    let mut value: u128 = 0;
    let mut shift: u32 = 0;
    let mut i = 0;
    loop {
        let byte = *bytes.get(i)?;
        // u128 holds at most ceil(128/7) = 19 groups; reject a longer run.
        if shift >= 128 {
            return None;
        }
        value |= ((byte & 0x7F) as u128) << shift;
        i += 1;
        if byte & 0x80 == 0 {
            return Some((value, &bytes[i..]));
        }
        shift += 7;
    }
}

/// Append one unsigned LEB128 varint — the inverse of [`read_leb128`], matching
/// postcard's encoding for unsigned ints (7 payload bits per byte, low 7 bits
/// little-endian, high bit = continuation). Used to hand-roll the V4-shaped V5
/// column value so it is byte-identical to a postcard-encoded V4 prefix.
fn write_leb128(buf: &mut Vec<u8>, mut value: u128) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

/// The zero-copy vector-column reader, also returning the stored squared norm
/// when present. The column's leading marker byte selects the layout: `0x04`
/// carries the norm — after the `len * 4` f32 bytes come 8 little-endian bytes
/// holding `‖v‖²` (fixed `f64`, not varint); `0x03` (a norm-less column) has no
/// such field and the norm is `None` (the caller computes it live). Both markers
/// are the column's on-wire contract, not record versions.
///
/// # Returns
/// `(lid, field_id, f32_bytes, norm_sq)` where `norm_sq` is `Some` only for a
/// `0x04` column; `None` for `vec: None`, an unrecognized marker, or a
/// truncated/overflowing prefix.
pub fn read_vector_prefix_raw_norm(bytes: &[u8]) -> Option<(LID, u16, &[u8], Option<f64>)> {
    if bytes.len() < 3 || bytes[0..2] != XYZDB_MAGIC {
        return None;
    }
    let has_norm = match bytes[2] {
        0x03 => false,
        0x04 => true,
        _ => return None,
    };
    let rest = &bytes[3..];
    let (lid_raw, rest) = read_leb128(rest)?;
    let (&tag, rest) = rest.split_first()?;
    match tag {
        0 => return None,
        1 => {}
        _ => return None,
    }
    let (field_id, rest) = read_leb128(rest)?;
    let field_id = u16::try_from(field_id).ok()?;
    let (len, rest) = read_leb128(rest)?;
    let nbytes = usize::try_from(len).ok()?.checked_mul(4)?;
    let fbytes = rest.get(..nbytes)?;
    let norm_sq = if has_norm {
        let nb = rest.get(nbytes..nbytes + 8)?;
        Some(f64::from_le_bytes([
            nb[0], nb[1], nb[2], nb[3], nb[4], nb[5], nb[6], nb[7],
        ]))
    } else {
        None
    };
    Some((LID::from_raw(lid_raw), field_id, fbytes, norm_sq))
}

/// Restore a V5 record's search vector from its column value — the counterpart
/// to the split [`serialize_record_v5`] produces. The V5 blob decodes without
/// the vector (it never lived in the blob); this re-inserts it so the in-memory
/// Record is indistinguishable from a V2/V3/V4 one.
///
/// `column_value` is the V4-shaped mini-blob, parsed with the existing
/// [`read_vector_prefix_raw_norm`]; its `field_id` is resolved to a name via
/// `field_dict` and the little-endian f32 bytes are decoded into a
/// `Value::Vector` inserted under that name.
///
/// # Returns
/// `true` when the vector was decoded and inserted; `false` when the bytes do
/// not parse as a vector prefix or the `field_id` is not in the dict (the record
/// is left unchanged in that case).
pub fn hydrate_vector(record: &mut Record, column_value: &[u8], field_dict: &FieldDict) -> bool {
    let Some((_lid, field_id, fbytes, _norm_sq)) = read_vector_prefix_raw_norm(column_value) else {
        return false;
    };
    let Some(name) = field_dict.get_name(field_id) else {
        return false;
    };
    let floats: Vec<f32> = fbytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    record
        .fields
        .insert(name.to_string(), Value::Vector(floats));
    true
}

/// Deserialize a Record from disk, detecting format automatically.
///
/// - `[0x58, 0x59, 0x05, ...]` -- V5 postcard: no vector in the blob; caller hydrates from the column (needs field_dict)
/// - `[0x58, 0x59, 0x02, ...]` -- V2 postcard with field IDs (needs field_dict)
/// - `[0x58, 0x59, 0x01, ...]` -- V1 postcard with string field names
/// - anything else -- legacy bincode (with lobe_name)
///
/// `lobe_name` is injected for V1+ formats. `field_dict` is required for V2.
pub fn deserialize_record(
    bytes: &[u8],
    lobe_name: &str,
    field_dict: Option<&FieldDict>,
) -> crate::error::Result<Record> {
    if bytes.len() >= 3 && bytes[0..2] == XYZDB_MAGIC {
        match bytes[2] {
            0x02 => {
                if let Ok(on_disk) = postcard::from_bytes::<RecordOnDiskV2>(&bytes[3..])
                    && let Some(dict) = field_dict
                {
                    let string_fields: BTreeMap<String, Value> = on_disk
                        .fields
                        .into_iter()
                        .filter_map(|(id, val)| {
                            dict.get_name(id).map(|name| (name.to_string(), val))
                        })
                        .collect();
                    return Ok(Record {
                        lid: on_disk.lid,
                        lobe_name: lobe_name.to_string(),
                        fields: string_fields,
                        created_at: on_disk.created_at,
                        updated_at: on_disk.updated_at,
                    });
                }
                // V2 without field_dict — fall through to try V1/bincode
            }
            0x05 => {
                if let Ok(on_disk) = postcard::from_bytes::<RecordOnDiskV5>(&bytes[3..])
                    && let Some(dict) = field_dict
                {
                    let string_fields: BTreeMap<String, Value> = on_disk
                        .fields
                        .into_iter()
                        .filter_map(|(id, val)| {
                            dict.get_name(id).map(|name| (name.to_string(), val))
                        })
                        .collect();
                    // V5: the search vector is NOT in the blob — it lives in the
                    // column. The caller restores it via `hydrate_vector`, so the
                    // record decoded here is missing exactly that one field.
                    return Ok(Record {
                        lid: on_disk.lid,
                        lobe_name: lobe_name.to_string(),
                        fields: string_fields,
                        created_at: on_disk.created_at,
                        updated_at: on_disk.updated_at,
                    });
                }
                // V5 without field_dict — fall through to try V1/bincode
            }
            0x01 => {
                if let Ok(on_disk) = postcard::from_bytes::<RecordOnDiskV1>(&bytes[3..]) {
                    return Ok(Record {
                        lid: on_disk.lid,
                        lobe_name: lobe_name.to_string(),
                        fields: on_disk.fields,
                        created_at: on_disk.created_at,
                        updated_at: on_disk.updated_at,
                    });
                }
            }
            _ => {}
        }
    }
    // Legacy: bincode with lobe_name embedded
    let record: Record = bincode::deserialize(bytes)
        .map_err(|e| XyzError::Storage(format!("Record deserialization failed: {e}")))?;
    Ok(record)
}

/// Returns true if bytes are in xyzDB V1+ format (magic prefix present).
pub fn is_new_format(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0..2] == XYZDB_MAGIC
}

/// Returns the on-disk record format version: 0 = legacy bincode, 1 = postcard
/// V1, 2 = postcard V2, 5 = postcard V5 (vector split into its own column, not
/// in the blob). Records are written as V1/V2/V5; the retired V3/V4 record
/// formats are no longer emitted. (`0x04` survives only as the vector-column
/// marker read by [`read_vector_prefix_raw_norm`], never as a record blob.)
pub fn format_version(bytes: &[u8]) -> u8 {
    if bytes.len() >= 3 && bytes[0..2] == XYZDB_MAGIC {
        bytes[2]
    } else {
        0
    }
}

/// A record in xyzDB. In-memory representation (always has lobe_name).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub lid: LID,
    pub lobe_name: String,
    pub fields: BTreeMap<String, Value>,
    pub created_at: i64, // microseconds since epoch
    pub updated_at: i64, // microseconds since epoch
}

impl Record {
    /// Serialize to bytes (V1 format). Convenience wrapper.
    pub fn to_bytes(&self) -> crate::error::Result<Vec<u8>> {
        Ok(serialize_record(self))
    }

    /// Deserialize from bytes (auto-detect format, no field_dict for V2).
    /// For V2 records, prefer `deserialize_record()` with field_dict.
    pub fn from_bytes(data: &[u8]) -> crate::error::Result<Self> {
        deserialize_record(data, "", None)
    }

    /// Rough estimate of heap memory used by this record (for cache budget).
    pub fn estimated_size(&self) -> usize {
        // Base struct + lobe_name + fields overhead
        std::mem::size_of::<Self>()
            + self.lobe_name.len()
            + self.fields.len() * 64 // BTreeMap node + key string avg
            + self.fields.values().map(|v| v.estimated_size()).sum::<usize>()
    }

    /// Check if a record matches all given filters.
    /// Supports dot notation for nested field access (e.g. "scoring.bureau").
    pub fn matches_filters(&self, filters: &[(String, FilterOp, Value)]) -> bool {
        filters.iter().all(|(field, op, val)| match op {
            FilterOp::IsNull => {
                matches!(resolve_path(&self.fields, field), None | Some(Value::Null))
            }
            FilterOp::IsNotNull => {
                matches!(resolve_path(&self.fields, field), Some(v) if !matches!(v, Value::Null))
            }
            FilterOp::Contains => match resolve_path(&self.fields, field) {
                Some(Value::List(list)) => list.iter().any(|elem| elem == val),
                _ => false,
            },
            // Mirror of Contains: the VALUE is the list, the field is a scalar
            // that must be one of its elements. `x IN (a, b, c)`.
            FilterOp::In => match (resolve_path(&self.fields, field), val) {
                (Some(field_val), Value::List(list)) => list.iter().any(|elem| elem == field_val),
                _ => false,
            },
            _ => match resolve_path(&self.fields, field) {
                None => false,
                Some(field_val) => match op {
                    FilterOp::Eq => field_val == val,
                    FilterOp::Neq => field_val != val,
                    FilterOp::Gt => {
                        field_val.partial_cmp_value(val) == Some(std::cmp::Ordering::Greater)
                    }
                    FilterOp::Gte => matches!(
                        field_val.partial_cmp_value(val),
                        Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
                    ),
                    FilterOp::Lt => {
                        field_val.partial_cmp_value(val) == Some(std::cmp::Ordering::Less)
                    }
                    FilterOp::Lte => matches!(
                        field_val.partial_cmp_value(val),
                        Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
                    ),
                    FilterOp::IsNull | FilterOp::IsNotNull | FilterOp::Contains | FilterOp::In => {
                        unreachable!()
                    }
                },
            },
        })
    }
}

/// Resolve a dot-separated path into a nested Value.
/// "scoring.bureau" navigates Map values; "items.0" indexes into List.
/// Returns None if any segment is missing or the type doesn't support nesting.
pub fn resolve_path<'a>(fields: &'a BTreeMap<String, Value>, path: &str) -> Option<&'a Value> {
    // Fast path: no dot = direct field lookup (most common case)
    if !path.contains('.') {
        return fields.get(path);
    }
    let mut parts = path.splitn(2, '.');
    let first = parts.next()?;
    let mut current = fields.get(first)?;
    if let Some(rest) = parts.next() {
        for part in rest.split('.') {
            match current {
                Value::Map(map) => {
                    current = map.get(part)?;
                }
                Value::List(list) => {
                    let idx: usize = part.parse().ok()?;
                    current = list.get(idx)?;
                }
                _ => return None,
            }
        }
    }
    Some(current)
}

/// Filter operators for WHERE clauses. Mirrored from parser AST for engine use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
    IsNull,    // V4
    IsNotNull, // V4
    Contains,  // V4: List CONTAINS element
    In,        // scalar field ∈ a list of candidate values
}

impl std::fmt::Display for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "LID: {}", self.lid)?;
        writeln!(f, "Lobe: {}", self.lobe_name)?;
        for (k, v) in &self.fields {
            writeln!(f, "{k}: {v}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_v1_roundtrip() {
        let mut fields = BTreeMap::new();
        fields.insert("name".into(), Value::Text("Ivan".into()));
        fields.insert("age".into(), Value::Int(38));

        let rec = Record {
            lid: LID::new(1),
            lobe_name: "workspace".into(),
            fields,
            created_at: 1000,
            updated_at: 1000,
        };

        let bytes = serialize_record(&rec);
        // Verify magic prefix
        assert_eq!(&bytes[0..2], &XYZDB_MAGIC);
        assert_eq!(bytes[2], 0x01);

        let restored = deserialize_record(&bytes, "workspace", None).unwrap();
        assert_eq!(restored.lobe_name, "workspace");
        assert_eq!(
            restored.fields.get("name"),
            Some(&Value::Text("Ivan".into()))
        );
        assert_eq!(restored.fields.get("age"), Some(&Value::Int(38)));
        assert_eq!(restored.created_at, 1000);
    }

    #[test]
    fn record_legacy_bincode_compat() {
        let mut fields = BTreeMap::new();
        fields.insert("status".into(), Value::Text("active".into()));

        let rec = Record {
            lid: LID::new(1),
            lobe_name: "clientes".into(),
            fields,
            created_at: 500,
            updated_at: 600,
        };

        // Serialize with legacy bincode (as V4 would have)
        let legacy_bytes = bincode::serialize(&rec).unwrap();
        // First byte should be 0x00 (LID reserved), not 0x58
        assert_ne!(legacy_bytes[0], XYZDB_MAGIC[0]);

        // deserialize_record should read it via bincode fallback
        let restored = deserialize_record(&legacy_bytes, "clientes", None).unwrap();
        assert_eq!(restored.lobe_name, "clientes");
        assert_eq!(
            restored.fields.get("status"),
            Some(&Value::Text("active".into()))
        );
    }

    #[test]
    fn record_v1_lobe_name_injected() {
        let mut fields = BTreeMap::new();
        fields.insert("x".into(), Value::Int(1));

        let rec = Record {
            lid: LID::new(5),
            lobe_name: "original".into(),
            fields,
            created_at: 0,
            updated_at: 0,
        };

        let bytes = serialize_record(&rec);
        // Deserialize with a different lobe_name — it should use the injected one
        let restored = deserialize_record(&bytes, "injected", None).unwrap();
        assert_eq!(restored.lobe_name, "injected");
    }

    #[test]
    fn is_new_format_detection() {
        let rec = Record {
            lid: LID::new(1),
            lobe_name: "t".into(),
            fields: BTreeMap::new(),
            created_at: 0,
            updated_at: 0,
        };
        let v1_bytes = serialize_record(&rec);
        assert!(is_new_format(&v1_bytes));

        let legacy_bytes = bincode::serialize(&rec).unwrap();
        assert!(!is_new_format(&legacy_bytes));
    }

    #[test]
    fn record_v1_all_value_types() {
        let mut inner_map = BTreeMap::new();
        inner_map.insert("nested".into(), Value::Bool(true));

        let mut fields = BTreeMap::new();
        fields.insert("b".into(), Value::Bool(false));
        fields.insert("i".into(), Value::Int(-42));
        fields.insert("f".into(), Value::Float(1.23));
        fields.insert("t".into(), Value::Text("hello".into()));
        fields.insert("ts".into(), Value::Timestamp(1_000_000));
        fields.insert("by".into(), Value::Bytes(vec![0xDE, 0xAD]));
        fields.insert("l".into(), Value::List(vec![Value::Int(1), Value::Null]));
        fields.insert("m".into(), Value::Map(inner_map));
        fields.insert("n".into(), Value::Null);

        let rec = Record {
            lid: LID::new(1),
            lobe_name: "test".into(),
            fields,
            created_at: 100,
            updated_at: 200,
        };

        let bytes = serialize_record(&rec);
        let restored = deserialize_record(&bytes, "test", None).unwrap();
        assert_eq!(rec.fields, restored.fields);
    }

    #[test]
    fn record_vector_roundtrips_and_is_compact() {
        let dims = 768;
        let packed: Vec<f32> = (0..dims).map(|i| i as f32 * 0.001).collect();

        let mut fields = BTreeMap::new();
        fields.insert("emb".into(), Value::Vector(packed.clone()));
        let rec = Record {
            lid: LID::new(1),
            lobe_name: "m".into(),
            fields,
            created_at: 0,
            updated_at: 0,
        };
        let bytes = serialize_record(&rec);
        let restored = deserialize_record(&bytes, "m", None).unwrap();
        assert_eq!(
            restored.fields.get("emb"),
            Some(&Value::Vector(packed.clone()))
        );

        // The equivalent f64 List (1 tag + 8 bytes per element) must be materially larger:
        // the packed f32 vector should be well under 60% of the List form.
        let list: Vec<Value> = packed.iter().map(|x| Value::Float(*x as f64)).collect();
        let mut f2 = BTreeMap::new();
        f2.insert("emb".into(), Value::List(list));
        let rec_list = Record {
            lid: LID::new(1),
            lobe_name: "m".into(),
            fields: f2,
            created_at: 0,
            updated_at: 0,
        };
        let list_bytes = serialize_record(&rec_list);
        assert!(
            bytes.len() * 100 < list_bytes.len() * 60,
            "packed vector ({}) should be < 60% of f64 list ({})",
            bytes.len(),
            list_bytes.len()
        );
    }

    #[test]
    fn sanity_check_size_postcard_vs_bincode() {
        // Realistic record similar to benchmark entities
        let mut fields = BTreeMap::new();
        fields.insert("_type".into(), Value::Text("Installment".into()));
        fields.insert("status".into(), Value::Text("overdue".into()));
        fields.insert("monto".into(), Value::Float(15432.50));
        fields.insert("dias_atraso".into(), Value::Int(45));
        fields.insert(
            "fecha_vencimiento".into(),
            Value::Timestamp(1_700_000_000_000_000),
        );
        fields.insert("numero_parcialidad".into(), Value::Int(12));
        fields.insert("capital".into(), Value::Float(12000.0));
        fields.insert("interes".into(), Value::Float(3432.50));
        fields.insert("saldo_pendiente".into(), Value::Float(95000.0));
        fields.insert("referencia".into(), Value::Text("CRED-2024-000042".into()));

        let rec = Record {
            lid: LID::new(3),
            lobe_name: "creditos".into(),
            fields,
            created_at: 1_700_000_000_000_000,
            updated_at: 1_700_000_000_000_000,
        };

        let v1_bytes = serialize_record(&rec);
        let bincode_bytes = bincode::serialize(&rec).unwrap();

        // postcard + no lobe_name should be smaller
        // Measured: 391 bytes (bincode) → 234 bytes (postcard V1) = 40.2% reduction
        assert!(
            v1_bytes.len() < bincode_bytes.len(),
            "V1 ({}) should be smaller than V0 ({})",
            v1_bytes.len(),
            bincode_bytes.len()
        );

        // V2 with field IDs should be even smaller
        let mut fd = crate::field_dict::FieldDict::new();
        let v2_bytes = serialize_record_v2(&rec, &mut fd);
        // Measured: V0=391 → V1=234 → V2=133 bytes (66% total reduction)
        // V2 replaces field name strings with u16 IDs
        assert!(
            v2_bytes.len() < v1_bytes.len(),
            "V2 ({}) should be smaller than V1 ({})",
            v2_bytes.len(),
            v1_bytes.len()
        );

        // Verify V2 roundtrip
        let restored = deserialize_record(&v2_bytes, "creditos", Some(&fd)).unwrap();
        assert_eq!(restored.fields.len(), rec.fields.len());
        assert_eq!(
            restored.fields.get("status"),
            Some(&Value::Text("overdue".into()))
        );
    }

    #[test]
    fn filter_matching() {
        let mut fields = BTreeMap::new();
        fields.insert("budget".into(), Value::Int(50000));
        fields.insert("status".into(), Value::Text("active".into()));

        let rec = Record {
            lid: LID::new(1),
            lobe_name: "workspace".into(),
            fields,
            created_at: 0,
            updated_at: 0,
        };

        let filters = vec![
            ("budget".into(), FilterOp::Gt, Value::Int(10000)),
            ("status".into(), FilterOp::Eq, Value::Text("active".into())),
        ];
        assert!(rec.matches_filters(&filters));

        let bad = vec![("budget".into(), FilterOp::Lt, Value::Int(10000))];
        assert!(!rec.matches_filters(&bad));
    }

    // ── vector-record test helper (shared by V4/V5) ──────────────────────────────────────────────────

    fn vec_record() -> Record {
        let mut fields = BTreeMap::new();
        fields.insert(
            "emb".into(),
            Value::Vector((0..64).map(|i| i as f32 * 0.1).collect()),
        );
        fields.insert(
            "txt".into(),
            Value::Text("a representative memory chunk".into()),
        );
        fields.insert("topic".into(), Value::Text("a".into()));
        Record {
            lid: LID::new(7),
            lobe_name: "mem".into(),
            fields,
            created_at: 11,
            updated_at: 22,
        }
    }

    // ── V5 (vector split into its own column) ────────────────────────────────

    #[test]
    fn v5_roundtrip_blob_then_hydrate() {
        let rec = vec_record();
        let mut fd = crate::field_dict::FieldDict::new();
        let (blob, column) = serialize_record_v5(&rec, &mut fd, Some("emb"));
        assert_eq!(&blob[0..2], &XYZDB_MAGIC);
        assert_eq!(blob[2], 0x05, "V5 version byte");

        // The blob decodes WITHOUT the search vector: every field but "emb".
        let mut restored = deserialize_record(&blob, "mem", Some(&fd)).unwrap();
        assert_eq!(restored.lid, rec.lid);
        assert_eq!(restored.created_at, 11);
        assert_eq!(restored.updated_at, 22);
        assert!(
            !restored.fields.contains_key("emb"),
            "V5 blob must NOT carry the search vector"
        );
        for name in ["txt", "topic"] {
            assert_eq!(
                restored.fields.get(name),
                rec.fields.get(name),
                "non-search field {name} survives the blob"
            );
        }

        // Hydrating from the column restores the vector → full record equality.
        let ok = hydrate_vector(&mut restored, &column.expect("vector → column"), &fd);
        assert!(ok, "hydrate_vector must succeed on a well-formed column");
        assert_eq!(
            restored.fields.get("emb"),
            rec.fields.get("emb"),
            "hydrated vector must equal the original"
        );
        assert_eq!(
            restored.fields, rec.fields,
            "V5 blob + hydrate must reconstruct the full record"
        );
    }

    #[test]
    fn v5_column_parses_with_existing_prefix_reader() {
        let rec = vec_record();
        let mut fd = crate::field_dict::FieldDict::new();
        let (_blob, column) = serialize_record_v5(&rec, &mut fd, Some("emb"));
        let column = column.expect("vector → column");

        // The column is V4-shaped: the EXISTING reader parses it byte-for-byte.
        let (lid, field_id, fbytes, norm_sq) =
            read_vector_prefix_raw_norm(&column).expect("V5 column parses as a V4 prefix");
        assert_eq!(lid, rec.lid, "column lid equals the record lid");
        assert_eq!(fd.get_name(field_id), Some("emb"), "column field_id is emb");
        assert!(norm_sq.is_some(), "V5 column carries the stored norm");
        let decoded: Vec<f32> = fbytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        match rec.fields.get("emb") {
            Some(Value::Vector(orig)) => {
                assert_eq!(&decoded, orig, "column f32 bytes equal the input vector")
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn v5_column_norm_sq_is_canonical_bit_exact() {
        // The load-bearing equivalence for lever C: the vector column's stored
        // ‖v‖² must be the RAW sum of squares (no sqrt) via the canonical f32x8
        // reduction (`distance::norm_sq` = `dot_acc(v,v)`), bit-for-bit equal to
        // what the live cosine path computes — else the column score would diverge.
        let rec = vec_record();
        let mut fd = crate::field_dict::FieldDict::new();
        let (_blob, column) = serialize_record_v5(&rec, &mut fd, Some("emb"));
        let column = column.expect("vector → column");
        let (_, _, fbytes, col_norm) = read_vector_prefix_raw_norm(&column).expect("column");
        let floats: Vec<f32> = fbytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let live: f64 = crate::distance::norm_sq(&floats);
        assert_eq!(
            col_norm.expect("column carries the norm").to_bits(),
            live.to_bits(),
            "vector column norm_sq must equal the canonical norm_sq bit-for-bit"
        );
    }

    #[test]
    fn v5_no_search_field_gives_no_column() {
        let rec = vec_record();

        // search_field = None → no column, blob carries the full record.
        let mut fd = crate::field_dict::FieldDict::new();
        let (blob, column) = serialize_record_v5(&rec, &mut fd, None);
        assert_eq!(blob[2], 0x05);
        assert!(column.is_none(), "no search field → no column value");
        let restored = deserialize_record(&blob, "mem", Some(&fd)).unwrap();
        assert_eq!(
            restored.fields, rec.fields,
            "with no search field the V5 blob holds every field"
        );

        // search_field names a non-vector field → still no column value.
        let mut fd2 = crate::field_dict::FieldDict::new();
        let (blob2, column2) = serialize_record_v5(&rec, &mut fd2, Some("txt"));
        assert!(
            column2.is_none(),
            "non-vector search field → no column value"
        );
        // A non-vector search field is KEPT in the blob (V5 only moves the field
        // out when it is the vector going to the column) — no silent drop, mirrors V4.
        let restored2 = deserialize_record(&blob2, "mem", Some(&fd2)).unwrap();
        assert_eq!(
            restored2.fields, rec.fields,
            "non-vector search field stays in the blob (mirrors V4, no data loss)"
        );
    }
}
