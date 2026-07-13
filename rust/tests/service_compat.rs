//! Cross-binary Phase-4 gate: drive the Rust `KnowledgeService` through the
//! same sequence the Python oracle (`tests/compat/service_oracle.py`) runs on
//! the real Python implementation, and assert every labelled result is
//! byte-identical under `canonical_json`.
//!
//! Requires `uv` and the Python source tree at the repo root. Skipped (not
//! failed) if `uv` is unavailable. Timestamps (`created_at`/`updated_at`) are
//! stripped from `show_project` so the comparison is deterministic.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use review_kb::json_util::canonical_json;
use review_kb::repository::Repository;
use review_kb::service::KnowledgeService;
use serde_json::{json, Map, Value};

const CHECKLIST_SRC: &str = "\
---
schema_version: 1
checklist_version: \"2026.07.1\"
global_description: project-wide guidance
---

## SEC-001

```yaml review-rule
summary: Check SQL parameterization
tags:
  - security
paths:
  - \"src/**/*.py\"
languages:
  - python
```

Do not concatenate SQL strings.

## DB-004

```yaml review-rule
summary: Check transaction boundaries
tags:
  - database
paths:
  - \"src/**/services/*.py\"
languages:
  - python
```

Wrap full business operations in a transaction.
";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has a parent")
        .to_path_buf()
}

fn python_available() -> bool {
    Command::new("uv")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Recursively drop `created_at`/`updated_at` keys (matches the Python oracle).
fn strip_timestamps(value: &Value) -> Value {
    match value {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                if k == "created_at" || k == "updated_at" {
                    continue;
                }
                out.insert(k.clone(), strip_timestamps(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(strip_timestamps).collect()),
        other => other.clone(),
    }
}

/// Run the Python oracle; returns `{label: canonical_json(value)}`.
fn python_oracle(db: &std::path::Path, checklist: &std::path::Path) -> BTreeMap<String, String> {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("compat")
        .join("service_oracle.py");
    let output = Command::new("uv")
        .arg("run")
        .arg("python")
        .arg(&script)
        .arg(db)
        .arg(checklist)
        .current_dir(repo_root())
        .output()
        .expect("failed to spawn `uv run python`");
    assert!(
        output.status.success(),
        "python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("non-utf8 oracle stdout");
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("oracle stdout is not JSON");
    let obj = parsed.as_object().expect("oracle emits an object");
    obj.iter()
        .map(|(k, v)| (k.clone(), v.as_str().expect("values are strings").to_string()))
        .collect()
}

#[test]
fn rust_service_matches_python_byte_for_byte() {
    if !python_available() {
        eprintln!("skipping: `uv` not available");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let checklist = dir.path().join("cl.md");
    std::fs::write(&checklist, CHECKLIST_SRC).expect("write checklist");
    let py_db = dir.path().join("py.db");

    let expected = python_oracle(&py_db, &checklist);

    // Drive the Rust service on its own fresh DB with the same checklist.
    let rust_db = dir.path().join("rust.db");
    let repo = Repository::open(&rust_db).expect("rust open");
    repo.migrate().expect("rust migrate");
    let svc = KnowledgeService::new(&repo);

    let mut got: BTreeMap<String, String> = BTreeMap::new();
    // The oracle strips created_at/updated_at from every result; do the same here.
    let canon = |v: &Value| canonical_json(&strip_timestamps(v));

    let prepare_create = svc
        .prepare("p1", "payments", checklist.to_str().unwrap(), false)
        .expect("prepare create");
    got.insert("prepare_create".into(), canon(&prepare_create));

    let prepare_reuse = svc
        .prepare("p1", "payments", checklist.to_str().unwrap(), false)
        .expect("prepare reuse");
    got.insert("prepare_reuse".into(), canon(&prepare_reuse));

    got.insert(
        "status_current".into(),
        canon(&svc.status("p1", checklist.to_str().unwrap()).expect("status")),
    );

    let mut fields = Map::new();
    fields.insert("summary".into(), json!("OVERRIDDEN"));
    fields.insert("tags".into(), json!(["security", "security", "extra"]));
    let set_result = svc
        .set_override("p1", "SEC-001", &fields, "why")
        .expect("set_override");
    got.insert("set_override".into(), canon(&set_result));

    let active_revision = set_result["knowledge_revision"].clone();
    let payload = json!({
        "project_id": "p1",
        "knowledge_revision": active_revision,
        "keys": ["SEC-001", "DB-004"],
    });
    got.insert(
        "get_selected_rules".into(),
        canon(&svc.get_selected_rules(&payload).expect("get_selected_rules")),
    );

    got.insert(
        "show_project".into(),
        canon(&svc.show_project("p1").expect("show_project")),
    );

    got.insert(
        "get_description".into(),
        canon(&svc.get_description("p1").expect("get_description")),
    );

    let list_rules = Value::Array(svc.list_rules("p1").expect("list_rules"));
    got.insert("list_rules".into(), canon(&list_rules));

    let list_overrides = Value::Array(svc.list_overrides("p1").expect("list_overrides"));
    got.insert("list_overrides".into(), canon(&list_overrides));

    // Compare every label; report the first divergence in full.
    let labels = [
        "prepare_create",
        "prepare_reuse",
        "status_current",
        "set_override",
        "get_selected_rules",
        "show_project",
        "get_description",
        "list_rules",
        "list_overrides",
    ];
    for label in labels {
        let py = expected.get(label).map(|s| s.as_str()).unwrap_or("<missing>");
        let rs = got.get(label).map(|s| s.as_str()).unwrap_or("<missing>");
        assert_eq!(rs, py, "label `{label}` diverges:\n  rust   = {rs}\n  python = {py}");
    }
}
