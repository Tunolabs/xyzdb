use serde::de::{Deserializer, Error as DeError};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt;

/// Fundamental data type of xyzDB. Every stored datum is a Value.
///
/// `Deserialize` is hand-written (see below) to cap nesting depth; `Serialize`
/// stays derived so the on-disk/wire encoding is unchanged.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Timestamp(i64), // Microseconds since Unix epoch
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Null, // V4: ordinal 8 — keep at this ordinal for bincode/postcard backward compat
    /// Dense `f32` embedding (V5). Appended AFTER `Null` so every existing variant
    /// ordinal is unchanged: old postcard/bincode records (ordinals 0..=8) still
    /// deserialize untouched, and only new binaries emit ordinal 9. Packs ~2x denser
    /// than `List(Float(f64))` (4 vs 9 bytes/element under postcard). A homogeneous
    /// float list of `>= VECTOR_F32_MIN_DIMS` elements is stored here automatically
    /// (see `ops::literal_to_value`); shorter float lists stay `List` to preserve f64.
    Vector(Vec<f32>),
}

/// Maximum nesting depth accepted when DECODING a `Value`. Deserialization
/// recurses through `List`/`Map`, and the binary formats (postcard, bincode)
/// impose no depth limit of their own, so an untrusted payload (≤16 MiB frame)
/// can nest deep enough to overflow the worker thread's stack — an uncatchable
/// abort that kills the process. The language surface is already capped far
/// below this (`MAX_LITERAL_DEPTH = 16`) and a real record lives at depth 3–4,
/// so 32 rejects only adversarial input. The cap ONLY rejects over-deep input;
/// within it, decoding is byte-for-byte the derived behaviour (guarded by the
/// `value_golden` fixtures).
const MAX_DECODE_DEPTH: usize = 32;

thread_local! {
    static DECODE_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Restores the decode-depth counter on scope exit — including error unwinds —
/// so a rejected or failed decode never leaves the thread-local inflated.
struct DepthGuard;
impl Drop for DepthGuard {
    fn drop(&mut self) {
        DECODE_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Derived twin of `Value` — identical variant order and field types, so its
/// generated `Deserialize` decodes byte-for-byte like the original derive on
/// every format (postcard/bincode key off the ordinal; self-describing formats
/// key off the variant names, which also match). The recursive variants hold
/// `Value`, so nested decoding routes back through the depth-guarded impl below.
/// This is the whole point: the field decoding stays derive-generated (no manual
/// format logic to get subtly wrong), and only a depth counter is added.
#[derive(Deserialize)]
enum ValueRepr {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Timestamp(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Null,
    Vector(Vec<f32>),
}

impl From<ValueRepr> for Value {
    fn from(r: ValueRepr) -> Self {
        match r {
            ValueRepr::Bool(v) => Value::Bool(v),
            ValueRepr::Int(v) => Value::Int(v),
            ValueRepr::Float(v) => Value::Float(v),
            ValueRepr::Text(v) => Value::Text(v),
            ValueRepr::Timestamp(v) => Value::Timestamp(v),
            ValueRepr::Bytes(v) => Value::Bytes(v),
            ValueRepr::List(v) => Value::List(v),
            ValueRepr::Map(v) => Value::Map(v),
            ValueRepr::Null => Value::Null,
            ValueRepr::Vector(v) => Value::Vector(v),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let depth = DECODE_DEPTH.with(|d| {
            let n = d.get() + 1;
            d.set(n);
            n
        });
        // Decrement on every exit path (Ok, the depth Err below, or an inner
        // decode error) before returning.
        let _guard = DepthGuard;
        if depth > MAX_DECODE_DEPTH {
            return Err(DeError::custom(format!(
                "Value nesting exceeds the maximum decode depth of {MAX_DECODE_DEPTH}"
            )));
        }
        ValueRepr::deserialize(deserializer).map(Value::from)
    }
}

impl Value {
    /// Returns the type name as a static string.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Text(_) => "text",
            Value::Timestamp(_) => "timestamp",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Map(_) => "map",
            Value::Null => "null",
            Value::Vector(_) => "vector",
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(v) => Some(v.as_str()),
            _ => None,
        }
    }

    pub fn as_timestamp(&self) -> Option<i64> {
        match self {
            Value::Timestamp(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Map(v) => Some(v),
            _ => None,
        }
    }

    /// Truthiness for filter evaluation.
    /// Falsy: false, 0, 0.0, "", empty list, empty map, empty bytes.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(v) => *v,
            Value::Int(v) => *v != 0,
            Value::Float(v) => *v != 0.0,
            Value::Text(v) => !v.is_empty(),
            Value::Timestamp(v) => *v != 0,
            Value::Bytes(v) => !v.is_empty(),
            Value::List(v) => !v.is_empty(),
            Value::Map(v) => !v.is_empty(),
            Value::Null => false,
            Value::Vector(v) => !v.is_empty(),
        }
    }

    /// Partial ordering for filter comparisons (>, <, >=, <=).
    /// Only comparable within the same type. Returns None for cross-type.
    pub fn partial_cmp_value(&self, other: &Value) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
            (Value::Null, _) | (_, Value::Null) => None,
            (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.partial_cmp(b),
            (Value::Bool(a), Value::Bool(b)) => a.partial_cmp(b),
            _ => None,
        }
    }

    /// Rough estimate of heap memory used by this value (for cache budget).
    pub fn estimated_size(&self) -> usize {
        match self {
            Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Timestamp(_)
            | Value::Null => 0,
            Value::Text(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::List(v) => v.iter().map(|i| 16 + i.estimated_size()).sum(),
            Value::Map(m) => m
                .iter()
                .map(|(k, v)| 48 + k.len() + v.estimated_size())
                .sum(),
            Value::Vector(v) => v.len() * std::mem::size_of::<f32>(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(v) => write!(f, "{v}"),
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{v}"),
            Value::Text(v) => write!(f, "\"{v}\""),
            Value::Timestamp(v) => {
                let secs = v / 1_000_000;
                let micros = v % 1_000_000;
                write!(f, "@ts({secs}.{micros:06})")
            }
            Value::Bytes(v) => write!(f, "<{} bytes>", v.len()),
            Value::List(v) => {
                write!(f, "[")?;
                for (i, item) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Value::Map(v) => {
                write!(f, "{{")?;
                for (i, (k, val)) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {val}")?;
                }
                write!(f, "}}")
            }
            Value::Null => write!(f, "null"),
            Value::Vector(v) => {
                write!(f, "[")?;
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{x}")?;
                }
                write!(f, "]")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names() {
        assert_eq!(Value::Bool(true).type_name(), "bool");
        assert_eq!(Value::Int(42).type_name(), "int");
        assert_eq!(Value::Float(1.23).type_name(), "float");
        assert_eq!(Value::Text("hi".into()).type_name(), "text");
    }

    #[test]
    fn truthiness() {
        assert!(Value::Bool(true).is_truthy());
        assert!(!Value::Bool(false).is_truthy());
        assert!(Value::Int(1).is_truthy());
        assert!(!Value::Int(0).is_truthy());
        assert!(Value::Text("x".into()).is_truthy());
        assert!(!Value::Text(String::new()).is_truthy());
    }

    #[test]
    fn cross_type_comparison() {
        let i = Value::Int(10);
        let f = Value::Float(10.5);
        assert_eq!(i.partial_cmp_value(&f), Some(std::cmp::Ordering::Less));
    }
}
