//! Description builder + hash helpers — byte-faithful port of `review_kb/description.py`.
//!
//! `build_description` produces the value returned by `description get` and the
//! `description` field of `prepare`. `knowledge_revision` is the sha256 of the
//! canonical JSON of the *effective* knowledge (per-rule content included).

use serde_json::{json, Map, Value};

use crate::json_util::{canonical_json, sha256_hex};
use crate::models::{Checklist, Rule};

/// The per-rule summary object: `{key, summary, tags, paths, languages}` (no content).
pub fn rule_summary(rule: &Rule) -> Value {
    json!({
        "key": rule.key,
        "summary": rule.summary,
        "tags": rule.tags,
        "paths": rule.paths,
        "languages": rule.languages,
    })
}

/// Build the description value from a (already effective) checklist.
///
/// Mirrors `description.build_description`. The `knowledge_revision` hashes
/// `{"global_description": ..., "rules": [{..., "content": ...}, ...]}` with
/// per-rule content, in checklist order, canonical-JSON encoded.
pub fn build_description(project_id: &str, project_name: &str, checklist: &Checklist) -> Value {
    let summaries: Vec<Value> = checklist.rules.iter().map(rule_summary).collect();

    let effective_rules: Vec<Value> = summaries
        .iter()
        .zip(checklist.rules.iter())
        .map(|(summary, rule)| {
            // {**summary, "content": rule.content}
            let mut m = summary
                .as_object()
                .expect("rule_summary is always an object")
                .clone();
            m.insert("content".into(), Value::String(rule.content.clone()));
            Value::Object(m)
        })
        .collect();

    let effective_knowledge = json!({
        "global_description": checklist.global_description,
        "rules": effective_rules,
    });
    let revision = sha256_hex(canonical_json(&effective_knowledge).as_bytes());

    json!({
        "project": { "id": project_id, "name": project_name },
        "checklist": {
            "schema_version": checklist.schema_version,
            "version": checklist.checklist_version,
            "content_hash": checklist.content_hash,
            "knowledge_revision": revision,
        },
        "global_description": checklist.global_description,
        "rules": summaries,
    })
}

/// Extract `checklist.knowledge_revision` from a description value (or None).
pub fn knowledge_revision_of(description: &Value) -> Option<&str> {
    description
        .get("checklist")
        .and_then(|c| c.get("knowledge_revision"))
        .and_then(|v| v.as_str())
}

/// Convenience: build a `Checklist` field map (used by tests/helpers). Not in Python.
#[allow(dead_code)]
pub(crate) fn _rule_field_map(rule: &Rule) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("key".into(), Value::String(rule.key.clone()));
    m.insert("summary".into(), Value::String(rule.summary.clone()));
    m.insert("content".into(), Value::String(rule.content.clone()));
    m.insert("tags".into(), json!(rule.tags));
    m.insert("paths".into(), json!(rule.paths));
    m.insert("languages".into(), json!(rule.languages));
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_util::canonical_hash;

    fn sample_rule() -> Rule {
        Rule {
            key: "SEC-001".into(),
            summary: "检查 SQL".into(),
            content: "禁止拼接 SQL".into(),
            tags: vec!["security".into(), "database".into()],
            paths: vec!["src/**/*.py".into()],
            languages: vec!["python".into()],
            source_rule_hash: String::new(),
        }
    }

    #[test]
    fn source_rule_hash_matches_python() {
        // Captured from Python `_canonical_hash(effective)` with effective =
        // {key, summary, content, tags, paths, languages} (canonical JSON hash).
        let m = _rule_field_map(&sample_rule());
        let effective = Value::Object(m);
        assert_eq!(
            canonical_hash(&effective),
            "sha256:a052f6cba87025803aeccb9ad0a6efb8d5cf867568ed8149d7577990c0aa9877"
        );
    }

    #[test]
    fn build_description_matches_python() {
        // Captured from Python `build_description("codehub-1", "payments", cl)`
        // then `canonical_json(desc)`, for a 1-rule checklist.
        let cl = Checklist {
            schema_version: 1,
            checklist_version: "2026.07.1".into(),
            global_description: "gd".into(),
            content_hash: "sha256:abc".into(),
            rules: vec![Rule {
                key: "SEC-001".into(),
                summary: "检查 SQL".into(),
                content: "禁止拼接 SQL".into(),
                tags: vec!["security".into()],
                paths: vec!["src/**/*.py".into()],
                languages: vec!["python".into()],
                source_rule_hash: String::new(),
            }],
        };
        let desc = build_description("codehub-1", "payments", &cl);
        assert_eq!(knowledge_revision_of(&desc), Some("sha256:b2a43980e716c6da801cb64fae552adb9461f161128121dbca0edb0f5c2f1bea"));
        assert_eq!(
            canonical_json(&desc),
            concat!(
                r#"{"checklist":{"content_hash":"sha256:abc","#,
                r#""knowledge_revision":"sha256:b2a43980e716c6da801cb64fae552adb9461f161128121dbca0edb0f5c2f1bea","#,
                r#""schema_version":1,"version":"2026.07.1"},"#,
                r#""global_description":"gd","#,
                r#""project":{"id":"codehub-1","name":"payments"},"#,
                r#""rules":[{"key":"SEC-001","languages":["python"],"#,
                r#""paths":["src/**/*.py"],"summary":"检查 SQL","tags":["security"]}]}"#,
            )
        );
    }
}
