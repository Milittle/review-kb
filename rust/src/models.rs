//! Domain models — plain structs (no serde) mirroring `review_kb/models.py`.
//!
//! All JSON serialization is explicit (via `serde_json::Value` + `canonical_json`),
//! never derived, so key ordering is controlled.

use serde_json::{json, Value};

/// A single review rule. Mirrors `models.Rule`.
/// `source_rule_hash` reflects the SOURCE rule (overrides never change it).
#[derive(Debug, Clone)]
pub struct Rule {
    pub key: String,
    pub summary: String,
    pub content: String,
    pub tags: Vec<String>,
    pub paths: Vec<String>,
    pub languages: Vec<String>,
    pub source_rule_hash: String,
}

/// A parsed checklist. Mirrors `models.Checklist`.
/// `content_hash` is the sha256 of the raw source file bytes.
#[derive(Debug, Clone)]
pub struct Checklist {
    pub schema_version: i64,
    pub checklist_version: String,
    pub global_description: String,
    pub content_hash: String,
    pub rules: Vec<Rule>,
}

/// A rule-selection request. Mirrors `models.Selection` (Pydantic model with
/// `extra="forbid"` and field/array validators).
#[derive(Debug, Clone)]
pub struct Selection {
    pub project_id: String,
    pub knowledge_revision: String,
    pub keys: Vec<String>,
}

/// Outcome of validating a selection payload. Mirrors the *observable* effect
/// of Pydantic's `ValidationError` as rendered by the Python CLI:
///
/// - `Envelope(errors)` — only "clean" errors (`missing` / `*_type`) are
///   present, so Python's `canonical_json` succeeds → the service emits the
///   `INVALID_SELECTION` envelope with `validation_errors` (exit 2).
/// - `Raw` — at least one validator `value_error` is present (empty/whitespace
///   `project_id` or `knowledge_revision`, empty/whitespace/duplicate `keys`).
///   Python's `canonical_json` then **crashes** serializing the raw
///   `ValueError` stored in each error's `ctx`; the twin replicates that as the
///   raw-escape path (exit 1, empty stdout). See `rust-port-progress` memory.
#[derive(Debug)]
pub enum SelectionError {
    Envelope(Vec<Value>),
    Raw,
}

/// Pydantic-shaped error object. Keys sort (under `canonical_json`) to
/// `input`, `loc`, `msg`, `type` — matching `error.errors(include_url=False)`.
fn error_obj(type_name: &str, loc: Value, msg: &str, input: &Value) -> Value {
    json!({
        "type": type_name,
        "loc": loc,
        "msg": msg,
        "input": input,
    })
}

/// Validate a selection payload the way Pydantic's `Selection.model_validate`
/// does, returning the `Selection` or the `SelectionError` the service renders.
///
/// Errors are collected (not short-circuited), in declaration order
/// `project_id`, `knowledge_revision`, `keys`; array-element type errors carry
/// their index in `loc`. A `value_error` from any validator makes the whole
/// result `Raw` (Python crashes serializing `ctx`), regardless of other clean
/// errors present.
pub fn validate_selection(payload: &Value) -> Result<Selection, SelectionError> {
    let obj = payload.as_object();
    let mut errors: Vec<Value> = Vec::new();
    let mut value_error = false;

    let project_id = string_field(obj, "project_id", payload, &mut errors, &mut value_error);
    let knowledge_revision =
        string_field(obj, "knowledge_revision", payload, &mut errors, &mut value_error);
    let keys = keys_field(obj, payload, &mut errors, &mut value_error);

    if value_error {
        return Err(SelectionError::Raw);
    }
    if !errors.is_empty() {
        return Err(SelectionError::Envelope(errors));
    }
    Ok(Selection {
        project_id: project_id.expect("present when no value/type/missing error"),
        knowledge_revision: knowledge_revision.expect("present when no value/type/missing error"),
        keys: keys.expect("present when no value/type/missing error"),
    })
}

/// Validate a `str` field (`project_id` / `knowledge_revision`): missing →
/// `missing`; non-string → `string_type`; otherwise run the non-empty
/// validator (→ `value_error`). Returns `Some(value)` only when valid.
fn string_field(
    obj: Option<&serde_json::Map<String, Value>>,
    field: &str,
    payload: &Value,
    errors: &mut Vec<Value>,
    value_error: &mut bool,
) -> Option<String> {
    match obj.and_then(|o| o.get(field)) {
        None => {
            errors.push(error_obj("missing", json!([field]), "Field required", payload));
            None
        }
        Some(Value::String(s)) => {
            if s.trim().is_empty() {
                *value_error = true;
            }
            Some(s.clone())
        }
        Some(other) => {
            errors.push(error_obj(
                "string_type",
                json!([field]),
                "Input should be a valid string",
                other,
            ));
            None
        }
    }
}

/// Validate the `keys` field: missing → `missing`; non-array → `list_type`;
/// array with non-string elements → per-element `string_type`; all-string
/// array → run the keys validator (non-empty / no surrounding whitespace /
/// no duplicates → `value_error`).
fn keys_field(
    obj: Option<&serde_json::Map<String, Value>>,
    payload: &Value,
    errors: &mut Vec<Value>,
    value_error: &mut bool,
) -> Option<Vec<String>> {
    match obj.and_then(|o| o.get("keys")) {
        None => {
            errors.push(error_obj("missing", json!(["keys"]), "Field required", payload));
            None
        }
        Some(Value::Array(items)) => {
            let mut keys: Vec<String> = Vec::with_capacity(items.len());
            let mut all_strings = true;
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::String(s) => keys.push(s.clone()),
                    other => {
                        all_strings = false;
                        errors.push(error_obj(
                            "string_type",
                            json!(["keys", i]),
                            "Input should be a valid string",
                            other,
                        ));
                    }
                }
            }
            if !all_strings {
                return None;
            }
            if keys.is_empty() {
                *value_error = true;
            } else if keys.iter().any(|k| k != k.trim()) {
                *value_error = true;
            } else if keys.iter().collect::<std::collections::HashSet<_>>().len() != keys.len() {
                *value_error = true;
            }
            Some(keys)
        }
        Some(other) => {
            errors.push(error_obj(
                "list_type",
                json!(["keys"]),
                "Input should be a valid list",
                other,
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_util::canonical_json;

    fn env(val: &Value) -> Result<Selection, SelectionError> {
        validate_selection(val)
    }

    fn envelope_json(res: Result<Selection, SelectionError>) -> String {
        match res {
            Err(SelectionError::Envelope(errors)) => canonical_json(&Value::Array(errors)),
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    #[test]
    fn missing_fields_match_pydantic() {
        // Captured from Python Selection.model_validate({}).errors() — errors
        // appear in field-declaration order (project_id, knowledge_revision,
        // keys); the array order is preserved under canonical_json.
        let j = envelope_json(env(&json!({})));
        assert_eq!(
            j,
            concat!(
                r#"[{"input":{},"loc":["project_id"],"msg":"Field required","type":"missing"},"#,
                r#"{"input":{},"loc":["knowledge_revision"],"msg":"Field required","type":"missing"},"#,
                r#"{"input":{},"loc":["keys"],"msg":"Field required","type":"missing"}]"#,
            )
        );
    }

    #[test]
    fn project_id_wrong_type_is_string_type() {
        let j = envelope_json(env(&json!({"project_id": 123, "knowledge_revision": "r", "keys": ["A"]})));
        assert_eq!(
            j,
            r#"[{"input":123,"loc":["project_id"],"msg":"Input should be a valid string","type":"string_type"}]"#
        );
    }

    #[test]
    fn keys_wrong_type_is_list_type() {
        let j = envelope_json(env(&json!({"project_id": "p", "knowledge_revision": "r", "keys": "A"})));
        assert_eq!(
            j,
            r#"[{"input":"A","loc":["keys"],"msg":"Input should be a valid list","type":"list_type"}]"#
        );
    }

    #[test]
    fn keys_non_string_elements_emit_per_index_errors() {
        let j = envelope_json(env(&json!({"project_id": "p", "knowledge_revision": "r", "keys": [1, 2]})));
        assert_eq!(
            j,
            concat!(
                r#"[{"input":1,"loc":["keys",0],"msg":"Input should be a valid string","type":"string_type"},"#,
                r#"{"input":2,"loc":["keys",1],"msg":"Input should be a valid string","type":"string_type"}]"#,
            )
        );
    }

    #[test]
    fn value_errors_become_raw() {
        // Each of these crashes Python (value_error with non-serializable ctx)
        // → exit 1, empty stdout. The twin surfaces them as `Raw`.
        for case in [
            json!({"project_id": "", "knowledge_revision": "r", "keys": ["A"]}),
            json!({"project_id": "p", "knowledge_revision": "  ", "keys": ["A"]}),
            json!({"project_id": "p", "knowledge_revision": "r", "keys": []}),
            json!({"project_id": "p", "knowledge_revision": "r", "keys": ["A", "A"]}),
            json!({"project_id": "p", "knowledge_revision": "r", "keys": [" A"]}),
        ] {
            assert!(matches!(env(&case), Err(SelectionError::Raw)), "expected Raw for {case}");
        }
    }

    #[test]
    fn valid_payload_builds_selection() {
        let sel = env(&json!({"project_id": "p", "knowledge_revision": "r", "keys": ["A", "B"]})).unwrap();
        assert_eq!(sel.project_id, "p");
        assert_eq!(sel.knowledge_revision, "r");
        assert_eq!(sel.keys, vec!["A".to_string(), "B".to_string()]);
    }
}
