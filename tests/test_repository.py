from pathlib import Path
import sqlite3

import pytest

from review_kb.checklist import parse_checklist
from review_kb.description import build_description
from review_kb.errors import ErrorCode, ReviewKBError
from review_kb.repository import Repository


@pytest.fixture
def repository(tmp_path: Path) -> Repository:
    repository = Repository.open(tmp_path / "knowledge.db")
    repository.migrate()
    yield repository
    repository.close()


@pytest.fixture
def parsed_checklist():
    return parse_checklist("tests/fixtures/valid-checklist.md")


def store(repository: Repository, project_id: str, name: str, checklist) -> None:
    repository.replace_project(
        project_id,
        name,
        f"/{name}/review-checklist.md",
        checklist,
        build_description(project_id, name, checklist),
    )


def test_replace_project_is_atomic_and_preserves_requested_rule_order(
    repository: Repository,
    parsed_checklist,
) -> None:
    store(repository, "123", "payments", parsed_checklist)

    project = repository.get_project("123")
    rules = repository.get_rules("123", ["DB-004", "SEC-001"])

    assert project is not None
    assert project["rule_count"] == 2
    assert [rule["key"] for rule in rules] == ["DB-004", "SEC-001"]


def test_same_rule_key_can_exist_in_different_projects(
    repository: Repository,
    parsed_checklist,
) -> None:
    store(repository, "123", "one", parsed_checklist)
    store(repository, "456", "two", parsed_checklist)

    assert repository.get_rules("123", ["SEC-001"])
    assert repository.get_rules("456", ["SEC-001"])


def test_literal_search_matches_summary_and_content(
    repository: Repository,
    parsed_checklist,
) -> None:
    store(repository, "123", "payments", parsed_checklist)

    summary_matches = repository.search_rules("123", "事务边界")
    content_matches = repository.search_rules("123", "字符串拼接")

    assert [rule["key"] for rule in summary_matches] == ["DB-004"]
    assert [rule["key"] for rule in content_matches] == ["SEC-001"]


def test_integrity_check_reports_ok(repository: Repository) -> None:
    assert repository.integrity_check() == ["ok"]


def test_schema_version_and_allowlisted_read_only_query(
    repository: Repository,
    parsed_checklist,
) -> None:
    store(repository, "123", "payments", parsed_checklist)

    rows = repository.query_view(
        "rules", project_id="123", query="SEC-001", limit=10
    )

    assert repository.schema_version() == 2
    assert [row["key"] for row in rows] == ["SEC-001"]
    with pytest.raises(ReviewKBError) as caught:
        repository.query_view("sqlite_master")
    assert caught.value.code is ErrorCode.INVALID_ARGUMENT


def test_backup_and_restore_round_trip(
    repository: Repository,
    parsed_checklist,
    tmp_path: Path,
) -> None:
    store(repository, "123", "payments", parsed_checklist)
    backup = tmp_path / "backup ? copy.db"

    result = repository.backup(backup)
    restored_path = tmp_path / "restored.db"
    restore_result = Repository.restore(backup, restored_path)

    assert result["schema_version"] == 2
    assert restore_result["schema_version"] == 2
    restored = Repository.open(restored_path)
    try:
        restored.migrate()
        assert restored.get_project("123") is not None
        assert restored.integrity_check() == ["ok"]
    finally:
        restored.close()


def test_migrate_rejects_database_from_newer_cli(tmp_path: Path) -> None:
    database = tmp_path / "future.db"
    connection = sqlite3.connect(database)
    connection.execute(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)"
    )
    connection.execute(
        "INSERT INTO schema_migrations(version, applied_at) VALUES (999, 'future')"
    )
    connection.commit()
    connection.close()
    repository = Repository.open(database)
    try:
        with pytest.raises(ReviewKBError) as caught:
            repository.migrate()
    finally:
        repository.close()

    assert caught.value.code is ErrorCode.DB_SCHEMA_UNSUPPORTED
