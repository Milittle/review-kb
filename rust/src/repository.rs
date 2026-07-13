//! SQLite data layer — byte-faithful port of `review_kb/repository.py`.
//!
//! Mirrors the Python `Repository`: synchronous/autocommit connection
//! (rusqlite is autocommit by default, matching `isolation_level=None`),
//! `BEGIN IMMEDIATE`/`COMMIT` via `execute_batch`, `busy_timeout(5000)`,
//! `foreign_keys=ON`, WAL on file DBs only, embedded migrations, the online
//! backup API, and read-only-URI restore.
//!
//! Error asymmetry (preserved exactly): only `replace_project` maps sqlite
//! errors → `ReviewKBError` (`OperationalError` w/ "locked" → `DB_LOCKED`,
//! else `DB_INTEGRITY_ERROR`; other `DatabaseError` → `DB_INTEGRITY_ERROR`).
//! The override writers (`upsert_override`, `disable_override`,
//! `resolve_override`) re-raise the raw sqlite error as `RepositoryError::Sqlite`,
//! which escapes the envelope path at the CLI (eprintln + exit 1, empty stdout).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::Backup;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};
use serde_json::{json, Map, Value};

use crate::description::knowledge_revision_of;
use crate::details;
use crate::errors::{ErrorCode, ReviewKBError};
use crate::json_util::canonical_json;
use crate::models::Checklist;
use crate::path_util::expanduser;
use crate::time_util::{now_iso, restore_stamp};

/// Migrations embedded verbatim (parity test asserts byte-equality vs Python).
const MIGRATION_001: &str = include_str!("../migrations/001_initial.sql");
const MIGRATION_002: &str = include_str!("../migrations/002_overrides.sql");

/// The two migrations in apply order: `(version, sql)`.
const MIGRATIONS: &[(i64, &str)] = &[(1, MIGRATION_001), (2, MIGRATION_002)];

const LATEST_SCHEMA_VERSION: i64 = 2;

/// `view` → `ORDER BY` clause, mirroring `_QUERY_VIEWS`.
fn query_view_order(view: &str) -> Option<&'static str> {
    match view {
        "projects" => Some("project_id"),
        "rules" => Some("project_id, ordinal"),
        "overrides" => Some("project_id, rule_key"),
        "sync_history" => Some("sync_id"),
        "audit_log" => Some("audit_id"),
        _ => None,
    }
}

/// Repository errors. `Kb` flows through the JSON envelope; `Sqlite`/`Io`
/// are the raw-escape path (stderr + exit 1, empty stdout) matching Python's
/// uncaught exceptions from the override writers and filesystem failures.
#[derive(Debug)]
pub enum RepositoryError {
    Kb(ReviewKBError),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

impl From<ReviewKBError> for RepositoryError {
    fn from(e: ReviewKBError) -> Self {
        RepositoryError::Kb(e)
    }
}

impl From<rusqlite::Error> for RepositoryError {
    fn from(e: rusqlite::Error) -> Self {
        RepositoryError::Sqlite(e)
    }
}

impl From<std::io::Error> for RepositoryError {
    fn from(e: std::io::Error) -> Self {
        RepositoryError::Io(e)
    }
}

/// The SQLite repository. `path` is `:memory:` for in-memory DBs.
pub struct Repository {
    pub conn: Connection,
    pub path: PathBuf,
}

/// RAII guard for `read_transaction`: BEGIN on creation (if not already in a
/// transaction), COMMIT on normal drop, ROLLBACK on unwinding drop. Matches
/// Python's contextmanager semantics for the read-only snapshot scope.
pub struct ReadTransaction<'a> {
    conn: &'a Connection,
    owns: bool,
    done: bool,
}

impl<'a> ReadTransaction<'a> {
    fn begin(conn: &'a Connection) -> Result<Self, RepositoryError> {
        // `owns` = we are currently in autocommit (no active transaction), so
        // this guard opens — and later commits — the transaction.
        let owns = conn.is_autocommit();
        if owns {
            conn.execute_batch("BEGIN")?;
        }
        Ok(ReadTransaction { conn, owns, done: false })
    }

    /// Explicitly commit now (otherwise Drop commits on normal exit).
    pub fn commit(&mut self) -> Result<(), RepositoryError> {
        if self.owns && !self.done {
            self.conn.execute_batch("COMMIT")?;
            self.done = true;
        }
        Ok(())
    }
}

impl Drop for ReadTransaction<'_> {
    fn drop(&mut self) {
        if self.owns && !self.done {
            if std::thread::panicking() {
                if !self.conn.is_autocommit() {
                    let _ = self.conn.execute_batch("ROLLBACK");
                }
            } else {
                let _ = self.conn.execute_batch("COMMIT");
            }
        }
    }
}

impl Repository {
    const fn _latest() -> i64 {
        LATEST_SCHEMA_VERSION
    }

    /// Open a file DB: `mkdir -p` parent, autocommit, foreign_keys on,
    /// busy_timeout 5s, WAL journal mode. Mirrors `Repository.open`.
    pub fn open(path: &Path) -> Result<Self, RepositoryError> {
        let db_path = expanduser(path);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)?;
        Self::configure_file(&conn)?;
        Ok(Repository { conn, path: db_path })
    }

    /// In-memory DB: foreign_keys on, busy_timeout 5s (no WAL).
    pub fn in_memory() -> Result<Self, RepositoryError> {
        let conn = Connection::open_in_memory()?;
        Self::configure_memory(&conn)?;
        Ok(Repository { conn, path: PathBuf::from(":memory:") })
    }

    fn configure_file(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch("PRAGMA busy_timeout=5000")?;
        conn.execute_batch("PRAGMA foreign_keys=ON")?;
        conn.execute_batch("PRAGMA journal_mode=WAL")?;
        Ok(())
    }

    fn configure_memory(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch("PRAGMA busy_timeout=5000")?;
        conn.execute_batch("PRAGMA foreign_keys=ON")?;
        Ok(())
    }

    pub fn close(self) {
        // Connection drops closed; method exists for API parity with Python.
        drop(self);
    }

    /// Read-only snapshot scope. `owns` is true when this guard opened the
    /// transaction (so it commits on drop).
    pub fn read_transaction(&self) -> Result<ReadTransaction<'_>, RepositoryError> {
        ReadTransaction::begin(&self.conn)
    }

    /// Apply pending migrations. Mirrors `migrate`, including the
    /// schema-too-new (`DB_SCHEMA_UNSUPPORTED`) and migration-failure
    /// (`DB_INTEGRITY_ERROR`) error shapes.
    pub fn migrate(&self) -> Result<(), RepositoryError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations \
             (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        )?;
        let applied: HashSet<i64> = self
            .conn
            .prepare("SELECT version FROM schema_migrations")?
            .query_map([], |row| row.get::<_, i64>(0))?
            .filter_map(Result::ok)
            .collect();
        let newest = applied.iter().copied().max().unwrap_or(0);
        if newest > LATEST_SCHEMA_VERSION {
            return Err(ReviewKBError::new(
                ErrorCode::DbSchemaUnsupported,
                "database schema is newer than this CLI supports",
                details!(
                    "schema_version" => Value::Number(newest.into()),
                    "supported" => Value::Number(LATEST_SCHEMA_VERSION.into()),
                    "path" => Value::String(self.path.to_string_lossy().into_owned())
                ),
            )
            .into());
        }
        for (version, sql) in MIGRATIONS {
            if applied.contains(version) {
                continue;
            }
            let result = self
                .conn
                .execute_batch(sql)
                .and_then(|_| {
                    self.conn.execute(
                        "INSERT INTO schema_migrations(version, applied_at) VALUES (?, ?)",
                        rusqlite::params![version, now_iso()],
                    )
                });
            if let Err(error) = result {
                return Err(ReviewKBError::new(
                    ErrorCode::DbIntegrityError,
                    format!("database migration failed: {error}"),
                    details!("path" => Value::String(self.path.to_string_lossy().into_owned())),
                )
                .into());
            }
        }
        Ok(())
    }

    /// `COALESCE(MAX(version), 0)` from `schema_migrations`.
    pub fn schema_version(&self) -> Result<i64, RepositoryError> {
        let row: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )?;
        Ok(row)
    }

    /// Generic view query with optional project filter and a casefold
    /// substring `query` applied to the canonical-JSON of each record.
    /// Mirrors `query_view`.
    pub fn query_view(
        &self,
        view: &str,
        project_id: Option<&str>,
        query: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Value>, RepositoryError> {
        let order = query_view_order(view).ok_or_else(|| {
            ReviewKBError::new(
                ErrorCode::InvalidArgument,
                format!("unsupported database view: {view}"),
                details!(
                    "allowed_views" => Value::Array(
                        ["audit_log", "overrides", "projects", "rules", "sync_history"]
                            .iter()
                            .map(|s| Value::String((*s).into()))
                            .collect()
                    )
                ),
            )
        })?;
        if !(1..=1000).contains(&limit) {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "query limit must be between 1 and 1000",
                details!("limit" => Value::Number(limit.into())),
            )
            .into());
        }
        let mut sql = format!("SELECT * FROM {view}");
        let mut params: Vec<&str> = Vec::new();
        if let Some(pid) = project_id {
            sql.push_str(" WHERE project_id = ?");
            params.push(pid);
        }
        sql.push_str(&format!(" ORDER BY {order}"));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            record_for_view(row, view)
        })?;
        let needle = query.map(|q| q.to_lowercase());
        let mut records: Vec<Value> = Vec::new();
        for row in rows {
            let record = row?;
            if let Some(needle) = &needle {
                if !canonical_json(&record).to_lowercase().contains(needle) {
                    continue;
                }
            }
            records.push(record);
            if records.len() as i64 == limit {
                break;
            }
        }
        Ok(records)
    }

    /// Online backup to a fresh file with an integrity check. Mirrors `backup`.
    pub fn backup(&self, output: &Path) -> Result<Value, RepositoryError> {
        let target = expanduser(output);
        if target.exists() {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                format!("backup output already exists: {}", target.display()),
                details!("path" => Value::String(target.to_string_lossy().into_owned())),
            )
            .into());
        }
        if self.path != Path::new(":memory:")
            && crate::path_util::lexpath_resolve(&target)
                == crate::path_util::lexpath_resolve(&self.path)
        {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "backup output must differ from the active database",
                details!("path" => Value::String(target.to_string_lossy().into_owned())),
            )
            .into());
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut destination = Connection::open(&target)?;
        let backup_result = {
            let backup = Backup::new(&self.conn, &mut destination)?;
            backup.run_to_completion(100, Duration::from_millis(250), None)
        };
        if let Err(error) = backup_result {
            let _ = std::fs::remove_file(&target);
            return Err(ReviewKBError::new(
                ErrorCode::BackupInvalid,
                format!("database backup failed: {error}"),
                details!("path" => Value::String(target.to_string_lossy().into_owned())),
            )
            .into());
        }
        let integrity = self.integrity_check_with(&destination)?;
        if integrity != vec!["ok".to_string()] {
            let _ = std::fs::remove_file(&target);
            return Err(ReviewKBError::new(
                ErrorCode::BackupInvalid,
                "created backup failed integrity check",
                details!(
                    "path" => Value::String(target.to_string_lossy().into_owned()),
                    "integrity" => Value::Array(
                        integrity.into_iter().map(Value::String).collect()
                    )
                ),
            )
            .into());
        }
        let schema_version = self.schema_version()?;
        Ok(json!({
            "path": target.to_string_lossy(),
            "schema_version": schema_version,
        }))
    }

    /// Restore a backup file into `destination`, with a safety backup of any
    /// existing DB and an atomic replace. Mirrors the classmethod `restore`.
    pub fn restore(source: &Path, destination: &Path) -> Result<Value, RepositoryError> {
        let source_path = expanduser(source);
        let destination_path = expanduser(destination);
        if crate::path_util::lexpath_resolve(&source_path)
            == crate::path_util::lexpath_resolve(&destination_path)
        {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "backup source and restore destination must differ",
                details!("path" => Value::String(source_path.to_string_lossy().into_owned())),
            )
            .into());
        }
        if !source_path.is_file() {
            return Err(ReviewKBError::new(
                ErrorCode::BackupInvalid,
                format!("backup file not found: {}", source_path.display()),
                details!("path" => Value::String(source_path.to_string_lossy().into_owned())),
            )
            .into());
        }
        if let Some(parent) = destination_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let dest_name = destination_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dest_parent = destination_path.parent().unwrap_or_else(|| Path::new("."));

        let uri = format!("{}?mode=ro", file_uri(&crate::path_util::lexpath_resolve(&source_path)));
        let ro_flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;

        let mut safety_backup: Option<PathBuf> = None;
        // Track the temp file path so we can clean it up on any failure path.
        let mut temp_path: Option<PathBuf> = None;

        let source_result = (|| -> Result<Value, RepositoryError> {
            let source_connection =
                Connection::open_with_flags(&uri, ro_flags)?;
            let integrity = self::integrity_check_conn(&source_connection)?;
            if integrity != vec!["ok".to_string()] {
                return Err(ReviewKBError::new(
                    ErrorCode::BackupInvalid,
                    "backup failed integrity check",
                    details!(
                        "path" => Value::String(source_path.to_string_lossy().into_owned()),
                        "integrity" => Value::Array(
                            integrity.into_iter().map(Value::String).collect()
                        )
                    ),
                )
                .into());
            }
            let version = source_connection
                .query_row(
                    "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|_| {
                    ReviewKBError::new(
                        ErrorCode::BackupInvalid,
                        "backup does not contain a review-kb schema",
                        details!("path" => Value::String(
                            source_path.to_string_lossy().into_owned(),
                        )),
                    )
                })?;
            if version > LATEST_SCHEMA_VERSION {
                return Err(ReviewKBError::new(
                    ErrorCode::DbSchemaUnsupported,
                    "backup schema is newer than this CLI supports",
                    details!(
                        "schema_version" => Value::Number(version.into()),
                        "supported" => Value::Number(LATEST_SCHEMA_VERSION.into())
                    ),
                )
                .into());
            }
            if destination_path.exists() {
                let stamp = restore_stamp();
                let safety = destination_path.with_file_name(format!(
                    "{dest_name}.safety-{stamp}.db"
                ));
                {
                    let existing = Repository::open(&destination_path)?;
                    existing.backup(&safety)?;
                    existing.close();
                }
                safety_backup = Some(safety);
            }
            let stamp = restore_stamp();
            let tmp = dest_parent.join(format!(".{dest_name}.restore-{stamp}"));
            temp_path = Some(tmp.clone());

            {
                let mut target_connection = Connection::open(&tmp)?;
                let backup = Backup::new(&source_connection, &mut target_connection)?;
                backup.run_to_completion(100, Duration::from_millis(250), None)?;
            }
            for suffix in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(format!(
                    "{}{suffix}",
                    destination_path.to_string_lossy()
                ));
            }
            std::fs::rename(&tmp, &destination_path)?;
            temp_path = None;

            Ok(json!({
                "path": destination_path.to_string_lossy(),
                "source": source_path.to_string_lossy(),
                "schema_version": version,
                "safety_backup": safety_backup
                    .as_ref()
                    .map(|p| Value::String(p.to_string_lossy().into_owned()))
                    .unwrap_or(Value::Null),
            }))
        })();

        // Map a raw sqlite error from the restore body to BACKUP_INVALID.
        let mapped = match source_result {
            Ok(v) => Ok(v),
            Err(RepositoryError::Sqlite(error)) => Err(ReviewKBError::new(
                ErrorCode::BackupInvalid,
                format!("database restore failed: {error}"),
                details!("path" => Value::String(source_path.to_string_lossy().into_owned())),
            )
            .into()),
            Err(other) => Err(other),
        };

        // finally: clean up an abandoned temp file.
        if let Some(tmp) = temp_path {
            let _ = std::fs::remove_file(&tmp);
        }
        mapped
    }

    /// Insert into `audit_log`. Mirrors `_audit`. Free function so the
    /// override writers can call it from a `run_write_tx` closure holding a
    /// `&Connection`.
    fn audit(
        conn: &Connection,
        project_id: &str,
        rule_key: Option<&str>,
        action: &str,
        reason: &str,
        change: &Value,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO audit_log(project_id, rule_key, action, reason, change_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                project_id,
                rule_key,
                action,
                reason,
                canonical_json(change),
                now_iso(),
            ],
        )?;
        Ok(())
    }

    // ---- overrides ----

    /// Build the override record value from a row. Mirrors `_override_dict`:
    /// tags_json/paths_json/languages_json are decoded (or null) into
    /// tags/paths/languages.
    fn override_dict(row: &Row<'_>) -> Result<Value, rusqlite::Error> {
        let tags = json_array_or_null(row.get::<_, Option<String>>("tags_json")?.as_deref());
        let paths = json_array_or_null(row.get::<_, Option<String>>("paths_json")?.as_deref());
        let languages =
            json_array_or_null(row.get::<_, Option<String>>("languages_json")?.as_deref());
        Ok(json!({
            "project_id": row.get::<_, String>("project_id")?,
            "rule_key": row.get::<_, String>("rule_key")?,
            "summary": row.get::<_, Option<String>>("summary")?,
            "content": row.get::<_, Option<String>>("content")?,
            "base_source_rule_hash": row.get::<_, String>("base_source_rule_hash")?,
            "status": row.get::<_, String>("status")?,
            "reason": row.get::<_, String>("reason")?,
            "created_at": row.get::<_, String>("created_at")?,
            "updated_at": row.get::<_, String>("updated_at")?,
            "tags": tags,
            "paths": paths,
            "languages": languages,
        }))
    }

    pub fn get_override(
        &self,
        project_id: &str,
        rule_key: &str,
    ) -> Result<Option<Value>, RepositoryError> {
        let row = self.conn.query_row(
            "SELECT * FROM rule_overrides WHERE project_id = ? AND rule_key = ?",
            rusqlite::params![project_id, rule_key],
            Self::override_dict,
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_overrides(
        &self,
        project_id: &str,
        statuses: Option<&[String]>,
    ) -> Result<Vec<Value>, RepositoryError> {
        let records = match statuses {
            Some(statuses) if !statuses.is_empty() => {
                let placeholders = statuses
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "SELECT * FROM rule_overrides \
                     WHERE project_id = ? AND status IN ({placeholders}) ORDER BY rule_key"
                );
                let mut params: Vec<&str> = vec![project_id];
                for s in statuses {
                    params.push(s.as_str());
                }
                self.conn.prepare(&sql)?
                    .query_map(rusqlite::params_from_iter(params), Self::override_dict)?
                    .collect::<Result<Vec<_>, _>>()?
            }
            _ => self.conn.prepare(
                "SELECT * FROM rule_overrides WHERE project_id = ? ORDER BY rule_key",
            )?
            .query_map([project_id], Self::override_dict)?
            .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(records)
    }

    /// Upsert an override. RULE_NOT_FOUND if the source rule is missing.
    /// Raw sqlite errors (e.g. lock contention) re-raise as `Sqlite`.
    pub fn upsert_override(
        &self,
        project_id: &str,
        rule_key: &str,
        fields: &Map<String, Value>,
        reason: &str,
    ) -> Result<(), RepositoryError> {
        let source_hash: Option<String> = self.conn.query_row(
            "SELECT source_rule_hash FROM rules WHERE project_id = ? AND rule_key = ?",
            rusqlite::params![project_id, rule_key],
            |row| row.get(0),
        ).ok();
        let source_hash = match source_hash {
            Some(h) => h,
            None => {
                return Err(ReviewKBError::new(
                    ErrorCode::RuleNotFound,
                    format!("rule not found: {rule_key}"),
                    details!(
                        "project_id" => Value::String(project_id.into()),
                        "key" => Value::String(rule_key.into())
                    ),
                )
                .into())
            }
        };
        let now = now_iso();
        let arrays = ["tags", "paths", "languages"].map(|name| {
            fields
                .get(name)
                .map(|v| canonical_json(v))
        });
        let summary = opt_str_field(fields, "summary");
        let content = opt_str_field(fields, "content");

        self.run_write_tx(|conn| {
            conn.execute(
                "INSERT INTO rule_overrides(
                    project_id, rule_key, summary, content, tags_json, paths_json,
                    languages_json, base_source_rule_hash, status, reason, created_at, updated_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?)
                 ON CONFLICT(project_id, rule_key) DO UPDATE SET
                    summary=excluded.summary,
                    content=excluded.content,
                    tags_json=excluded.tags_json,
                    paths_json=excluded.paths_json,
                    languages_json=excluded.languages_json,
                    base_source_rule_hash=excluded.base_source_rule_hash,
                    status='active', reason=excluded.reason, updated_at=excluded.updated_at",
                rusqlite::params![
                    project_id,
                    rule_key,
                    summary,
                    content,
                    arrays[0],
                    arrays[1],
                    arrays[2],
                    source_hash,
                    reason,
                    now,
                    now,
                ],
            )?;
            // audit uses the raw fields value.
            let change = Value::Object(fields.clone());
            Self::audit(conn, project_id, Some(rule_key), "override.set", reason, &change)?;
            Ok(())
        })
    }

    /// Mark an override `disabled`. Mirrors `disable_override`.
    pub fn disable_override(
        &self,
        project_id: &str,
        rule_key: &str,
        reason: &str,
    ) -> Result<(), RepositoryError> {
        if self.get_override(project_id, rule_key)?.is_none() {
            return Err(ReviewKBError::new(
                ErrorCode::RuleNotFound,
                format!("override not found: {rule_key}"),
                details!(
                    "project_id" => Value::String(project_id.into()),
                    "key" => Value::String(rule_key.into())
                ),
            )
            .into());
        }
        self.run_write_tx(|conn| {
            conn.execute(
                "UPDATE rule_overrides SET status = 'disabled', reason = ?, updated_at = ?
                 WHERE project_id = ? AND rule_key = ?",
                rusqlite::params![reason, now_iso(), project_id, rule_key],
            )?;
            Self::audit(conn, project_id, Some(rule_key), "override.unset", reason, &json!({}))?;
            Ok(())
        })
    }

    /// Bulk-mark overrides `conflict`. Mirrors `mark_override_conflicts`.
    pub fn mark_override_conflicts(
        &self,
        project_id: &str,
        keys: &[String],
    ) -> Result<(), RepositoryError> {
        if keys.is_empty() {
            return Ok(());
        }
        let placeholders = keys.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE rule_overrides SET status = 'conflict', updated_at = ?
             WHERE project_id = ? AND rule_key IN ({placeholders})"
        );
        let now = now_iso();
        let mut params: Vec<&str> = vec![&now, project_id];
        for k in keys {
            params.push(k.as_str());
        }
        self.conn.execute(&sql, rusqlite::params_from_iter(params))?;
        Ok(())
    }

    /// Resolve a conflicting override (keep it, or accept the source).
    /// Mirrors `resolve_override`.
    pub fn resolve_override(
        &self,
        project_id: &str,
        rule_key: &str,
        keep: bool,
        base_source_rule_hash: Option<&str>,
        reason: &str,
    ) -> Result<(), RepositoryError> {
        let override_row = self.get_override(project_id, rule_key)?;
        let status = override_row
            .as_ref()
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_str())
            .map(String::from);
        if override_row.is_none() {
            return Err(ReviewKBError::new(
                ErrorCode::RuleNotFound,
                format!("override not found: {rule_key}"),
                details!(
                    "project_id" => Value::String(project_id.into()),
                    "key" => Value::String(rule_key.into())
                ),
            )
            .into());
        }
        let status = status.unwrap_or_default();
        if status != "conflict" {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "only a conflicting override can be resolved",
                details!(
                    "project_id" => Value::String(project_id.into()),
                    "key" => Value::String(rule_key.into()),
                    "status" => Value::String(status)
                ),
            )
            .into());
        }
        if keep && base_source_rule_hash.is_none() {
            return Err(ReviewKBError::new(
                ErrorCode::RuleNotFound,
                format!("source rule not found: {rule_key}"),
                details!(
                    "project_id" => Value::String(project_id.into()),
                    "key" => Value::String(rule_key.into())
                ),
            )
            .into());
        }
        let now = now_iso();
        let action = if keep {
            "override.resolve.keep"
        } else {
            "override.resolve.accept_source"
        };
        self.run_write_tx(|conn| {
            if keep {
                conn.execute(
                    "UPDATE rule_overrides
                     SET base_source_rule_hash = ?, status = 'active', reason = ?, updated_at = ?
                     WHERE project_id = ? AND rule_key = ?",
                    rusqlite::params![
                        base_source_rule_hash,
                        reason,
                        now,
                        project_id,
                        rule_key
                    ],
                )?;
            } else {
                conn.execute(
                    "UPDATE rule_overrides
                     SET status = 'disabled', reason = ?, updated_at = ?
                     WHERE project_id = ? AND rule_key = ?",
                    rusqlite::params![reason, now, project_id, rule_key],
                )?;
            }
            let change = json!({ "base_source_rule_hash": base_source_rule_hash });
            Self::audit(conn, project_id, Some(rule_key), action, reason, &change)?;
            Ok(())
        })
    }

    // ---- effective description / projects / rules ----

    /// Write the effective description onto the project row. Mirrors
    /// `update_effective_description`.
    pub fn update_effective_description(
        &self,
        project_id: &str,
        description: &Value,
    ) -> Result<(), RepositoryError> {
        let revision = knowledge_revision_of(description)
            .ok_or_else(|| {
                ReviewKBError::plain(
                    ErrorCode::InternalError,
                    "description missing checklist.knowledge_revision",
                )
            })?;
        self.conn.execute(
            "UPDATE projects SET knowledge_revision = ?, description_json = ?, updated_at = ?
             WHERE project_id = ?",
            rusqlite::params![
                revision,
                canonical_json(description),
                now_iso(),
                project_id
            ],
        )?;
        Ok(())
    }

    /// Return the raw audit rows (newest first), including `change_json`.
    /// Mirrors `list_audit_log` (raw dict, no decoding).
    pub fn list_audit_log(&self, project_id: &str) -> Result<Vec<Value>, RepositoryError> {
        self.conn
            .prepare("SELECT * FROM audit_log WHERE project_id = ? ORDER BY audit_id DESC")?
            .query_map([project_id], row_to_object)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn project_row(&self, project_id: &str) -> Result<Option<Value>, RepositoryError> {
        let row = self.conn.query_row(
            "SELECT p.*, (SELECT COUNT(*) FROM rules r WHERE r.project_id = p.project_id) AS rule_count
             FROM projects p WHERE p.project_id = ?",
            [project_id],
            row_to_object,
        );
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get_project(&self, project_id: &str) -> Result<Option<Value>, RepositoryError> {
        self.project_row(project_id)
    }

    /// Full project refresh: upsert project row, replace all rules, append a
    /// sync_history row. Mirrors `replace_project`, including the sqlite-error
    /// → `ReviewKBError` mapping (`DB_LOCKED` / `DB_INTEGRITY_ERROR`).
    pub fn replace_project(
        &self,
        project_id: &str,
        project_name: &str,
        checklist_path: &str,
        checklist: &Checklist,
        description: &Value,
        action: &str,
        warnings: &[String],
    ) -> Result<(), RepositoryError> {
        let current = self.project_row(project_id)?;
        let now = now_iso();
        let revision = knowledge_revision_of(description)
            .ok_or_else(|| {
                ReviewKBError::plain(
                    ErrorCode::InternalError,
                    "description missing checklist.knowledge_revision",
                )
            })?;
        let old_version = current
            .as_ref()
            .and_then(|c| c.get("checklist_version"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let old_hash = current
            .as_ref()
            .and_then(|c| c.get("content_hash"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let tx_result = self.conn.execute_batch("BEGIN IMMEDIATE").and_then(|_| {
            let r = (|| -> rusqlite::Result<()> {
                self.conn.execute(
                    "INSERT INTO projects(
                        project_id, project_name, checklist_path, schema_version,
                        checklist_version, content_hash, knowledge_revision,
                        description_json, created_at, updated_at
                     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(project_id) DO UPDATE SET
                        project_name=excluded.project_name,
                        checklist_path=excluded.checklist_path,
                        schema_version=excluded.schema_version,
                        checklist_version=excluded.checklist_version,
                        content_hash=excluded.content_hash,
                        knowledge_revision=excluded.knowledge_revision,
                        description_json=excluded.description_json,
                        updated_at=excluded.updated_at",
                    rusqlite::params![
                        project_id,
                        project_name,
                        checklist_path,
                        checklist.schema_version,
                        checklist.checklist_version,
                        checklist.content_hash,
                        revision,
                        canonical_json(description),
                        now,
                        now,
                    ],
                )?;
                self.conn.execute(
                    "DELETE FROM rules WHERE project_id = ?",
                    [project_id],
                )?;
                {
                    let mut stmt = self.conn.prepare(
                        "INSERT INTO rules(
                            project_id, rule_key, ordinal, summary, content,
                            tags_json, paths_json, languages_json, source_rule_hash
                         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )?;
                    for (ordinal, rule) in checklist.rules.iter().enumerate() {
                        let ordinal_i64 = ordinal as i64;
                        stmt.execute(rusqlite::params![
                            project_id,
                            rule.key,
                            ordinal_i64,
                            rule.summary,
                            rule.content,
                            canonical_json(&json!(rule.tags)),
                            canonical_json(&json!(rule.paths)),
                            canonical_json(&json!(rule.languages)),
                            rule.source_rule_hash,
                        ])?;
                    }
                }
                self.conn.execute(
                    "INSERT INTO sync_history(
                        project_id, action, old_version, new_version, old_hash,
                        new_hash, result, warnings_json, created_at
                     ) VALUES (?, ?, ?, ?, ?, ?, 'success', ?, ?)",
                    rusqlite::params![
                        project_id,
                        action,
                        old_version,
                        checklist.checklist_version,
                        old_hash,
                        checklist.content_hash,
                        canonical_json(&Value::Array(
                            warnings.iter().map(|w| Value::String(w.clone())).collect()
                        )),
                        now,
                    ],
                )?;
                Ok(())
            })();
            match r {
                Ok(()) => {
                    let _ = self.conn.execute_batch("COMMIT");
                    Ok(())
                }
                Err(e) => {
                    if !self.conn.is_autocommit() {
                        let _ = self.conn.execute_batch("ROLLBACK");
                    }
                    Err(e)
                }
            }
        });
        if let Err(error) = tx_result {
            let code = if is_locked_error(&error) {
                ErrorCode::DbLocked
            } else {
                ErrorCode::DbIntegrityError
            };
            return Err(ReviewKBError::new(
                code,
                format!("database write failed: {error}"),
                details!("path" => Value::String(self.path.to_string_lossy().into_owned())),
            )
            .into());
        }
        Ok(())
    }

    /// Update project name/path/description without touching rules.
    /// Mirrors `update_project_metadata`.
    pub fn update_project_metadata(
        &self,
        project_id: &str,
        project_name: &str,
        checklist_path: &str,
        description: &Value,
    ) -> Result<(), RepositoryError> {
        self.conn.execute(
            "UPDATE projects
             SET project_name = ?, checklist_path = ?, description_json = ?, updated_at = ?
             WHERE project_id = ?",
            rusqlite::params![
                project_name,
                checklist_path,
                canonical_json(description),
                now_iso(),
                project_id
            ],
        )?;
        Ok(())
    }

    fn rule_dict(row: &Row<'_>) -> Result<Value, rusqlite::Error> {
        let tags_json: String = row.get("tags_json")?;
        let paths_json: String = row.get("paths_json")?;
        let languages_json: String = row.get("languages_json")?;
        Ok(json!({
            "key": row.get::<_, String>("rule_key")?,
            "summary": row.get::<_, String>("summary")?,
            "content": row.get::<_, String>("content")?,
            "tags": json_array_or_null(Some(&tags_json)),
            "paths": json_array_or_null(Some(&paths_json)),
            "languages": json_array_or_null(Some(&languages_json)),
            "source_rule_hash": row.get::<_, String>("source_rule_hash")?,
            "ordinal": row.get::<_, i64>("ordinal")?,
        }))
    }

    pub fn list_projects(&self) -> Result<Vec<Value>, RepositoryError> {
        self.conn
            .prepare(
                "SELECT p.*, (SELECT COUNT(*) FROM rules r WHERE r.project_id = p.project_id) AS rule_count
                 FROM projects p ORDER BY p.project_id",
            )?
            .query_map([], row_to_object)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn list_rules(&self, project_id: &str) -> Result<Vec<Value>, RepositoryError> {
        self.conn
            .prepare("SELECT * FROM rules WHERE project_id = ? ORDER BY ordinal")?
            .query_map([project_id], Self::rule_dict)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Rules for the given keys, in input order, skipping unknown keys.
    /// Mirrors `get_rules`.
    pub fn get_rules(
        &self,
        project_id: &str,
        keys: &[String],
    ) -> Result<Vec<Value>, RepositoryError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = keys.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT * FROM rules WHERE project_id = ? AND rule_key IN ({placeholders})"
        );
        let mut params: Vec<&str> = vec![project_id];
        for k in keys {
            params.push(k.as_str());
        }
        let mut by_key: Map<String, Value> = Map::new();
        let records = self.conn.prepare(&sql)?
            .query_map(rusqlite::params_from_iter(params), Self::rule_dict)?
            .collect::<Result<Vec<_>, _>>()?;
        for record in records {
            if let Some(key) = record.get("key").and_then(|v| v.as_str()) {
                by_key.insert(key.to_string(), record);
            }
        }
        Ok(keys
            .iter()
            .filter_map(|k| by_key.get(k).cloned())
            .collect())
    }

    /// Substring search across rule columns (literal LIKE, NOCASE).
    /// Mirrors `search_rules`.
    pub fn search_rules(
        &self,
        project_id: &str,
        query: &str,
    ) -> Result<Vec<Value>, RepositoryError> {
        let pattern = literal_pattern(query);
        self.conn
            .prepare(
                "SELECT * FROM rules
                 WHERE project_id = ? AND (
                     rule_key LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                     summary LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                     content LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                     tags_json LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                     paths_json LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                     languages_json LIKE ? ESCAPE '\\' COLLATE NOCASE
                 ) ORDER BY ordinal",
            )?
            .query_map(
                rusqlite::params![project_id, pattern, pattern, pattern, pattern, pattern, pattern],
                Self::rule_dict,
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// `PRAGMA integrity_check` as a list of strings.
    pub fn integrity_check(&self) -> Result<Vec<String>, RepositoryError> {
        self.integrity_check_with(&self.conn)
    }

    fn integrity_check_with(&self, conn: &Connection) -> Result<Vec<String>, RepositoryError> {
        self::integrity_check_conn(conn).map_err(Into::into)
    }

    /// Run `f` inside `BEGIN IMMEDIATE` / `COMMIT` (+`ROLLBACK` on error),
    /// re-raising any sqlite error as `RepositoryError::Sqlite` (the raw-escape
    /// path used by the override writers).
    fn run_write_tx(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<()>,
    ) -> Result<(), RepositoryError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(&self.conn) {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                if !self.conn.is_autocommit() {
                    let _ = self.conn.execute_batch("ROLLBACK");
                }
                Err(RepositoryError::Sqlite(e))
            }
        }
    }
}

// ---- module-level helpers ----

/// `PRAGMA integrity_check` → list of strings, for an arbitrary connection.
fn integrity_check_conn(conn: &Connection) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Build a generic row→object value (mirrors Python `dict(row)`).
fn row_to_object(row: &Row<'_>) -> Result<Value, rusqlite::Error> {
    let count = row.as_ref().column_count();
    let mut m = Map::new();
    for i in 0..count {
        let name = row
            .as_ref()
            .column_name(i)
            .map(|s| s.to_string())
            .unwrap_or_else(|_| format!("col{i}"));
        let val = match row.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => Value::Number(n.into()),
            ValueRef::Real(f) => serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            ValueRef::Text(bytes) => {
                Value::String(String::from_utf8_lossy(bytes).into_owned())
            }
            ValueRef::Blob(_) => Value::Null,
        };
        m.insert(name, val);
    }
    Ok(Value::Object(m))
}

/// Transform a raw row into the per-view record shape, mirroring the
/// `query_view` per-view branching.
fn record_for_view(row: &Row<'_>, view: &str) -> Result<Value, rusqlite::Error> {
    match view {
        "rules" => {
            let mut record = json!({
                "project_id": row.get::<_, String>("project_id")?,
            });
            let rule = Repository::rule_dict(row)?;
            if let (Value::Object(m1), Value::Object(m2)) = (&mut record, rule) {
                for (k, v) in m2 {
                    m1.insert(k, v);
                }
            }
            Ok(record)
        }
        "overrides" => Repository::override_dict(row),
        "sync_history" => {
            let mut record = row_to_object(row)?;
            if let Some(obj) = record.as_object_mut() {
                if let Some(warnings_json) = obj.remove("warnings_json") {
                    let warnings: Value = warnings_json
                        .as_str()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(Value::Null);
                    obj.insert("warnings".into(), warnings);
                }
            }
            Ok(record)
        }
        "audit_log" => {
            let mut record = row_to_object(row)?;
            if let Some(obj) = record.as_object_mut() {
                if let Some(change_json) = obj.remove("change_json") {
                    let change: Value = change_json
                        .as_str()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(Value::Null);
                    obj.insert("change".into(), change);
                }
            }
            Ok(record)
        }
        _ => row_to_object(row),
    }
}

/// Decode a stored JSON array string into a `Value` array, or null on absence.
fn json_array_or_null(raw: Option<&str>) -> Value {
    match raw {
        Some(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

/// `opt_str_field(fields, name)` → the field if present and string, else None.
fn opt_str_field(fields: &Map<String, Value>, name: &str) -> Option<String> {
    fields.get(name).and_then(|v| v.as_str()).map(String::from)
}

/// Escape `query` for a literal `LIKE ... ESCAPE '\\'` and wrap in `%…%`.
/// Mirrors `_literal_pattern`.
fn literal_pattern(query: &str) -> String {
    let escaped: String = query
        .chars()
        .flat_map(|c| match c {
            '\\' => vec!['\\', '\\'],
            '%' => vec!['\\', '%'],
            '_' => vec!['\\', '_'],
            _ => vec![c],
        })
        .collect();
    format!("%{escaped}%")
}

/// `True` if a sqlite error corresponds to a "locked" condition. Mirrors
/// Python's `"locked" in str(error).lower()` (with a code fallback for the
/// rare case rusqlite supplies no message).
fn is_locked_error(error: &rusqlite::Error) -> bool {
    if let rusqlite::Error::SqliteFailure(err, msg) = error {
        if let Some(msg) = msg {
            if msg.to_lowercase().contains("locked") {
                return true;
            }
        }
        // SQLITE_BUSY / SQLITE_LOCKED primary codes as a fallback.
        return matches!(
            err.code,
            rusqlite::ffi::ErrorCode::DatabaseBusy | rusqlite::ffi::ErrorCode::DatabaseLocked
        );
    }
    false
}

/// Build a `file:` URI with `mode`-less path, mirroring Python's
/// `Path.resolve().as_uri()` (POSIX: `file://` + percent-encoded absolute path).
fn file_uri(abs: &Path) -> String {
    let s = abs.to_string_lossy();
    let mut out = String::from("file://");
    for &b in s.as_bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_pattern_escapes() {
        assert_eq!(literal_pattern("a%b_c"), r#"%a\%b\_c%"#);
        assert_eq!(literal_pattern(r#"a\b"#), r#"%a\\b%"#);
        assert_eq!(literal_pattern("plain"), "%plain%");
    }

    #[test]
    fn file_uri_encodes() {
        assert_eq!(file_uri(Path::new("/a/b.db")), "file:///a/b.db");
        assert_eq!(
            file_uri(Path::new("/tmp/a b/c.db")),
            "file:///tmp/a%20b/c.db"
        );
    }

    #[test]
    fn migrate_then_schema_version_in_memory() {
        let repo = Repository::in_memory().unwrap();
        repo.migrate().unwrap();
        assert_eq!(repo.schema_version().unwrap(), 2);
        // re-running is a no-op.
        repo.migrate().unwrap();
        assert_eq!(repo.schema_version().unwrap(), 2);
    }

    #[test]
    fn round_trip_replace_and_read() {
        let repo = Repository::in_memory().unwrap();
        repo.migrate().unwrap();
        let rule = crate::models::Rule {
            key: "SEC-001".into(),
            summary: "s".into(),
            content: "c".into(),
            tags: vec!["security".into()],
            paths: vec!["src/**".into()],
            languages: vec!["python".into()],
            source_rule_hash: "sha256:src".into(),
        };
        let checklist = crate::models::Checklist {
            schema_version: 1,
            checklist_version: "2026.07.1".into(),
            global_description: "gd".into(),
            content_hash: "sha256:abc".into(),
            rules: vec![rule],
        };
        let description = crate::description::build_description("p1", "proj", &checklist);
        repo.replace_project(
            "p1",
            "proj",
            "/tmp/cl.md",
            &checklist,
            &description,
            "refresh",
            &[],
        )
        .unwrap();

        let project = repo.get_project("p1").unwrap().unwrap();
        assert_eq!(project.get("project_name").and_then(|v| v.as_str()), Some("proj"));
        assert_eq!(project.get("rule_count").and_then(|v| v.as_i64()), Some(1));

        let rules = repo.list_rules("p1").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].get("key").and_then(|v| v.as_str()), Some("SEC-001"));
        assert_eq!(
            rules[0].get("tags").unwrap(),
            &Value::Array(vec![Value::String("security".into())])
        );

        let got = repo.get_rules("p1", &["SEC-001".into()]).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn search_rules_matches() {
        let repo = Repository::in_memory().unwrap();
        repo.migrate().unwrap();
        let rule = crate::models::Rule {
            key: "DB-001".into(),
            summary: "Never SELECT *".into(),
            content: "explain".into(),
            tags: vec![],
            paths: vec![],
            languages: vec![],
            source_rule_hash: "sha256:x".into(),
        };
        let cl = crate::models::Checklist {
            schema_version: 1,
            checklist_version: "v".into(),
            global_description: "".into(),
            content_hash: "sha256:h".into(),
            rules: vec![rule],
        };
        let desc = crate::description::build_description("p", "proj", &cl);
        repo.replace_project("p", "proj", "/c.md", &cl, &desc, "refresh", &[])
            .unwrap();
        let hits = repo.search_rules("p", "select").unwrap();
        assert_eq!(hits.len(), 1);
        let none = repo.search_rules("p", "zzznope").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn backup_and_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("knowledge.db");
        let repo = Repository::open(&db).unwrap();
        repo.migrate().unwrap();
        let rule = crate::models::Rule {
            key: "K".into(),
            summary: "s".into(),
            content: "c".into(),
            tags: vec![],
            paths: vec![],
            languages: vec![],
            source_rule_hash: "sha256:k".into(),
        };
        let cl = crate::models::Checklist {
            schema_version: 1,
            checklist_version: "v".into(),
            global_description: "".into(),
            content_hash: "sha256:h".into(),
            rules: vec![rule],
        };
        let desc = crate::description::build_description("p", "proj", &cl);
        repo.replace_project("p", "proj", "/c.md", &cl, &desc, "refresh", &[])
            .unwrap();

        let backup_path = dir.path().join("backup.db");
        let info = repo.backup(&backup_path).unwrap();
        assert_eq!(info.get("schema_version").and_then(|v| v.as_i64()), Some(2));

        // restore into a fresh destination, then read it back.
        let restored = dir.path().join("restored.db");
        let restore_info = Repository::restore(&backup_path, &restored).unwrap();
        assert_eq!(restore_info.get("schema_version").and_then(|v| v.as_i64()), Some(2));
        let r2 = Repository::open(&restored).unwrap();
        r2.migrate().unwrap();
        let rules = r2.list_rules("p").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].get("key").and_then(|v| v.as_str()), Some("K"));
    }
}
