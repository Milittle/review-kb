//! Command-line interface — byte-faithful port of `review_kb/cli.py`.
//!
//! 24 commands across the top level + six command groups (`description`,
//! `projects`, `rules`, `overrides`, `db`, `config`), each emitting a single
//! canonical-JSON envelope line to stdout and exiting with the documented code.
//!
//! The `with_service` engine mirrors Python's `_with_service`: it opens the
//! file DB when `create_database || path.exists()` (else an in-memory DB),
//! migrates, runs the callback, pops `result["warnings"]` up to the envelope
//! top level, and emits the success envelope. Two error paths are preserved
//! exactly (see [`ServiceError`]):
//! - `Kb(ReviewKBError)` → the JSON failure envelope + `exit(code)`;
//! - `Raw(_)` → stderr message + `exit(1)`, **empty stdout** (the uncaught-
//!   exception path Python uses for raw sqlite/IO errors and the selection
//!   `value_error` case).
//!
//! `clap` owns all usage errors (missing flags, bad `--limit` range, unknown
//! commands): exit 2 with a stderr message and no envelope, matching Typer.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Map, Value};

use crate::config::{self, DatabaseLocation};
use crate::details;
use crate::errors::{ErrorCode, ReviewKBError};
use crate::json_util::canonical_json;
use crate::repository::Repository;
use crate::service::{KnowledgeService, ServiceError};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Binary entry: parse argv (clap owns usage/help), execute, `exit(code)`.
pub fn run() {
    let cli = Cli::parse();
    let code = execute(&cli);
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// clap model — global `--db`, top-level commands, and six command groups
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "review-kb", arg_required_else_help = true)]
pub struct Cli {
    /// SQLite database path (overrides REVIEW_KB_DB / config / default).
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,

    #[command(subcommand)]
    pub command: TopCommand,
}

#[derive(Subcommand, Debug)]
pub enum TopCommand {
    Prepare(PrepareArgs),
    Status(StatusArgs),
    Sync(PrepareArgs),
    Rebuild(PrepareArgs),
    Description(DescriptionArgs),
    Projects(ProjectsArgs),
    Rules(RulesArgs),
    Overrides(OverridesArgs),
    Db(DbArgs),
    Config(ConfigArgs),
}

#[derive(Args, Debug)]
pub struct PrepareArgs {
    #[arg(long)]
    pub project_id: String,
    #[arg(long)]
    pub project_name: String,
    #[arg(long)]
    pub checklist: PathBuf,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    #[arg(long)]
    pub project_id: String,
    #[arg(long)]
    pub checklist: PathBuf,
}

#[derive(Args, Debug)]
pub struct DescriptionArgs {
    #[command(subcommand)]
    pub command: DescriptionCommand,
}

#[derive(Subcommand, Debug)]
pub enum DescriptionCommand {
    Get {
        #[arg(long)]
        project_id: String,
    },
}

#[derive(Args, Debug)]
pub struct ProjectsArgs {
    #[command(subcommand)]
    pub command: ProjectsCommand,
}

#[derive(Subcommand, Debug)]
pub enum ProjectsCommand {
    List,
    Show {
        #[arg(long)]
        project_id: String,
    },
}

#[derive(Args, Debug)]
pub struct RulesArgs {
    #[command(subcommand)]
    pub command: RulesCommand,
}

#[derive(Subcommand, Debug)]
pub enum RulesCommand {
    List {
        #[arg(long)]
        project_id: String,
    },
    Get {
        /// `-` reads the selection payload from stdin; otherwise a file path.
        #[arg(long)]
        input: String,
    },
    Search {
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        query: String,
    },
}

#[derive(Args, Debug)]
pub struct OverridesArgs {
    #[command(subcommand)]
    pub command: OverridesCommand,
}

#[derive(Subcommand, Debug)]
pub enum OverridesCommand {
    Set {
        /// `-` reads the override payload from stdin; otherwise a file path.
        #[arg(long)]
        input: String,
    },
    List {
        #[arg(long)]
        project_id: String,
    },
    Show {
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        key: String,
    },
    Unset {
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        reason: String,
    },
    Resolve {
        #[arg(long)]
        project_id: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        strategy: String,
        #[arg(long)]
        checklist: PathBuf,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Args, Debug)]
pub struct DbArgs {
    #[command(subcommand)]
    pub command: DbCommand,
}

#[derive(Subcommand, Debug)]
pub enum DbCommand {
    Info,
    Check,
    Query {
        #[arg(long)]
        view: String,
        #[arg(long)]
        project_id: Option<String>,
        #[arg(long)]
        query: Option<String>,
        // Typer validates min=1/max=1000 at parse time (exit 2, no envelope);
        // clap's range parser does the same.
        #[arg(long, default_value = "100", value_parser = clap::value_parser!(i64).range(1..=1000))]
        limit: i64,
    },
    Backup {
        #[arg(long)]
        output: PathBuf,
    },
    Restore {
        #[arg(long)]
        input: PathBuf,
    },
    Migrate,
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    Show,
    /// Read a config key (positional). Only `db_path` is supported.
    Get { key: String },
    /// Write a config key (positional `<key> <value>`). Only `db_path`.
    Set { key: String, value: PathBuf },
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Resolve the database location (Python's root `--db` callback), then dispatch
/// to the matched command. Usage errors are already handled by clap in `run`.
fn execute(cli: &Cli) -> i32 {
    let env = config::real_env();
    let location = match config::resolve_database_path(cli.db.as_deref(), &env, None) {
        Ok(loc) => loc,
        Err(error) => {
            // Python: an uncaught ReviewKBError from the root callback →
            // traceback on stderr, exit 1, empty stdout.
            eprintln!("{error}");
            return 1;
        }
    };

    match &cli.command {
        TopCommand::Prepare(a) => with_service(&location, "prepare", true, |svc| {
            svc.prepare(
                a.project_id.as_str(),
                a.project_name.as_str(),
                &a.checklist.to_string_lossy(),
                false,
            )
        }),
        TopCommand::Status(a) => with_service(&location, "status", false, |svc| {
            svc.status(a.project_id.as_str(), &a.checklist.to_string_lossy())
        }),
        TopCommand::Sync(a) => with_service(&location, "sync", true, |svc| {
            svc.sync(
                a.project_id.as_str(),
                a.project_name.as_str(),
                &a.checklist.to_string_lossy(),
            )
        }),
        TopCommand::Rebuild(a) => with_service(&location, "rebuild", true, |svc| {
            svc.rebuild(
                a.project_id.as_str(),
                a.project_name.as_str(),
                &a.checklist.to_string_lossy(),
            )
        }),

        TopCommand::Description(DescriptionArgs {
            command: DescriptionCommand::Get { project_id },
        }) => with_service(&location, "description get", false, |svc| {
            svc.get_description(project_id.as_str())
        }),

        TopCommand::Projects(ProjectsArgs {
            command: ProjectsCommand::List,
        }) => with_service(&location, "projects list", false, |svc| {
            let projects = svc.list_projects()?;
            Ok(json!({ "projects": projects }))
        }),
        TopCommand::Projects(ProjectsArgs {
            command: ProjectsCommand::Show { project_id },
        }) => with_service(&location, "projects show", false, |svc| {
            svc.show_project(project_id.as_str())
        }),

        TopCommand::Rules(RulesArgs {
            command: RulesCommand::List { project_id },
        }) => with_service(&location, "rules list", false, |svc| {
            let rules = svc.list_rules(project_id.as_str())?;
            Ok(json!({ "rules": rules }))
        }),
        TopCommand::Rules(RulesArgs {
            command: RulesCommand::Get { input },
        }) => rules_get(&location, input),
        TopCommand::Rules(RulesArgs {
            command: RulesCommand::Search { project_id, query },
        }) => with_service(&location, "rules search", false, |svc| {
            let rules = svc.search_rules(project_id.as_str(), query.as_str())?;
            Ok(json!({ "rules": rules }))
        }),

        TopCommand::Overrides(OverridesArgs {
            command: OverridesCommand::Set { input },
        }) => overrides_set(&location, input),
        TopCommand::Overrides(OverridesArgs {
            command: OverridesCommand::List { project_id },
        }) => with_service(&location, "overrides list", false, |svc| {
            let overrides = svc.list_overrides(project_id.as_str())?;
            Ok(json!({ "overrides": overrides }))
        }),
        TopCommand::Overrides(OverridesArgs {
            command:
                OverridesCommand::Show { project_id, key },
        }) => with_service(&location, "overrides show", false, |svc| {
            svc.show_override(project_id.as_str(), key.as_str())
        }),
        TopCommand::Overrides(OverridesArgs {
            command:
                OverridesCommand::Unset {
                    project_id,
                    key,
                    reason,
                },
        }) => with_service(&location, "overrides unset", false, |svc| {
            svc.unset_override(project_id.as_str(), key.as_str(), reason.as_str())
        }),
        TopCommand::Overrides(OverridesArgs {
            command:
                OverridesCommand::Resolve {
                    project_id,
                    key,
                    strategy,
                    checklist,
                    reason,
                },
        }) => {
            // Python explicitly passes create_database=False for resolve.
            with_service(&location, "overrides resolve", false, |svc| {
                svc.resolve_override(
                    project_id.as_str(),
                    key.as_str(),
                    strategy.as_str(),
                    &checklist.to_string_lossy(),
                    reason.as_str(),
                )
            })
        }

        TopCommand::Db(DbArgs { command: DbCommand::Info }) => {
            with_service(&location, "db info", false, |svc| {
                let projects = svc.list_projects()?;
                let project_count = projects.len() as i64;
                let rule_count: i64 = projects
                    .iter()
                    .map(|p| p.get("rule_count").and_then(|v| v.as_i64()).unwrap_or(0))
                    .sum();
                let schema_version = svc.repository.schema_version()?;
                Ok(json!({
                    "db_path": path_str(&location.path),
                    "path_source": location.source.as_str(),
                    "project_count": project_count,
                    "rule_count": rule_count,
                    "schema_version": schema_version,
                }))
            })
        }
        TopCommand::Db(DbArgs { command: DbCommand::Check }) => {
            with_service(&location, "db check", false, |svc| {
                let integrity = svc.repository.integrity_check()?;
                Ok(json!({ "integrity": integrity }))
            })
        }
        TopCommand::Db(DbArgs {
            command:
                DbCommand::Query {
                    view,
                    project_id,
                    query,
                    limit,
                },
        }) => with_service(&location, "db query", false, |svc| {
            let rows = svc.repository.query_view(
                view,
                project_id.as_deref(),
                query.as_deref(),
                *limit,
            )?;
            Ok(json!({ "view": view, "rows": rows }))
        }),
        TopCommand::Db(DbArgs {
            command: DbCommand::Backup { output },
        }) => db_backup(&location, output),
        TopCommand::Db(DbArgs {
            command: DbCommand::Restore { input },
        }) => db_restore(&location, input),
        TopCommand::Db(DbArgs { command: DbCommand::Migrate }) => {
            with_service(&location, "db migrate", true, |svc| {
                let schema_version = svc.repository.schema_version()?;
                Ok(json!({ "schema_version": schema_version }))
            })
        }

        TopCommand::Config(ConfigArgs {
            command: ConfigCommand::Show,
        }) => config_show(&location),
        TopCommand::Config(ConfigArgs {
            command: ConfigCommand::Get { key },
        }) => config_get(&location, key),
        TopCommand::Config(ConfigArgs {
            command: ConfigCommand::Set { key, value },
        }) => config_set(key, value),
    }
}

// ---------------------------------------------------------------------------
// The `with_service` engine + the commands that don't use it
// ---------------------------------------------------------------------------

/// Mirror Python's `_with_service`: open the file DB iff `create_database ||
/// path.exists()` (else an in-memory DB), migrate, run `f`, pop `warnings` from
/// the result into the envelope, and emit. Errors route through [`finish`].
fn with_service<F>(location: &DatabaseLocation, command: &str, create_database: bool, f: F) -> i32
where
    F: FnOnce(&KnowledgeService) -> Result<Value, ServiceError>,
{
    // The repository opens/migrates here and drops at the end of this block,
    // matching Python's `finally: repository.close()`.
    let result: Result<Value, ServiceError> = (|| -> Result<Value, ServiceError> {
        let repository = if create_database || location.path.exists() {
            Repository::open(&location.path)?
        } else {
            Repository::in_memory()?
        };
        repository.migrate()?;
        let service = KnowledgeService::new(&repository);
        f(&service)
    })();
    finish(command, result)
}

/// `rules get`: read the selection payload before opening any DB (a bad input
/// yields the `rules get` failure envelope without touching the repository).
fn rules_get(location: &DatabaseLocation, input: &str) -> i32 {
    let payload = match read_selection(input) {
        Ok(payload) => payload,
        Err(error) => return emit_failure("rules get", &error),
    };
    with_service(location, "rules get", false, |svc| {
        svc.get_selected_rules(&payload)
    })
}

/// `overrides set`: read + unpack the override payload before opening any DB.
fn overrides_set(location: &DatabaseLocation, input: &str) -> i32 {
    let (project_id, key, reason, fields) =
        match read_selection(input).and_then(|payload| override_input(&payload)) {
            Ok(unpacked) => unpacked,
            Err(error) => return emit_failure("overrides set", &error),
        };
    with_service(location, "overrides set", false, |svc| {
        svc.set_override(&project_id, &key, &fields, &reason)
    })
}

/// `db backup`: manual (no `with_service`) — refuse if the DB file is absent,
/// then open+migrate+backup.
fn db_backup(location: &DatabaseLocation, output: &Path) -> i32 {
    let result: Result<Value, ServiceError> = (|| -> Result<Value, ServiceError> {
        if !location.path.is_file() {
            return Err(ReviewKBError::new(
                ErrorCode::BackupInvalid,
                format!("database file not found: {}", path_str(&location.path)),
                details!("path" => Value::String(path_str(&location.path))),
            )
            .into());
        }
        let repository = Repository::open(&location.path)?;
        repository.migrate()?;
        let result = repository.backup(output)?;
        Ok(result)
    })();
    finish("db backup", result)
}

/// `db restore`: restore a backup file into the resolved DB path.
fn db_restore(location: &DatabaseLocation, input: &Path) -> i32 {
    let result = match Repository::restore(input, &location.path) {
        Ok(value) => Ok(value),
        Err(error) => Err(ServiceError::from(error)),
    };
    finish("db restore", result)
}

// ---------------------------------------------------------------------------
// config group (no `with_service`)
// ---------------------------------------------------------------------------

fn config_show(location: &DatabaseLocation) -> i32 {
    emit_success(
        "config show",
        json!({ "db_path": path_str(&location.path), "source": location.source.as_str() }),
        Value::Array(Vec::new()),
    )
}

fn config_get(location: &DatabaseLocation, key: &str) -> i32 {
    if key != "db_path" {
        return emit_failure(
            "config get",
            &ReviewKBError::new(
                ErrorCode::InvalidArgument,
                format!("unsupported config key: {key}"),
                details!("allowed_keys" => Value::Array(vec![Value::String("db_path".into())])),
            ),
        );
    }
    emit_success(
        "config get",
        json!({ "key": key, "value": path_str(&location.path), "source": location.source.as_str() }),
        Value::Array(Vec::new()),
    )
}

fn config_set(key: &str, value: &Path) -> i32 {
    if key != "db_path" {
        return emit_failure(
            "config set",
            &ReviewKBError::new(
                ErrorCode::InvalidArgument,
                format!("unsupported config key: {key}"),
                details!("allowed_keys" => Value::Array(vec![Value::String("db_path".into())])),
            ),
        );
    }
    let env = config::real_env();
    match config::write_database_path(value, None, &env) {
        Ok(config_path) => emit_success(
            "config set",
            json!({
                "key": key,
                "value": path_str(value),
                "config_path": path_str(&config_path),
            }),
            Value::Array(Vec::new()),
        ),
        Err(error) => emit_failure("config set", &error),
    }
}

// ---------------------------------------------------------------------------
// Input parsing helpers (Python's `_read_selection` / `_override_input`)
// ---------------------------------------------------------------------------

/// Read a selection/override payload: stdin when `input == "-"`, else a UTF-8
/// file. IO and JSON-decode failures both surface as `INVALID_SELECTION`
/// "selection input must be one valid JSON document: {error}".
fn read_selection(input_path: &str) -> Result<Value, ReviewKBError> {
    let source = if input_path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| selection_error(input_path, e))?;
        buf
    } else {
        std::fs::read_to_string(input_path).map_err(|e| selection_error(input_path, e))?
    };
    let payload: Value =
        serde_json::from_str(&source).map_err(|e| selection_error(input_path, e))?;
    if !payload.is_object() {
        return Err(ReviewKBError::new(
            ErrorCode::InvalidSelection,
            "selection input must be a JSON object",
            details!("input" => Value::String(input_path.into())),
        ));
    }
    Ok(payload)
}

fn selection_error(input_path: &str, error: impl std::fmt::Display) -> ReviewKBError {
    ReviewKBError::new(
        ErrorCode::InvalidSelection,
        format!("selection input must be one valid JSON document: {error}"),
        details!("input" => Value::String(input_path.into())),
    )
}

/// Unpack an override payload into `(project_id, key, reason, fields)`.
/// `project_id`/`key`/`reason` must be strings; any other keys become the
/// override `fields` map (preserving payload order, which the service's field
/// validator iterates).
fn override_input(
    payload: &Value,
) -> Result<(String, String, String, Map<String, Value>), ReviewKBError> {
    const REQUIRED: &[&str] = &["project_id", "key", "reason"];
    let obj = payload.as_object();
    let mut missing: Vec<Value> = Vec::new();
    for field in REQUIRED {
        let present = obj
            .and_then(|m| m.get(*field))
            .and_then(|v| v.as_str())
            .is_some();
        if !present {
            missing.push(Value::String((*field).to_string()));
        }
    }
    if !missing.is_empty() {
        return Err(ReviewKBError::new(
            ErrorCode::InvalidArgument,
            "override input is missing required string fields",
            details!("fields" => Value::Array(missing)),
        ));
    }
    // Safe: nothing was missing, so `obj` is `Some` and holds the three strings.
    let obj = obj.expect("override payload is an object when no required field is missing");
    let project_id = obj["project_id"].as_str().unwrap().to_string();
    let key = obj["key"].as_str().unwrap().to_string();
    let reason = obj["reason"].as_str().unwrap().to_string();
    let fields: Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| !REQUIRED.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Ok((project_id, key, reason, fields))
}

// ---------------------------------------------------------------------------
// Envelope construction + emission
// ---------------------------------------------------------------------------

/// Build the success envelope Value (sorted on emit by `canonical_json`).
fn success_envelope(command: &str, data: Value, warnings: Value) -> Value {
    json!({
        "ok": true,
        "data": data,
        "warnings": warnings,
        "meta": { "command": command, "schema_version": 1 },
    })
}

/// Build the failure envelope Value.
fn failure_envelope(command: &str, error: &ReviewKBError) -> Value {
    json!({
        "ok": false,
        "error": error.as_value(),
        "warnings": [],
        "meta": { "command": command, "schema_version": 1 },
    })
}

/// Python's `result.pop("warnings", []) if isinstance(result, dict) else []`:
/// pull `warnings` out of an object result (defaulting to `[]`), leaving the
/// remainder as `data`.
fn split_warnings(mut data: Value) -> (Value, Value) {
    let warnings = if let Some(obj) = data.as_object_mut() {
        obj.remove("warnings")
            .unwrap_or_else(|| Value::Array(Vec::new()))
    } else {
        Value::Array(Vec::new())
    };
    (data, warnings)
}

/// Map a service outcome to a process exit code, emitting the envelope on the
/// `Kb`/success paths and a stderr diagnostic on the `Raw` path (empty stdout).
fn finish(command: &str, result: Result<Value, ServiceError>) -> i32 {
    match result {
        Ok(data) => {
            let (data, warnings) = split_warnings(data);
            emit_success(command, data, warnings)
        }
        Err(ServiceError::Kb(error)) => emit_failure(command, &error),
        Err(ServiceError::Raw(message)) => {
            // Raw-escape path: stderr + exit 1, stdout stays empty.
            eprintln!("{message}");
            1
        }
    }
}

fn emit_success(command: &str, data: Value, warnings: Value) -> i32 {
    emit_json(&success_envelope(command, data, warnings));
    0
}

fn emit_failure(command: &str, error: &ReviewKBError) -> i32 {
    emit_json(&failure_envelope(command, error));
    error.exit_code()
}

/// Write `canonical_json(value)` + `\n` to stdout (matching `typer.echo`) and
/// flush so the bytes land before any `process::exit`.
fn emit_json(value: &Value) {
    let encoded = canonical_json(value);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(encoded.as_bytes());
    let _ = handle.write_all(b"\n");
    let _ = handle.flush();
}

/// `str(Path)` equivalent (lossy; paths here are valid UTF-8).
fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_util::canonical_json;

    #[test]
    fn success_envelope_shape_is_canonical() {
        let v = success_envelope("prepare", json!({"a": 1}), json!(["w"]));
        assert_eq!(
            canonical_json(&v),
            concat!(
                r#"{"data":{"a":1},"meta":{"command":"prepare","schema_version":1},"#,
                r#""ok":true,"warnings":["w"]}"#,
            )
        );
        assert_eq!(v["meta"]["schema_version"], 1);
    }

    #[test]
    fn failure_envelope_shape_is_canonical() {
        let error = ReviewKBError::plain(ErrorCode::RuleNotFound, "missing rule");
        let v = failure_envelope("rules get", &error);
        assert_eq!(
            canonical_json(&v),
            concat!(
                r#"{"error":{"code":"RULE_NOT_FOUND","details":{},"message":"missing rule"},"#,
                r#""meta":{"command":"rules get","schema_version":1},"ok":false,"warnings":[]}"#,
            )
        );
    }

    #[test]
    fn split_warnings_pops_from_object() {
        let (data, warnings) = split_warnings(json!({"a": 1, "warnings": ["x"]}));
        assert_eq!(canonical_json(&data), r#"{"a":1}"#);
        assert_eq!(warnings, json!(["x"]));
    }

    #[test]
    fn split_warnings_defaults_when_absent_or_non_object() {
        let (data, warnings) = split_warnings(json!({"a": 1}));
        assert_eq!(canonical_json(&data), r#"{"a":1}"#);
        assert_eq!(warnings, json!([]));

        let (data, warnings) = split_warnings(json!([1, 2, 3]));
        assert_eq!(data, json!([1, 2, 3]));
        assert_eq!(warnings, json!([]));
    }

    #[test]
    fn override_input_reports_missing_fields_in_order() {
        let error = override_input(&json!({"project_id": "p"})).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        // project_id present → key, reason missing (declaration order).
        assert_eq!(error.details["fields"], json!(["key", "reason"]));
    }

    #[test]
    fn override_input_keeps_extra_fields_as_override_fields() {
        let (pid, key, reason, fields) = override_input(&json!({
            "project_id": "p",
            "key": "K",
            "reason": "r",
            "summary": "s",
            "tags": ["a", "b"],
        }))
        .unwrap();
        assert_eq!(pid, "p");
        assert_eq!(key, "K");
        assert_eq!(reason, "r");
        assert_eq!(fields["summary"], "s");
        assert_eq!(fields["tags"], json!(["a", "b"]));
        assert!(fields.get("project_id").is_none());
    }
}
