//! Cross-implementation parity: build a DB with the Python `review-kb`, then
//! read it back with this Rust `Repository` and assert byte-identical output.
//!
//! This is the Phase-3 data-layer gate from the port plan. It shells out to
//! `uv run python tests/compat/build_db.py <db>` (which uses the real Python
//! implementation), so it requires `uv` and the Python source tree at the repo
//! root. Tests are skipped (not failed) if `uv` is unavailable.

use std::path::PathBuf;
use std::process::Command;

use review_kb::json_util::canonical_json;
use review_kb::repository::Repository;
use serde_json::Value;

/// Repo root (parent of the `rust/` crate dir).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has a parent")
        .to_path_buf()
}

/// Whether the Python oracle (`uv`) is available in this environment.
fn python_available() -> bool {
    Command::new("uv")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run the Python oracle to build the DB and return its expected results.
fn python_build_db(db: &std::path::Path) -> Value {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("compat")
        .join("build_db.py");
    let output = Command::new("uv")
        .arg("run")
        .arg("python")
        .arg(&script)
        .arg(db)
        .current_dir(repo_root())
        .output()
        .expect("failed to spawn `uv run python`");
    assert!(
        output.status.success(),
        "python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("non-utf8 oracle stdout");
    // The oracle prints a single canonical-JSON line; parse it back.
    serde_json::from_str(&stdout.trim()).expect("oracle stdout is not JSON")
}

#[test]
fn rust_reads_python_built_db_byte_identically() {
    if !python_available() {
        eprintln!("skipping: `uv` not available");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("knowledge.db");

    let expected = python_build_db(&db);
    assert!(db.is_file(), "python did not create the DB file");

    // Rust reads the same DB (migrate is a no-op: versions 1,2 already applied).
    let repo = Repository::open(&db).expect("rust open");
    repo.migrate().expect("rust migrate on python DB");

    let project = repo.get_project("123").expect("get_project").expect("project exists");
    let rules = repo.list_rules("123").expect("list_rules");
    let overrides = repo.list_overrides("123", None).expect("list_overrides");
    let integrity = repo.integrity_check().expect("integrity_check");
    let schema = repo.schema_version().expect("schema_version");

    assert_eq!(
        canonical_json(&project),
        expected["project"].as_str().unwrap(),
        "project record diverges"
    );
    assert_eq!(
        canonical_json(&Value::Array(rules)),
        expected["rules"].as_str().unwrap(),
        "rules records diverge"
    );
    assert_eq!(
        canonical_json(&Value::Array(overrides)),
        expected["overrides"].as_str().unwrap(),
        "override records diverge"
    );
    assert_eq!(
        serde_json::to_string(&integrity).unwrap(),
        serde_json::to_string(&expected["integrity"]).unwrap(),
        "integrity_check diverges"
    );
    assert_eq!(schema, expected["schema"].as_i64().unwrap(), "schema_version diverges");
}
