//! Phase-6 binary parity harness.
//!
//! Runs data-driven command scenarios through BOTH the compiled Rust binary
//! (`CARGO_BIN_EXE_review-kb`) and the real Python binary (`uv run review-kb`),
//! then asserts each step's exit code + normalized stdout match byte-for-byte.
//! This is the literal drop-in contract: a caller swapping one binary for the
//! other must see identical stdout and exit status.
//!
//! Per-step normalization:
//!   - each binary's private tempdir root -> `<TMP>` (both roots map to the
//!     same token, so `db_path` etc. compare equal even though the two runs
//!     live in different tempdirs);
//!   - ISO-8601 timestamps and the safety-backup filename stamp -> `<TS>`.
//! Shared inputs (the checklist file, the base tempdir) are the SAME path for
//! both binaries within a scenario, so they already match without help.
//!
//! `{rev}` substitutes the most recent `knowledge_revision` value seen in a
//! prior step's stdout within the same run, so selection payloads stay in sync
//! without hardcoding brittle hashes. `{db}`/`{bk}`/`{cfg}`/`{cl}` are the
//! per-binary db / backup-output / config-file paths and the shared checklist.
//!
//! Requires `uv` + the Python source tree at the repo root. Skipped (not
//! failed) if `uv` is unavailable.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;

/// Shared checklist content (the same fixture the Python suite uses), written
/// to a temp file per scenario so mutation steps can rewrite it.
const CHECKLIST: &str = include_str!("../../tests/fixtures/valid-checklist.md");

// Reusable arg slices (placeholders substituted at run time).
const PREPARE: &[&str] = &[
    "--db",
    "{db}",
    "prepare",
    "--project-id",
    "p1",
    "--project-name",
    "payments",
    "--checklist",
    "{cl}",
];

/// One action in a scenario.
enum Step {
    /// Run the binary with `args`; optional raw `stdin` (for `--input -`).
    Run {
        args: &'static [&'static str],
        stdin: Option<&'static str>,
    },
    /// Rewrite the shared checklist: replace `find` with `replace`.
    Mutate {
        find: &'static str,
        replace: &'static str,
    },
}

struct Scenario {
    name: &'static str,
    /// Extra env pairs (templates, placeholder-substituted per binary).
    /// `REVIEW_KB_DB`/`REVIEW_KB_CONFIG` are always stripped from the
    /// inherited env first, so the real shell never leaks in.
    env: &'static [(&'static str, &'static str)],
    steps: &'static [Step],
}

static SCENARIOS: &[Scenario] = &[
    // S1 — prepare then every read-only command + all `db query` views.
    Scenario {
        name: "prepare_and_reads",
        env: &[],
        steps: &[
            Step::Run { args: PREPARE, stdin: None },
            Step::Run { args: &["--db", "{db}", "projects", "list"], stdin: None },
            Step::Run { args: &["--db", "{db}", "projects", "show", "--project-id", "p1"], stdin: None },
            Step::Run { args: &["--db", "{db}", "rules", "list", "--project-id", "p1"], stdin: None },
            Step::Run { args: &["--db", "{db}", "description", "get", "--project-id", "p1"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "info"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "check"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "projects"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "rules", "--project-id", "p1"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "overrides", "--project-id", "p1"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "sync_history"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "audit_log"], stdin: None },
        ],
    },
    // S2 — status: missing db (must not create), then current, then bad checklist.
    Scenario {
        name: "status_lifecycle",
        env: &[],
        steps: &[
            Step::Run { args: &["--db", "{db}", "status", "--project-id", "p1", "--checklist", "{cl}"], stdin: None },
            Step::Run { args: PREPARE, stdin: None },
            Step::Run { args: &["--db", "{db}", "status", "--project-id", "p1", "--checklist", "{cl}"], stdin: None },
            Step::Run { args: &["--db", "{db}", "status", "--project-id", "p1", "--checklist", "{db}.missing.md"], stdin: None },
        ],
    },
    // S3 — sync creates; sync again is a no-op; rebuild.
    Scenario {
        name: "sync_and_rebuild",
        env: &[],
        steps: &[
            Step::Run { args: &["--db", "{db}", "sync", "--project-id", "p1", "--project-name", "payments", "--checklist", "{cl}"], stdin: None },
            Step::Run { args: &["--db", "{db}", "sync", "--project-id", "p1", "--project-name", "payments", "--checklist", "{cl}"], stdin: None },
            Step::Run { args: &["--db", "{db}", "rebuild", "--project-id", "p1", "--project-name", "payments", "--checklist", "{cl}"], stdin: None },
        ],
    },
    // S4 — every `rules get` outcome: success, both keys, bad key (3), INVALID_SELECTION
    // variants (2), value_error raw-escape (1), revision mismatch (4).
    Scenario {
        name: "rules_get_outcomes",
        env: &[],
        steps: &[
            Step::Run { args: PREPARE, stdin: None },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"{rev}","keys":["SEC-001"]}"#) },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"{rev}","keys":["SEC-001","DB-004"]}"#) },
            // Input order is preserved (DB-004 before SEC-001), not re-sorted.
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"{rev}","keys":["DB-004","SEC-001"]}"#) },
            // One missing key among several: RULE_NOT_FOUND with not_found +
            // suggestions and NO partial `rules`.
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"{rev}","keys":["SEC-01","DB-004"]}"#) },
            // Duplicate keys -> value_error raw-escape (exit 1, empty stdout).
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"{rev}","keys":["SEC-001","SEC-001"]}"#) },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"{rev}","keys":["SEC-01"]}"#) },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some("{}") },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"{rev}"}"#) },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"  ","knowledge_revision":"{rev}","keys":["SEC-001"]}"#) },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"{rev}","keys":[]}"#) },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"sha256:0000000000000000000000000000000000000000000000000000000000000000","keys":["SEC-001"]}"#) },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some("[]") },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some("not json") },
        ],
    },
    // S5 — rules search (key / language / no match / non-ASCII casefold).
    Scenario {
        name: "rules_search",
        env: &[],
        steps: &[
            Step::Run { args: PREPARE, stdin: None },
            Step::Run { args: &["--db", "{db}", "rules", "search", "--project-id", "p1", "--query", "SEC"], stdin: None },
            Step::Run { args: &["--db", "{db}", "rules", "search", "--project-id", "p1", "--query", "python"], stdin: None },
            Step::Run { args: &["--db", "{db}", "rules", "search", "--project-id", "p1", "--query", "zzznope"], stdin: None },
            Step::Run { args: &["--db", "{db}", "rules", "search", "--project-id", "p1", "--query", "事务"], stdin: None },
        ],
    },
    // S6 — overrides set (with a duplicate tag to exercise dedup) / list / show /
    // effective rule / unset / list-disabled.
    Scenario {
        name: "overrides_lifecycle",
        env: &[],
        steps: &[
            Step::Run { args: PREPARE, stdin: None },
            Step::Run { args: &["--db", "{db}", "overrides", "set", "--input", "-"], stdin: Some(r#"{"project_id":"p1","key":"SEC-001","reason":"应急","content":"临时正文","tags":["x","x","y"]}"#) },
            Step::Run { args: &["--db", "{db}", "overrides", "list", "--project-id", "p1"], stdin: None },
            Step::Run { args: &["--db", "{db}", "overrides", "show", "--project-id", "p1", "--key", "SEC-001"], stdin: None },
            Step::Run { args: &["--db", "{db}", "rules", "get", "--input", "-"], stdin: Some(r#"{"project_id":"p1","knowledge_revision":"{rev}","keys":["SEC-001"]}"#) },
            Step::Run { args: &["--db", "{db}", "overrides", "unset", "--project-id", "p1", "--key", "SEC-001", "--reason", "done"], stdin: None },
            Step::Run { args: &["--db", "{db}", "overrides", "list", "--project-id", "p1"], stdin: None },
            Step::Run { args: &["--db", "{db}", "overrides", "show", "--project-id", "p1", "--key", "NOPE"], stdin: None },
        ],
    },
    // S7 — mutation: override set, mutate source, prepare -> OVERRIDE_CONFLICT (4),
    // resolve accept-source, prepare succeeds.
    Scenario {
        name: "override_conflict_resolve",
        env: &[],
        steps: &[
            Step::Run { args: PREPARE, stdin: None },
            Step::Run { args: &["--db", "{db}", "overrides", "set", "--input", "-"], stdin: Some(r#"{"project_id":"p1","key":"SEC-001","reason":"r","content":"TEMP"}"#) },
            Step::Mutate { find: "参数化处理", replace: "安全参数绑定" },
            Step::Run { args: PREPARE, stdin: None },
            Step::Run { args: &["--db", "{db}", "overrides", "resolve", "--project-id", "p1", "--key", "SEC-001", "--strategy", "accept-source", "--checklist", "{cl}", "--reason", "r"], stdin: None },
            Step::Run { args: PREPARE, stdin: None },
        ],
    },
    // S8 — db backup / backup-again (BACKUP_INVALID=5) / restore (safety_backup) / migrate.
    Scenario {
        name: "db_backup_restore_migrate",
        env: &[],
        steps: &[
            Step::Run { args: PREPARE, stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "backup", "--output", "{bk}"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "backup", "--output", "{bk}"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "restore", "--input", "{bk}"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "migrate"], stdin: None },
        ],
    },
    // S9 — config file flow (REVIEW_KB_CONFIG set, REVIEW_KB_DB stripped).
    Scenario {
        name: "config_file_flow",
        env: &[("REVIEW_KB_CONFIG", "{cfg}")],
        steps: &[
            Step::Run { args: &["config", "set", "db_path", "{db}"], stdin: None },
            Step::Run { args: &["config", "get", "db_path"], stdin: None },
            Step::Run { args: &["config", "show"], stdin: None },
            Step::Run { args: &["config", "get", "bogus"], stdin: None },
        ],
    },
    // S10 — config via environment (REVIEW_KB_DB set).
    Scenario {
        name: "config_environment",
        env: &[("REVIEW_KB_DB", "{db}")],
        steps: &[
            Step::Run { args: &["config", "show"], stdin: None },
            Step::Run { args: &["config", "get", "db_path"], stdin: None },
        ],
    },
    // S11 — argument/usage errors across exit codes 2 (INVALID_ARGUMENT,
    // CHECKLIST_NOT_FOUND) and the clap/Typer limit-range path (exit 2, no envelope).
    Scenario {
        name: "argument_errors",
        env: &[],
        steps: &[
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "bogus"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "rules", "--limit", "0"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "rules", "--limit", "1001"], stdin: None },
            Step::Run { args: &["--db", "{db}", "overrides", "set", "--input", "-"], stdin: Some(r#"{"project_id":"p1"}"#) },
            Step::Run { args: &["config", "set", "bogus", "{db}"], stdin: None },
            Step::Run { args: &["--db", "{db}", "prepare", "--project-id", "p1", "--project-name", "payments", "--checklist", "{db}.missing.md"], stdin: None },
        ],
    },
    // S12 — db query limit + query-filter parity.
    Scenario {
        name: "db_query_filters",
        env: &[],
        steps: &[
            Step::Run { args: PREPARE, stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "rules", "--project-id", "p1", "--query", "SEC", "--limit", "1"], stdin: None },
            Step::Run { args: &["--db", "{db}", "db", "query", "--view", "rules", "--project-id", "p1", "--limit", "1000"], stdin: None },
        ],
    },
];

// --------------------------------------------------------------------------- //
// Driver
// --------------------------------------------------------------------------- //

/// Runs a scenario's steps on one binary, collecting each Run step's
/// (exit_code, stdout). Mutation steps are applied as side effects and produce
/// no entry; since both drivers execute the same step list, Run results align
/// by index.
struct Driver {
    cmd_prefix: Vec<String>,
    root: PathBuf,
    db: PathBuf,
    backup: PathBuf,
    config: PathBuf,
    checklist: PathBuf,
    extra_env: Vec<(String, String)>,
    rev: Option<String>,
}

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

impl Driver {
    fn new(is_python: bool, base: &Path, extra_env: &[(&str, &str)]) -> Driver {
        let root = base.join(if is_python { "py" } else { "rs" });
        std::fs::create_dir_all(&root).expect("mkdir binary root");
        Driver {
            cmd_prefix: if is_python {
                vec!["uv".into(), "run".into(), "review-kb".into()]
            } else {
                vec![env!("CARGO_BIN_EXE_review-kb").into()]
            },
            root: root.clone(),
            db: root.join("k.db"),
            backup: root.join("backup.db"),
            config: root.join("config.toml"),
            checklist: base.join("cl.md"),
            extra_env: extra_env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            rev: None,
        }
    }

    /// Substitute path/revision placeholders. `{rev}` is only resolved when
    /// present — a scenario that uses it before any revision is seen is a
    /// harness bug and fails loudly.
    fn substitute(&self, text: &str) -> String {
        let mut s = text
            .replace("{db}", &self.db.to_string_lossy())
            .replace("{bk}", &self.backup.to_string_lossy())
            .replace("{cfg}", &self.config.to_string_lossy())
            .replace("{cl}", &self.checklist.to_string_lossy());
        if s.contains("{rev}") {
            let rev = self
                .rev
                .clone()
                .expect("{rev} placeholder used before any knowledge_revision was observed");
            s = s.replace("{rev}", &rev);
        }
        s
    }

    /// Build the subprocess env: inherited vars, minus REVIEW_KB_*, plus the
    /// scenario's (substituted) extras.
    fn build_env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| k != "REVIEW_KB_DB" && k != "REVIEW_KB_CONFIG")
            .collect();
        for (k, v) in &self.extra_env {
            env.retain(|(ek, _)| ek != k);
            env.push((k.clone(), self.substitute(v)));
        }
        env
    }

    fn spawn(&self, argv: &[String], stdin: Option<&str>) -> (i32, String) {
        let mut cmd = Command::new(&self.cmd_prefix[0]);
        cmd.args(&self.cmd_prefix[1..]).args(argv);
        cmd.current_dir(repo_root());
        cmd.env_clear();
        cmd.envs(self.build_env());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn binary");
        match stdin {
            Some(s) => {
                if let Some(mut h) = child.stdin.take() {
                    h.write_all(s.as_bytes()).expect("write stdin");
                }
            }
            None => {
                drop(child.stdin.take());
            }
        }
        let out = child.wait_with_output().expect("wait");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    /// Track the most recent knowledge_revision seen, for `{rev}`.
    fn update_rev(&mut self, stdout: &str) {
        let re = Regex::new(r#""knowledge_revision":"(sha256:[0-9a-f]{64})""#).unwrap();
        if let Some(caps) = re.captures(stdout) {
            self.rev = Some(caps[1].to_string());
        }
    }

    /// Reset the shared checklist to the canonical content, then run every
    /// step; return (exit, stdout) for each Run step in order.
    fn run_all(&mut self, steps: &'static [Step]) -> Vec<(i32, String)> {
        std::fs::write(&self.checklist, CHECKLIST).expect("reset checklist");
        let mut out = Vec::new();
        for step in steps {
            match step {
                Step::Mutate { find, replace } => {
                    let content = std::fs::read_to_string(&self.checklist).expect("read checklist");
                    assert!(
                        content.contains(find),
                        "mutation `find` string not present in checklist"
                    );
                    std::fs::write(&self.checklist, content.replace(find, replace))
                        .expect("write checklist");
                }
                Step::Run { args, stdin } => {
                    let argv: Vec<String> = args.iter().map(|a| self.substitute(a)).collect();
                    let sin = stdin.map(|s| self.substitute(s));
                    let (code, stdout) = self.spawn(&argv, sin.as_deref());
                    self.update_rev(&stdout);
                    out.push((code, stdout));
                }
            }
        }
        out
    }
}

/// Normalize a binary's stdout so the comparison isolates real divergences:
///   - each binary's own tempdir root -> `<TMP>`;
///   - every timestamp (ISO-8601 or safety-backup stamp) -> `<TS>`;
///   - the JSON-library parse-error suffix on the malformed-selection message
///     -> `<json-error>`. Python's `json` and Rust's `serde_json` emit
///     different *and differently-positioned* error text for the same input
///     (their tokenizers consume differently), so matching byte-for-byte would
///     mean porting CPython's scanner error-reporting. This is an accepted
///     library-level divergence — the same category as the clap/Typer
///     usage-error text the plan already carves out as non-contract — and it
///     does not affect control flow (always exit 2 / `INVALID_SELECTION`). The
///     rest of that envelope (code, `details.input`, structure, exit) is still
///     compared verbatim.
fn normalize(stdout: &str, root: &Path) -> String {
    let s = stdout.replace(root.to_string_lossy().as_ref(), "<TMP>");
    let iso = Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})").unwrap();
    let s = iso.replace_all(&s, "<TS>");
    let safe = Regex::new(r"\d{8}T\d{12}Z").unwrap();
    let s = safe.replace_all(&s, "<TS>");
    let json_err = Regex::new(r#"selection input must be one valid JSON document: [^"]*"#).unwrap();
    json_err
        .replace_all(&s, "selection input must be one valid JSON document: <json-error>")
        .into_owned()
}

#[test]
fn rust_binary_matches_python_across_scenarios() {
    if !python_available() {
        eprintln!("skipping: `uv` not available");
        return;
    }
    for scen in SCENARIOS {
        let base = tempfile::tempdir().expect("tempdir");
        let base_path = base.path().to_path_buf();

        let mut py = Driver::new(true, &base_path, scen.env);
        let mut rs = Driver::new(false, &base_path, scen.env);
        let py_res = py.run_all(scen.steps);
        let rs_res = rs.run_all(scen.steps);

        assert_eq!(
            py_res.len(),
            rs_res.len(),
            "{}: result-count drift (shouldn't happen — same step list)",
            scen.name
        );
        for (i, ((pc, po), (rc, ro))) in py_res.iter().zip(rs_res.iter()).enumerate() {
            let pn = normalize(po, &py.root);
            let rn = normalize(ro, &rs.root);
            assert_eq!(
                rc, pc,
                "{}: step #{} exit code diverges (python={} rust={})",
                scen.name, i, pc, rc
            );
            assert_eq!(
                rn, pn,
                "{}: step #{} normalized stdout diverges\n\
                 --- rust ---\n{rn}\n\
                 --- python ---\n{pn}",
                scen.name, i
            );
        }
        eprintln!("ok: {}", scen.name);
    }
}
