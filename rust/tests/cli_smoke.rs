//! Phase-5 CLI gate: drive the compiled `review-kb` binary through every
//! command group and assert the JSON envelope shape + exit code, mirroring the
//! scenarios in the Python `tests/test_cli.py`. Byte-exact stdout diffs against
//! the Python binary are Phase 6 (`tests/compat/`); here we lock the structure.
//!
//! Uses the same `tests/fixtures/valid-checklist.md` the Python suite does so a
//! later cross-binary diff needs no new fixtures.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_review-kb"))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has a parent")
        .to_path_buf()
}

fn fixture() -> PathBuf {
    repo_root().join("tests").join("fixtures").join("valid-checklist.md")
}

/// Build owned args from a `&[&str]` slice (lets us mix literals and `&String`).
fn s(slice: &[&str]) -> Vec<String> {
    slice.iter().map(|a| (*a).to_string()).collect()
}

/// Run the binary with `args`; optionally pipe `stdin` (for `--input -`).
/// Returns `(exit_code, stdout, stderr)`.
fn run(args: &[String], stdin: Option<&str>) -> (i32, String, String) {
    let mut cmd = Command::new(bin());
    cmd.args(args);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn review-kb");
    if let Some(input) = stdin {
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
    } else {
        drop(child.stdin.take()); // close stdin immediately
    }
    let output = child.wait_with_output().expect("wait");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn envelope(stdout: &str) -> Value {
    serde_json::from_str::<Value>(stdout.trim()).expect("stdout is one JSON envelope")
}

fn data(stdout: &str) -> Value {
    envelope(stdout).get("data").expect("envelope has data").clone()
}

fn error_obj(stdout: &str) -> Value {
    envelope(stdout).get("error").expect("envelope has error").clone()
}

/// Full `prepare` invocation against a DB + the fixture.
fn common(db: &str, checklist: &str) -> Vec<String> {
    let mut args = s(&["--db", db, "prepare"]);
    args.extend(tail(checklist));
    args
}

/// Shared per-command options (`--project-id/--project-name/--checklist`) for
/// `prepare`/`sync`/`rebuild`.
fn tail(checklist: &str) -> Vec<String> {
    s(&["--project-id", "123", "--project-name", "payments", "--checklist", checklist])
}

#[test]
fn prepare_then_rules_get_emit_one_line_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let db_s = dir.path().join("k.db").to_string_lossy().into_owned();
    let cl = fixture();
    let cl_s = cl.to_str().unwrap();

    let (code, out, err) = run(&common(&db_s, cl_s), None);
    assert_eq!(code, 0, "prepare failed: {err}");
    let ctx = &data(&out)["selection_context"];
    assert_eq!(ctx["project_id"], "123");
    let revision = ctx["knowledge_revision"].as_str().unwrap().to_string();

    let selection = serde_json::json!({
        "project_id": "123",
        "knowledge_revision": revision,
        "keys": ["SEC-001"],
    });
    let args = s(&["--db", &db_s, "rules", "get", "--input", "-"]);
    let (code, out, _err) = run(&args, Some(&selection.to_string()));
    assert_eq!(code, 0, "rules get failed");
    let rules = data(&out)["rules"].as_array().unwrap().clone();
    assert_eq!(rules[0]["key"], "SEC-001");
    // typer.echo emits exactly one line.
    assert_eq!(out.trim().split('\n').count(), 1);
}

#[test]
fn bad_selection_is_invalid_selection_exit_two() {
    let dir = tempfile::tempdir().unwrap();
    let db_s = dir.path().join("k.db").to_string_lossy().into_owned();
    let args = s(&["--db", &db_s, "rules", "get", "--input", "-"]);
    let (code, out, _err) = run(&args, Some("{}"));
    assert_eq!(code, 2);
    assert_eq!(error_obj(&out)["code"], "INVALID_SELECTION");
}

#[test]
fn status_does_not_create_a_missing_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("must-not-exist.db");
    let db_s = db.to_string_lossy().into_owned();
    let args = s(&["--db", &db_s, "status", "--project-id", "123", "--checklist", fixture().to_str().unwrap()]);
    let (code, out, _err) = run(&args, None);
    assert_eq!(code, 0);
    assert_eq!(data(&out)["status"], "missing");
    assert!(!db.exists(), "status must not create the DB file");
}

#[test]
fn rule_not_found_is_exit_three_with_suggestions() {
    let dir = tempfile::tempdir().unwrap();
    let db_s = dir.path().join("k.db").to_string_lossy().into_owned();
    let cl = fixture();
    let cl_s = cl.to_str().unwrap();
    let (code, out, _err) = run(&common(&db_s, cl_s), None);
    assert_eq!(code, 0);
    let ctx = data(&out)["selection_context"].clone();
    let selection = serde_json::json!({
        "project_id": "123",
        "knowledge_revision": ctx["knowledge_revision"],
        "keys": ["SEC-01"],
    });
    let args = s(&["--db", &db_s, "rules", "get", "--input", "-"]);
    let (code, out, _err) = run(&args, Some(&selection.to_string()));
    assert_eq!(code, 3);
    let error = error_obj(&out);
    assert_eq!(error["code"], "RULE_NOT_FOUND");
    assert_eq!(error["details"]["suggestions"]["SEC-01"][0], "SEC-001");
}

#[test]
fn sync_creates_and_rebuild_marks_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let db_s = dir.path().join("k.db").to_string_lossy().into_owned();
    let cl = fixture();
    let cl_s = cl.to_str().unwrap();

    let mut sync_args = s(&["--db", &db_s, "sync"]);
    sync_args.extend(tail(cl_s));
    let (code, out, err) = run(&sync_args, None);
    assert_eq!(code, 0, "sync: {err}");
    assert_eq!(data(&out)["knowledge_status"], "created");

    let mut rebuild_args = s(&["--db", &db_s, "rebuild"]);
    rebuild_args.extend(tail(cl_s));
    let (code, out, err) = run(&rebuild_args, None);
    assert_eq!(code, 0, "rebuild: {err}");
    assert_eq!(data(&out)["knowledge_status"], "rebuilt");
}

#[test]
fn override_lifecycle_updates_revision_and_content() {
    let dir = tempfile::tempdir().unwrap();
    let db_s = dir.path().join("k.db").to_string_lossy().into_owned();
    let cl = fixture();
    let cl_s = cl.to_str().unwrap();
    let (code, out, err) = run(&common(&db_s, cl_s), None);
    assert_eq!(code, 0, "prepare: {err}");
    let old_revision = data(&out)["selection_context"]["knowledge_revision"]
        .as_str()
        .unwrap()
        .to_string();

    let payload = serde_json::json!({
        "project_id": "123",
        "key": "SEC-001",
        "reason": "应急修复",
        "content": "临时规则正文",
    });
    let args = s(&["--db", &db_s, "overrides", "set", "--input", "-"]);
    let (code, out, _err) = run(&args, Some(&payload.to_string()));
    assert_eq!(code, 0);
    let new_revision = data(&out)["knowledge_revision"].as_str().unwrap().to_string();
    assert_ne!(new_revision, old_revision);

    let args = s(&["--db", &db_s, "overrides", "list", "--project-id", "123"]);
    let (code, out, _err) = run(&args, None);
    assert_eq!(code, 0);
    assert_eq!(data(&out)["overrides"][0]["status"], "active");

    let selection = serde_json::json!({
        "project_id": "123",
        "knowledge_revision": new_revision,
        "keys": ["SEC-001"],
    });
    let args = s(&["--db", &db_s, "rules", "get", "--input", "-"]);
    let (code, out, _err) = run(&args, Some(&selection.to_string()));
    assert_eq!(code, 0);
    assert_eq!(data(&out)["rules"][0]["content"], "临时规则正文");

    let args = s(&[
        "--db", &db_s, "overrides", "unset", "--project-id", "123", "--key", "SEC-001", "--reason", "应急结束",
    ]);
    let (code, out, _err) = run(&args, None);
    assert_eq!(code, 0);
    assert_eq!(data(&out)["status"], "disabled");
}

#[test]
fn override_resolve_accept_source_emits_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let db_s = dir.path().join("k.db").to_string_lossy().into_owned();
    // Copy the fixture so we can mutate it (source-content change → conflict).
    let checklist = dir.path().join("review-checklist.md");
    std::fs::write(&checklist, std::fs::read_to_string(fixture()).unwrap()).unwrap();
    let cl_s = checklist.to_str().unwrap();
    let prep = common(&db_s, cl_s);
    run(&prep, None);

    let payload = serde_json::json!({
        "project_id": "123", "key": "SEC-001", "reason": "应急", "content": "临时规则",
    });
    let args = s(&["--db", &db_s, "overrides", "set", "--input", "-"]);
    run(&args, Some(&payload.to_string()));

    // Change the source content → prepare must report OVERRIDE_CONFLICT (exit 4).
    let mutated = std::fs::read_to_string(&checklist)
        .unwrap()
        .replace("参数化处理", "安全参数绑定");
    std::fs::write(&checklist, mutated).unwrap();
    let (code, _out, _err) = run(&prep, None);
    assert_eq!(code, 4);

    let args = s(&[
        "--db", &db_s, "overrides", "resolve", "--project-id", "123", "--key", "SEC-001",
        "--strategy", "accept-source", "--checklist", cl_s, "--reason", "接受源规则",
    ]);
    let (code, out, err) = run(&args, None);
    assert_eq!(code, 0, "resolve: {err}");
    assert_eq!(data(&out)["override_resolution"]["strategy"], "accept-source");
}

fn run_env(args: &[&str], envs: &[(&str, std::ffi::OsString)]) -> (i32, Vec<u8>, Vec<u8>) {
    let mut cmd = Command::new(bin());
    cmd.args(args).env_clear();
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn");
    (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
}

#[test]
fn config_show_reports_environment_and_set_get_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let env_db = dir.path().join("from-env.db");
    let home = dir.path().as_os_str().to_owned();

    let (code, stdout, _stderr) = run_env(
        &["config", "show"],
        &[("REVIEW_KB_DB", env_db.as_os_str().to_owned()), ("HOME", home.clone())],
    );
    assert_eq!(code, 0);
    let payload: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(
        payload["data"],
        serde_json::json!({
            "db_path": env_db.to_string_lossy(),
            "source": "environment",
        })
    );

    // config set then get via REVIEW_KB_CONFIG.
    let config_path = dir.path().join("rk").join("config.toml");
    let database_path = dir.path().join("data").join("k.db");
    let (code, _stdout, stderr) = run_env(
        &["config", "set", "db_path", &database_path.to_string_lossy()],
        &[("REVIEW_KB_CONFIG", config_path.as_os_str().to_owned()), ("HOME", home.clone())],
    );
    assert_eq!(code, 0, "config set: {}", String::from_utf8_lossy(&stderr));
    assert!(config_path.exists());

    let (code, stdout, _stderr) = run_env(
        &["config", "get", "db_path"],
        &[("REVIEW_KB_CONFIG", config_path.as_os_str().to_owned()), ("HOME", home)],
    );
    assert_eq!(code, 0);
    let payload: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(
        payload["data"],
        serde_json::json!({
            "key": "db_path",
            "value": database_path.to_string_lossy(),
            "source": "config_file",
        })
    );
}

#[test]
fn db_query_backup_restore_migrate_info() {
    let dir = tempfile::tempdir().unwrap();
    let db_s = dir.path().join("k.db").to_string_lossy().into_owned();
    let backup = dir.path().join("bk.db");
    let backup_s = backup.to_string_lossy().into_owned();
    let cl = fixture();
    let cl_s = cl.to_str().unwrap();
    run(&common(&db_s, cl_s), None);

    let args = s(&["--db", &db_s, "db", "query", "--view", "rules", "--project-id", "123", "--query", "SEC-001"]);
    let (code, out, err) = run(&args, None);
    assert_eq!(code, 0, "query: {err}");
    assert_eq!(data(&out)["rows"][0]["key"], "SEC-001");

    let args = s(&["--db", &db_s, "db", "backup", "--output", &backup_s]);
    let (code, _out, err) = run(&args, None);
    assert_eq!(code, 0, "backup: {err}");
    assert!(backup.exists());

    let args = s(&["--db", &db_s, "db", "restore", "--input", &backup_s]);
    let (code, out, err) = run(&args, None);
    assert_eq!(code, 0, "restore: {err}");
    let safety = data(&out)["safety_backup"].as_str().unwrap().to_string();
    assert!(PathBuf::from(&safety).exists(), "safety backup {safety} must exist");

    let args = s(&["--db", &db_s, "db", "migrate"]);
    let (code, out, err) = run(&args, None);
    assert_eq!(code, 0, "migrate: {err}");
    assert_eq!(data(&out)["schema_version"], 2);

    let args = s(&["--db", &db_s, "db", "info"]);
    let (code, out, err) = run(&args, None);
    assert_eq!(code, 0, "info: {err}");
    assert_eq!(data(&out)["schema_version"], 2);
    assert_eq!(data(&out)["project_count"], 1);
    assert_eq!(data(&out)["rule_count"], 2);
}

#[test]
fn db_query_limit_out_of_range_exits_two_no_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let db_s = dir.path().join("k.db").to_string_lossy().into_owned();
    let cl = fixture();
    let cl_s = cl.to_str().unwrap();
    run(&common(&db_s, cl_s), None);

    let args = s(&["--db", &db_s, "db", "query", "--view", "rules", "--limit", "0"]);
    let (code, out, err) = run(&args, None);
    assert_eq!(code, 2);
    assert!(out.is_empty(), "usage errors emit no stdout envelope");
    assert!(!err.is_empty(), "usage errors write a clap message to stderr");

    let args = s(&["--db", &db_s, "db", "query", "--view", "rules", "--limit", "1001"]);
    let (code, _out, _err) = run(&args, None);
    assert_eq!(code, 2);
}
