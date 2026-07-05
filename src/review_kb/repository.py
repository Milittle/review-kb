from __future__ import annotations

import json
import os
import sqlite3
import tempfile
from contextlib import contextmanager
from datetime import datetime, timezone
from importlib import resources
from pathlib import Path
from typing import Any, Iterable, Iterator

from .description import canonical_json
from .errors import ErrorCode, ReviewKBError
from .models import Checklist


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _literal_pattern(query: str) -> str:
    escaped = query.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
    return f"%{escaped}%"


class Repository:
    _LATEST_SCHEMA_VERSION = 2
    _QUERY_VIEWS = {
        "projects": "project_id",
        "rules": "project_id, ordinal",
        "overrides": "project_id, rule_key",
        "sync_history": "sync_id",
        "audit_log": "audit_id",
    }

    def __init__(self, connection: sqlite3.Connection, path: Path) -> None:
        self._connection = connection
        self.path = path

    @classmethod
    def open(cls, path: str | Path) -> "Repository":
        db_path = Path(path).expanduser()
        db_path.parent.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(db_path, timeout=5, isolation_level=None)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA foreign_keys=ON")
        connection.execute("PRAGMA busy_timeout=5000")
        connection.execute("PRAGMA journal_mode=WAL")
        return cls(connection, db_path)

    @classmethod
    def in_memory(cls) -> "Repository":
        connection = sqlite3.connect(":memory:", timeout=5, isolation_level=None)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA foreign_keys=ON")
        connection.execute("PRAGMA busy_timeout=5000")
        return cls(connection, Path(":memory:"))

    def close(self) -> None:
        self._connection.close()

    @contextmanager
    def read_transaction(self) -> Iterator[None]:
        already_in_transaction = self._connection.in_transaction
        if not already_in_transaction:
            self._connection.execute("BEGIN")
        try:
            yield
            if not already_in_transaction:
                self._connection.execute("COMMIT")
        except Exception:
            if not already_in_transaction and self._connection.in_transaction:
                self._connection.execute("ROLLBACK")
            raise

    def migrate(self) -> None:
        self._connection.execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)"
        )
        applied = {
            row[0] for row in self._connection.execute("SELECT version FROM schema_migrations")
        }
        newest = max(applied, default=0)
        if newest > self._LATEST_SCHEMA_VERSION:
            raise ReviewKBError(
                ErrorCode.DB_SCHEMA_UNSUPPORTED,
                "database schema is newer than this CLI supports",
                {
                    "schema_version": newest,
                    "supported": self._LATEST_SCHEMA_VERSION,
                    "path": str(self.path),
                },
            )
        migration_root = resources.files("review_kb.migrations")
        migrations = [(1, "001_initial.sql"), (2, "002_overrides.sql")]
        for version, filename in migrations:
            if version in applied:
                continue
            sql = migration_root.joinpath(filename).read_text(encoding="utf-8")
            try:
                self._connection.executescript(sql)
                self._connection.execute(
                    "INSERT INTO schema_migrations(version, applied_at) VALUES (?, ?)",
                    (version, _now()),
                )
            except sqlite3.DatabaseError as error:
                raise ReviewKBError(
                    ErrorCode.DB_INTEGRITY_ERROR,
                    f"database migration failed: {error}",
                    {"path": str(self.path)},
                ) from error

    def schema_version(self) -> int:
        row = self._connection.execute(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations"
        ).fetchone()
        return int(row[0])

    def query_view(
        self,
        view: str,
        *,
        project_id: str | None = None,
        query: str | None = None,
        limit: int = 100,
    ) -> list[dict[str, Any]]:
        if view not in self._QUERY_VIEWS:
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                f"unsupported database view: {view}",
                {"allowed_views": sorted(self._QUERY_VIEWS)},
            )
        if not 1 <= limit <= 1000:
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "query limit must be between 1 and 1000",
                {"limit": limit},
            )
        sql = f"SELECT * FROM {view}"
        parameters: list[Any] = []
        if project_id is not None:
            sql += " WHERE project_id = ?"
            parameters.append(project_id)
        sql += f" ORDER BY {self._QUERY_VIEWS[view]}"
        rows = self._connection.execute(sql, parameters)
        records: list[dict[str, Any]] = []
        needle = query.casefold() if query else None
        for row in rows:
            if view == "rules":
                record = {"project_id": row["project_id"], **self._rule_dict(row)}
            elif view == "overrides":
                record = self._override_dict(row)
            else:
                record = dict(row)
                if view == "sync_history":
                    record["warnings"] = json.loads(record.pop("warnings_json"))
                elif view == "audit_log":
                    record["change"] = json.loads(record.pop("change_json"))
            if needle and needle not in canonical_json(record).casefold():
                continue
            records.append(record)
            if len(records) == limit:
                break
        return records

    def backup(self, output: str | Path) -> dict[str, Any]:
        target = Path(output).expanduser()
        if target.exists():
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                f"backup output already exists: {target}",
                {"path": str(target)},
            )
        if self.path != Path(":memory:") and target.resolve() == self.path.resolve():
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "backup output must differ from the active database",
                {"path": str(target)},
            )
        target.parent.mkdir(parents=True, exist_ok=True)
        destination: sqlite3.Connection | None = None
        try:
            destination = sqlite3.connect(target)
            self._connection.backup(destination)
            integrity = [row[0] for row in destination.execute("PRAGMA integrity_check")]
            if integrity != ["ok"]:
                raise ReviewKBError(
                    ErrorCode.BACKUP_INVALID,
                    "created backup failed integrity check",
                    {"path": str(target), "integrity": integrity},
                )
            return {
                "path": str(target),
                "schema_version": self.schema_version(),
            }
        except ReviewKBError:
            target.unlink(missing_ok=True)
            raise
        except sqlite3.DatabaseError as error:
            target.unlink(missing_ok=True)
            raise ReviewKBError(
                ErrorCode.BACKUP_INVALID,
                f"database backup failed: {error}",
                {"path": str(target)},
            ) from error
        finally:
            if destination is not None:
                destination.close()

    @classmethod
    def restore(cls, source: str | Path, destination: str | Path) -> dict[str, Any]:
        source_path = Path(source).expanduser()
        destination_path = Path(destination).expanduser()
        if source_path.resolve() == destination_path.resolve():
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "backup source and restore destination must differ",
                {"path": str(source_path)},
            )
        if not source_path.is_file():
            raise ReviewKBError(
                ErrorCode.BACKUP_INVALID,
                f"backup file not found: {source_path}",
                {"path": str(source_path)},
            )
        destination_path.parent.mkdir(parents=True, exist_ok=True)
        source_connection: sqlite3.Connection | None = None
        temporary: Path | None = None
        safety_backup: Path | None = None
        try:
            source_connection = sqlite3.connect(
                f"{source_path.resolve().as_uri()}?mode=ro", uri=True
            )
            integrity = [row[0] for row in source_connection.execute("PRAGMA integrity_check")]
            if integrity != ["ok"]:
                raise ReviewKBError(
                    ErrorCode.BACKUP_INVALID,
                    "backup failed integrity check",
                    {"path": str(source_path), "integrity": integrity},
                )
            try:
                row = source_connection.execute(
                    "SELECT COALESCE(MAX(version), 0) FROM schema_migrations"
                ).fetchone()
            except sqlite3.DatabaseError as error:
                raise ReviewKBError(
                    ErrorCode.BACKUP_INVALID,
                    "backup does not contain a review-kb schema",
                    {"path": str(source_path)},
                ) from error
            version = int(row[0])
            if version > cls._LATEST_SCHEMA_VERSION:
                raise ReviewKBError(
                    ErrorCode.DB_SCHEMA_UNSUPPORTED,
                    "backup schema is newer than this CLI supports",
                    {"schema_version": version, "supported": cls._LATEST_SCHEMA_VERSION},
                )
            if destination_path.exists():
                stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
                safety_backup = destination_path.with_name(
                    f"{destination_path.name}.safety-{stamp}.db"
                )
                existing = cls.open(destination_path)
                try:
                    existing.backup(safety_backup)
                finally:
                    existing.close()
            with tempfile.NamedTemporaryFile(
                dir=destination_path.parent,
                prefix=f".{destination_path.name}.restore-",
                delete=False,
            ) as handle:
                temporary = Path(handle.name)
            target_connection = sqlite3.connect(temporary)
            try:
                source_connection.backup(target_connection)
            finally:
                target_connection.close()
            for suffix in ("-wal", "-shm"):
                Path(f"{destination_path}{suffix}").unlink(missing_ok=True)
            os.replace(temporary, destination_path)
            temporary = None
            return {
                "path": str(destination_path),
                "source": str(source_path),
                "schema_version": version,
                "safety_backup": str(safety_backup) if safety_backup else None,
            }
        except ReviewKBError:
            raise
        except sqlite3.DatabaseError as error:
            raise ReviewKBError(
                ErrorCode.BACKUP_INVALID,
                f"database restore failed: {error}",
                {"path": str(source_path)},
            ) from error
        finally:
            if source_connection is not None:
                source_connection.close()
            if temporary is not None:
                temporary.unlink(missing_ok=True)

    def _audit(
        self,
        project_id: str,
        rule_key: str | None,
        action: str,
        reason: str,
        change: dict[str, Any],
    ) -> None:
        self._connection.execute(
            """
            INSERT INTO audit_log(project_id, rule_key, action, reason, change_json, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            """,
            (project_id, rule_key, action, reason, canonical_json(change), _now()),
        )

    @staticmethod
    def _override_dict(row: sqlite3.Row) -> dict[str, Any]:
        result = dict(row)
        for field in ("tags", "paths", "languages"):
            raw = result.pop(f"{field}_json")
            result[field] = json.loads(raw) if raw is not None else None
        return result

    def get_override(self, project_id: str, rule_key: str) -> dict[str, Any] | None:
        row = self._connection.execute(
            "SELECT * FROM rule_overrides WHERE project_id = ? AND rule_key = ?",
            (project_id, rule_key),
        ).fetchone()
        return self._override_dict(row) if row is not None else None

    def list_overrides(
        self,
        project_id: str,
        statuses: tuple[str, ...] | None = None,
    ) -> list[dict[str, Any]]:
        if statuses:
            placeholders = ",".join("?" for _ in statuses)
            rows = self._connection.execute(
                f"""
                SELECT * FROM rule_overrides
                WHERE project_id = ? AND status IN ({placeholders})
                ORDER BY rule_key
                """,
                (project_id, *statuses),
            )
        else:
            rows = self._connection.execute(
                "SELECT * FROM rule_overrides WHERE project_id = ? ORDER BY rule_key",
                (project_id,),
            )
        return [self._override_dict(row) for row in rows]

    def upsert_override(
        self,
        project_id: str,
        rule_key: str,
        fields: dict[str, Any],
        *,
        reason: str,
    ) -> None:
        source = self._connection.execute(
            "SELECT source_rule_hash FROM rules WHERE project_id = ? AND rule_key = ?",
            (project_id, rule_key),
        ).fetchone()
        if source is None:
            raise ReviewKBError(
                ErrorCode.RULE_NOT_FOUND,
                f"rule not found: {rule_key}",
                {"project_id": project_id, "key": rule_key},
            )
        now = _now()
        arrays = {
            name: canonical_json(fields[name]) if name in fields else None
            for name in ("tags", "paths", "languages")
        }
        try:
            self._connection.execute("BEGIN IMMEDIATE")
            self._connection.execute(
                """
                INSERT INTO rule_overrides(
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
                    status='active', reason=excluded.reason, updated_at=excluded.updated_at
                """,
                (
                    project_id,
                    rule_key,
                    fields.get("summary"),
                    fields.get("content"),
                    arrays["tags"],
                    arrays["paths"],
                    arrays["languages"],
                    source["source_rule_hash"],
                    reason,
                    now,
                    now,
                ),
            )
            self._audit(project_id, rule_key, "override.set", reason, fields)
            self._connection.execute("COMMIT")
        except sqlite3.DatabaseError:
            if self._connection.in_transaction:
                self._connection.execute("ROLLBACK")
            raise

    def disable_override(self, project_id: str, rule_key: str, *, reason: str) -> None:
        if self.get_override(project_id, rule_key) is None:
            raise ReviewKBError(
                ErrorCode.RULE_NOT_FOUND,
                f"override not found: {rule_key}",
                {"project_id": project_id, "key": rule_key},
            )
        try:
            self._connection.execute("BEGIN IMMEDIATE")
            self._connection.execute(
                """
                UPDATE rule_overrides SET status = 'disabled', reason = ?, updated_at = ?
                WHERE project_id = ? AND rule_key = ?
                """,
                (reason, _now(), project_id, rule_key),
            )
            self._audit(project_id, rule_key, "override.unset", reason, {})
            self._connection.execute("COMMIT")
        except sqlite3.DatabaseError:
            if self._connection.in_transaction:
                self._connection.execute("ROLLBACK")
            raise

    def mark_override_conflicts(self, project_id: str, keys: list[str]) -> None:
        if not keys:
            return
        placeholders = ",".join("?" for _ in keys)
        self._connection.execute(
            f"""
            UPDATE rule_overrides SET status = 'conflict', updated_at = ?
            WHERE project_id = ? AND rule_key IN ({placeholders})
            """,
            (_now(), project_id, *keys),
        )

    def resolve_override(
        self,
        project_id: str,
        rule_key: str,
        *,
        keep: bool,
        base_source_rule_hash: str | None,
        reason: str,
    ) -> None:
        override = self.get_override(project_id, rule_key)
        if override is None:
            raise ReviewKBError(
                ErrorCode.RULE_NOT_FOUND,
                f"override not found: {rule_key}",
                {"project_id": project_id, "key": rule_key},
            )
        if override["status"] != "conflict":
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "only a conflicting override can be resolved",
                {"project_id": project_id, "key": rule_key, "status": override["status"]},
            )
        if keep and not base_source_rule_hash:
            raise ReviewKBError(
                ErrorCode.RULE_NOT_FOUND,
                f"source rule not found: {rule_key}",
                {"project_id": project_id, "key": rule_key},
            )
        now = _now()
        action = "override.resolve.keep" if keep else "override.resolve.accept_source"
        try:
            self._connection.execute("BEGIN IMMEDIATE")
            if keep:
                self._connection.execute(
                    """
                    UPDATE rule_overrides
                    SET base_source_rule_hash = ?, status = 'active', reason = ?, updated_at = ?
                    WHERE project_id = ? AND rule_key = ?
                    """,
                    (base_source_rule_hash, reason, now, project_id, rule_key),
                )
            else:
                self._connection.execute(
                    """
                    UPDATE rule_overrides
                    SET status = 'disabled', reason = ?, updated_at = ?
                    WHERE project_id = ? AND rule_key = ?
                    """,
                    (reason, now, project_id, rule_key),
                )
            self._audit(
                project_id,
                rule_key,
                action,
                reason,
                {"base_source_rule_hash": base_source_rule_hash},
            )
            self._connection.execute("COMMIT")
        except sqlite3.DatabaseError:
            if self._connection.in_transaction:
                self._connection.execute("ROLLBACK")
            raise

    def update_effective_description(
        self,
        project_id: str,
        description: dict[str, Any],
    ) -> None:
        self._connection.execute(
            """
            UPDATE projects SET knowledge_revision = ?, description_json = ?, updated_at = ?
            WHERE project_id = ?
            """,
            (
                description["checklist"]["knowledge_revision"],
                canonical_json(description),
                _now(),
                project_id,
            ),
        )

    def list_audit_log(self, project_id: str) -> list[dict[str, Any]]:
        rows = self._connection.execute(
            "SELECT * FROM audit_log WHERE project_id = ? ORDER BY audit_id DESC",
            (project_id,),
        )
        return [dict(row) for row in rows]

    def _project_row(self, project_id: str) -> sqlite3.Row | None:
        return self._connection.execute(
            """
            SELECT p.*, (SELECT COUNT(*) FROM rules r WHERE r.project_id = p.project_id) AS rule_count
            FROM projects p WHERE p.project_id = ?
            """,
            (project_id,),
        ).fetchone()

    def get_project(self, project_id: str) -> dict[str, Any] | None:
        row = self._project_row(project_id)
        return dict(row) if row is not None else None

    def replace_project(
        self,
        project_id: str,
        project_name: str,
        checklist_path: str,
        checklist: Checklist,
        description: dict[str, Any],
        *,
        action: str = "refresh",
        warnings: Iterable[str] = (),
    ) -> None:
        current = self._project_row(project_id)
        now = _now()
        revision = description["checklist"]["knowledge_revision"]
        try:
            self._connection.execute("BEGIN IMMEDIATE")
            self._connection.execute(
                """
                INSERT INTO projects(
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
                    updated_at=excluded.updated_at
                """,
                (
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
                ),
            )
            self._connection.execute("DELETE FROM rules WHERE project_id = ?", (project_id,))
            self._connection.executemany(
                """
                INSERT INTO rules(
                    project_id, rule_key, ordinal, summary, content,
                    tags_json, paths_json, languages_json, source_rule_hash
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                [
                    (
                        project_id,
                        rule.key,
                        ordinal,
                        rule.summary,
                        rule.content,
                        canonical_json(rule.tags),
                        canonical_json(rule.paths),
                        canonical_json(rule.languages),
                        rule.source_rule_hash,
                    )
                    for ordinal, rule in enumerate(checklist.rules)
                ],
            )
            self._connection.execute(
                """
                INSERT INTO sync_history(
                    project_id, action, old_version, new_version, old_hash,
                    new_hash, result, warnings_json, created_at
                ) VALUES (?, ?, ?, ?, ?, ?, 'success', ?, ?)
                """,
                (
                    project_id,
                    action,
                    current["checklist_version"] if current else None,
                    checklist.checklist_version,
                    current["content_hash"] if current else None,
                    checklist.content_hash,
                    canonical_json(list(warnings)),
                    now,
                ),
            )
            self._connection.execute("COMMIT")
        except sqlite3.OperationalError as error:
            if self._connection.in_transaction:
                self._connection.execute("ROLLBACK")
            code = ErrorCode.DB_LOCKED if "locked" in str(error).lower() else ErrorCode.DB_INTEGRITY_ERROR
            raise ReviewKBError(code, f"database write failed: {error}", {"path": str(self.path)}) from error
        except sqlite3.DatabaseError as error:
            if self._connection.in_transaction:
                self._connection.execute("ROLLBACK")
            raise ReviewKBError(
                ErrorCode.DB_INTEGRITY_ERROR,
                f"database write failed: {error}",
                {"path": str(self.path)},
            ) from error

    def update_project_metadata(
        self,
        project_id: str,
        project_name: str,
        checklist_path: str,
        description: dict[str, Any],
    ) -> None:
        self._connection.execute(
            """
            UPDATE projects
            SET project_name = ?, checklist_path = ?, description_json = ?, updated_at = ?
            WHERE project_id = ?
            """,
            (project_name, checklist_path, canonical_json(description), _now(), project_id),
        )

    @staticmethod
    def _rule_dict(row: sqlite3.Row) -> dict[str, Any]:
        return {
            "key": row["rule_key"],
            "summary": row["summary"],
            "content": row["content"],
            "tags": json.loads(row["tags_json"]),
            "paths": json.loads(row["paths_json"]),
            "languages": json.loads(row["languages_json"]),
            "source_rule_hash": row["source_rule_hash"],
            "ordinal": row["ordinal"],
        }

    def list_projects(self) -> list[dict[str, Any]]:
        rows = self._connection.execute(
            """
            SELECT p.*, (SELECT COUNT(*) FROM rules r WHERE r.project_id = p.project_id) AS rule_count
            FROM projects p ORDER BY p.project_id
            """
        )
        return [dict(row) for row in rows]

    def list_rules(self, project_id: str) -> list[dict[str, Any]]:
        rows = self._connection.execute(
            "SELECT * FROM rules WHERE project_id = ? ORDER BY ordinal",
            (project_id,),
        )
        return [self._rule_dict(row) for row in rows]

    def get_rules(self, project_id: str, keys: list[str]) -> list[dict[str, Any]]:
        if not keys:
            return []
        placeholders = ",".join("?" for _ in keys)
        rows = self._connection.execute(
            f"SELECT * FROM rules WHERE project_id = ? AND rule_key IN ({placeholders})",
            [project_id, *keys],
        )
        by_key = {row["rule_key"]: self._rule_dict(row) for row in rows}
        return [by_key[key] for key in keys if key in by_key]

    def search_rules(self, project_id: str, query: str) -> list[dict[str, Any]]:
        pattern = _literal_pattern(query)
        rows = self._connection.execute(
            """
            SELECT * FROM rules
            WHERE project_id = ? AND (
                rule_key LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                summary LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                content LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                tags_json LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                paths_json LIKE ? ESCAPE '\\' COLLATE NOCASE OR
                languages_json LIKE ? ESCAPE '\\' COLLATE NOCASE
            ) ORDER BY ordinal
            """,
            (project_id, *(pattern for _ in range(6))),
        )
        return [self._rule_dict(row) for row in rows]

    def integrity_check(self) -> list[str]:
        return [row[0] for row in self._connection.execute("PRAGMA integrity_check")]
