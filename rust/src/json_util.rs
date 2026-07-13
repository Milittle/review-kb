//! Canonical JSON and hashing primitives.
//!
//! `canonical_json` reproduces Python's
//! `json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))`:
//! recursively sorted keys, compact separators, raw non-ASCII (no `\uXXXX`).
//!
//! Because `serde_json` is built with `preserve_order` (so input parsing keeps
//! insertion order for error-parity elsewhere), we sort explicitly here rather
//! than relying on `BTreeMap` defaults.

use std::collections::BTreeMap;
use std::fmt::Write;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Serialize `value` with recursively sorted keys, compact, non-ASCII preserved.
pub fn canonical_json(value: &Value) -> String {
    let sorted = sort_value(value);
    serde_json::to_string(&sorted).expect("canonical json serialization is infallible")
}

/// Recursively rebuild `value` with every object's keys in sorted order.
/// Returns a fresh `Value`; the `serde_json::Map` (IndexMap with `preserve_order`)
/// receives keys in sorted insertion order, so `to_string` emits them sorted.
fn sort_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: BTreeMap<String, Value> = BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k.clone(), sort_value(v));
            }
            Value::Object(sorted.into_iter().collect::<Map<String, Value>>())
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_value).collect()),
        other => other.clone(),
    }
}

/// `"sha256:"` + lowercase hex of `sha256(bytes)`, matching `hashlib.sha256(...).hexdigest()`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(8 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest.iter() {
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

/// `"sha256:"` + hex of `sha256(canonical_json(value).utf8)` — the basis of
/// `source_rule_hash` and `knowledge_revision`.
pub fn canonical_hash(value: &Value) -> String {
    let encoded = canonical_json(value);
    sha256_hex(encoded.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_is_compact() {
        // Python: json.dumps({"b":1,"a":2}, sort_keys=True, separators=(",",":"))
        assert_eq!(canonical_json(&json!({"b": 1, "a": 2})), r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn does_not_escape_non_ascii() {
        // Python: json.dumps({"key":"café"}, ensure_ascii=False, ...) -> café raw
        assert_eq!(canonical_json(&json!({"key": "café"})), r#"{"key":"café"}"#);
        // CJK must pass through too (matches captured Python output).
        assert_eq!(
            canonical_json(&json!({"b": 1, "a": ["x", "y"], "c": "中文"})),
            r#"{"a":["x","y"],"b":1,"c":"中文"}"#
        );
    }

    #[test]
    fn sorts_nested_recursively() {
        assert_eq!(
            canonical_json(&json!({"a": {"z": 1, "a": 2}})),
            r#"{"a":{"a":2,"z":1}}"#
        );
        assert_eq!(
            canonical_json(&json!({"z": [{"b": 1, "a": 2}]})),
            r#"{"z":[{"a":2,"b":1}]}"#
        );
    }

    #[test]
    fn no_trailing_newline() {
        assert!(!canonical_json(&json!({"a": 1})).ends_with('\n'));
    }

    #[test]
    fn content_hash_matches_python() {
        // Python: "sha256:"+hashlib.sha256(b"---\nhello\n").hexdigest()
        assert_eq!(
            sha256_hex(b"---\nhello\n"),
            "sha256:336c813069a0b4997ca8eeb57e10997407ae282981f2d40aa84dd1968cafe264"
        );
    }
}
