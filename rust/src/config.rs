//! Database-path resolution — byte-faithful port of `review_kb/config.py`.
//!
//! XDG-only defaults (no OS branching), `expanduser` for leading `~`, atomic
//! single-key TOML write as `db_path = {json_string}\n`. Source strings are
//! exactly `command_line | environment | config_file | platform_default`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::errors::{ErrorCode, ReviewKBError};
use crate::path_util::expanduser;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseLocation {
    pub path: PathBuf,
    pub source: String,
}

/// Environment overlay: a list of `(KEY, VALUE)` pairs (first match wins; real
/// env from `std::env::vars` has no duplicates). Tests pass controlled slices;
/// the CLI passes a snapshot of the real environment.
pub type Env = [(String, String)];

pub fn env_get(env: &Env, key: &str) -> Option<String> {
    env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// Python guards `REVIEW_KB_DB` / `REVIEW_KB_CONFIG` with truthiness
/// (`if env.get(KEY):`), so an empty value falls through. Mirror that by
/// treating an empty value as absent here. (`XDG_*` / `HOME` keep presence
/// semantics, matching Python's `environ.get(KEY, default)`.)
fn env_get_nonempty(env: &Env, key: &str) -> Option<String> {
    match env_get(env, key) {
        Some(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn home(env: &Env) -> PathBuf {
    env_get(env, "HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn default_config_path(env: &Env) -> PathBuf {
    if let Some(p) = env_get_nonempty(env, "REVIEW_KB_CONFIG") {
        return expanduser(Path::new(&p));
    }
    let root = env_get(env, "XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home(env).join(".config"));
    root.join("review-kb").join("config.toml")
}

pub fn default_database_path(env: &Env) -> PathBuf {
    let root = env_get(env, "XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home(env).join(".local").join("share"));
    root.join("review-kb").join("knowledge.db")
}

/// Read `db_path` from a TOML config file. Missing file → `None`. Parse error
/// or invalid `db_path` → `INVALID_ARGUMENT`. Returned path is `expanduser`'d.
pub fn read_configured_database_path(config_path: &Path) -> Result<Option<PathBuf>, ReviewKBError> {
    if !config_path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(config_path).map_err(|error| {
        invalid_config(config_path, &format!("{error}"))
    })?;
    let parsed: toml::Value = toml::from_str(&text).map_err(|error| {
        invalid_config(config_path, &format!("{error}"))
    })?;
    let db_path = parsed.get("db_path");
    let s = match db_path {
        Some(toml::Value::String(s)) => s,
        _ => {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "config db_path must be a non-empty string",
                details_path(config_path),
            ))
        }
    };
    if s.trim().is_empty() {
        return Err(ReviewKBError::new(
            ErrorCode::InvalidArgument,
            "config db_path must be a non-empty string",
            details_path(config_path),
        ));
    }
    Ok(Some(expanduser(Path::new(s))))
}

/// Atomically write `db_path = {json(value)}\n` to the config file (full
/// overwrite). Returns the target path. Mirrors `write_database_path`.
pub fn write_database_path(
    database_path: &Path,
    config_path: Option<&Path>,
    env: &Env,
) -> Result<PathBuf, ReviewKBError> {
    let target = config_path
        .map(PathBuf::from)
        .unwrap_or_else(|| default_config_path(env));
    let value = database_path.to_string_lossy().trim().to_string();
    if value.is_empty() {
        return Err(ReviewKBError::new(
            ErrorCode::InvalidArgument,
            "db_path must not be empty",
            field_details("db_path"),
        ));
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|error| {
        write_err(&target, &format!("{error}"))
    })?;
    let json = serde_json::to_string(&value).expect("string serializes");
    let content = format!("db_path = {json}\n");
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| write_err(&target, &format!("{error}")))?;
    use std::io::Write;
    tmp.write_all(content.as_bytes())
        .map_err(|error| write_err(&target, &format!("{error}")))?;
    tmp.persist(&target)
        .map_err(|error| write_err(&target, &format!("{error}")))?;
    Ok(target)
}

/// Resolve the database path. Priority: `--db` > `REVIEW_KB_DB` > config file
/// `db_path` > platform default. Mirrors `resolve_database_path`.
pub fn resolve_database_path(
    cli_path: Option<&Path>,
    env: &Env,
    config_path: Option<&Path>,
) -> Result<DatabaseLocation, ReviewKBError> {
    if let Some(cli) = cli_path {
        return Ok(DatabaseLocation {
            path: expanduser(cli),
            source: "command_line".into(),
        });
    }
    if let Some(db) = env_get_nonempty(env, "REVIEW_KB_DB") {
        return Ok(DatabaseLocation {
            path: expanduser(Path::new(&db)),
            source: "environment".into(),
        });
    }
    let path = config_path
        .map(PathBuf::from)
        .unwrap_or_else(|| default_config_path(env));
    if let Some(configured) = read_configured_database_path(&path)? {
        return Ok(DatabaseLocation {
            path: configured,
            source: "config_file".into(),
        });
    }
    Ok(DatabaseLocation {
        path: default_database_path(env),
        source: "platform_default".into(),
    })
}

/// Snapshot the real environment for the CLI path.
pub fn real_env() -> Vec<(String, String)> {
    std::env::vars().collect()
}

fn details_path(path: &Path) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("path".into(), Value::String(path.to_string_lossy().into_owned()));
    m
}

fn field_details(field: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("field".into(), Value::String(field.into()));
    m
}

fn invalid_config(path: &Path, error: &str) -> ReviewKBError {
    ReviewKBError::new(
        ErrorCode::InvalidArgument,
        format!("invalid config file: {error}"),
        details_path(path),
    )
}

fn write_err(target: &Path, error: &str) -> ReviewKBError {
    ReviewKBError::new(
        ErrorCode::InvalidArgument,
        format!("could not write config file: {error}"),
        details_path(target),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn resolve_sources_and_paths_match_python() {
        let cases: &[(&str, Vec<(String, String)>, &str, &str)] = &[
            // (label, env, expected_path, expected_source)
            ("platform_default", env(&[("HOME", "/root")]), "/root/.local/share/review-kb/knowledge.db", "platform_default"),
            ("xdg_data", env(&[("HOME", "/root"), ("XDG_DATA_HOME", "/xdg")]), "/xdg/review-kb/knowledge.db", "platform_default"),
            ("env_var", env(&[("REVIEW_KB_DB", "/data/x.db")]), "/data/x.db", "environment"),
        ];
        for (_label, e, path, source) in cases {
            let loc = resolve_database_path(None, e, None).unwrap();
            assert_eq!(loc.path, PathBuf::from(path), "path for {source}");
            assert_eq!(loc.source, *source);
        }
    }

    #[test]
    fn cli_path_wins_and_expandusers() {
        let e = env(&[("HOME", "/root")]);
        let loc = resolve_database_path(Some(Path::new("/cli/x.db")), &e, None).unwrap();
        assert_eq!(loc.path, PathBuf::from("/cli/x.db"));
        assert_eq!(loc.source, "command_line");

        let loc2 = resolve_database_path(Some(Path::new("~/x.db")), &e, None).unwrap();
        assert_eq!(loc2.path, PathBuf::from("/root/x.db"));
    }

    #[test]
    fn default_paths_match_python() {
        let e = env(&[("HOME", "/root")]);
        assert_eq!(
            default_database_path(&e),
            PathBuf::from("/root/.local/share/review-kb/knowledge.db")
        );
        assert_eq!(
            default_config_path(&e),
            PathBuf::from("/root/.config/review-kb/config.toml")
        );
        assert_eq!(
            default_config_path(&env(&[("HOME", "/root"), ("XDG_CONFIG_HOME", "/c")])),
            PathBuf::from("/c/review-kb/config.toml")
        );
        assert_eq!(
            default_config_path(&env(&[("HOME", "/root"), ("REVIEW_KB_CONFIG", "/custom.toml")])),
            PathBuf::from("/custom.toml")
        );
    }

    #[test]
    fn write_then_read_roundtrip_format() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let target =
            write_database_path(Path::new("/data/review-kb.db"), Some(&cfg), &env(&[])).unwrap();
        assert_eq!(target, cfg);
        let bytes = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(bytes, "db_path = \"/data/review-kb.db\"\n");
        let read = read_configured_database_path(&cfg).unwrap().unwrap();
        assert_eq!(read, PathBuf::from("/data/review-kb.db"));
    }

    #[test]
    fn write_empty_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let err = write_database_path(Path::new("   "), Some(&cfg), &env(&[])).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.message, "db_path must not be empty");
    }

    #[test]
    fn missing_config_file_returns_none() {
        let cfg = tempfile::tempdir().unwrap().path().join("nope.toml");
        assert!(read_configured_database_path(&cfg).unwrap().is_none());
    }

    #[test]
    fn empty_review_kb_env_falls_through_like_python() {
        // Python guards REVIEW_KB_DB / REVIEW_KB_CONFIG with truthiness, so an
        // empty value must NOT count as "environment"/config-file resolution.
        let e = env(&[("HOME", "/root"), ("REVIEW_KB_DB", "")]);
        let loc = resolve_database_path(None, &e, None).unwrap();
        assert_eq!(loc.source, "platform_default");
        assert_eq!(
            loc.path,
            PathBuf::from("/root/.local/share/review-kb/knowledge.db")
        );

        // Empty REVIEW_KB_CONFIG → default XDG/home config path, not "".
        assert_eq!(
            default_config_path(&env(&[("HOME", "/root"), ("REVIEW_KB_CONFIG", "")])),
            PathBuf::from("/root/.config/review-kb/config.toml")
        );
    }
}
