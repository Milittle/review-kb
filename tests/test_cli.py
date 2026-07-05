import json
from pathlib import Path

from typer.testing import CliRunner

from review_kb.cli import app


runner = CliRunner()


def test_prepare_and_rules_get_emit_json_documents(tmp_path: Path) -> None:
    db_path = tmp_path / "knowledge.db"
    prepared = runner.invoke(
        app,
        [
            "--db",
            str(db_path),
            "prepare",
            "--project-id",
            "123",
            "--project-name",
            "payments",
            "--checklist",
            "tests/fixtures/valid-checklist.md",
        ],
    )

    assert prepared.exit_code == 0, prepared.output
    prepared_payload = json.loads(prepared.stdout)
    selection = {
        **prepared_payload["data"]["selection_context"],
        "keys": ["SEC-001"],
    }
    fetched = runner.invoke(
        app,
        ["--db", str(db_path), "rules", "get", "--input", "-"],
        input=json.dumps(selection),
    )

    assert fetched.exit_code == 0, fetched.output
    fetched_payload = json.loads(fetched.stdout)
    assert fetched_payload["data"]["rules"][0]["key"] == "SEC-001"
    assert len(fetched.stdout.strip().splitlines()) == 1


def test_bad_selection_has_json_error_and_exit_two(tmp_path: Path) -> None:
    result = runner.invoke(
        app,
        ["--db", str(tmp_path / "knowledge.db"), "rules", "get", "--input", "-"],
        input="{}",
    )

    assert result.exit_code == 2
    assert json.loads(result.stdout)["error"]["code"] == "INVALID_SELECTION"


def test_environment_database_path_is_reported(monkeypatch, tmp_path: Path) -> None:
    path = tmp_path / "from-env.db"
    monkeypatch.setenv("REVIEW_KB_DB", str(path))

    result = runner.invoke(app, ["config", "show"])

    assert result.exit_code == 0
    payload = json.loads(result.stdout)
    assert payload["data"] == {"db_path": str(path), "source": "environment"}


def test_config_set_then_get_database_path(monkeypatch, tmp_path: Path) -> None:
    config_path = tmp_path / "review-kb" / "config.toml"
    database_path = tmp_path / "data" / "knowledge.db"
    monkeypatch.delenv("REVIEW_KB_DB", raising=False)
    monkeypatch.setenv("REVIEW_KB_CONFIG", str(config_path))

    set_result = runner.invoke(
        app,
        ["config", "set", "db_path", str(database_path)],
    )
    get_result = runner.invoke(app, ["config", "get", "db_path"])

    assert set_result.exit_code == 0, set_result.output
    assert config_path.exists()
    assert get_result.exit_code == 0, get_result.output
    assert json.loads(get_result.stdout)["data"] == {
        "key": "db_path",
        "value": str(database_path),
        "source": "config_file",
    }


def test_rule_not_found_is_machine_recoverable(tmp_path: Path) -> None:
    db_path = tmp_path / "knowledge.db"
    prepared = runner.invoke(
        app,
        [
            "--db",
            str(db_path),
            "prepare",
            "--project-id",
            "123",
            "--project-name",
            "payments",
            "--checklist",
            "tests/fixtures/valid-checklist.md",
        ],
    )
    context = json.loads(prepared.stdout)["data"]["selection_context"]

    result = runner.invoke(
        app,
        ["--db", str(db_path), "rules", "get", "--input", "-"],
        input=json.dumps({**context, "keys": ["SEC-01"]}),
    )

    assert result.exit_code == 3
    error = json.loads(result.stdout)["error"]
    assert error["code"] == "RULE_NOT_FOUND"
    assert error["details"]["suggestions"]["SEC-01"][0] == "SEC-001"


def test_status_does_not_create_a_missing_database(tmp_path: Path) -> None:
    db_path = tmp_path / "must-not-be-created.db"

    result = runner.invoke(
        app,
        [
            "--db",
            str(db_path),
            "status",
            "--project-id",
            "123",
            "--checklist",
            "tests/fixtures/valid-checklist.md",
        ],
    )

    assert result.exit_code == 0
    assert json.loads(result.stdout)["data"]["status"] == "missing"
    assert not db_path.exists()


def test_override_commands_update_effective_rule_and_revision(tmp_path: Path) -> None:
    db_path = tmp_path / "knowledge.db"
    prepared = runner.invoke(
        app,
        [
            "--db",
            str(db_path),
            "prepare",
            "--project-id",
            "123",
            "--project-name",
            "payments",
            "--checklist",
            "tests/fixtures/valid-checklist.md",
        ],
    )
    old_revision = json.loads(prepared.stdout)["data"]["selection_context"][
        "knowledge_revision"
    ]
    overridden = runner.invoke(
        app,
        ["--db", str(db_path), "overrides", "set", "--input", "-"],
        input=json.dumps(
            {
                "project_id": "123",
                "key": "SEC-001",
                "reason": "应急修复",
                "content": "临时规则正文",
            }
        ),
    )

    assert overridden.exit_code == 0, overridden.output
    new_revision = json.loads(overridden.stdout)["data"]["knowledge_revision"]
    assert new_revision != old_revision
    listed = runner.invoke(
        app,
        ["--db", str(db_path), "overrides", "list", "--project-id", "123"],
    )
    assert json.loads(listed.stdout)["data"]["overrides"][0]["status"] == "active"

    fetched = runner.invoke(
        app,
        ["--db", str(db_path), "rules", "get", "--input", "-"],
        input=json.dumps(
            {
                "project_id": "123",
                "knowledge_revision": new_revision,
                "keys": ["SEC-001"],
            }
        ),
    )
    assert json.loads(fetched.stdout)["data"]["rules"][0]["content"] == "临时规则正文"

    unset = runner.invoke(
        app,
        [
            "--db",
            str(db_path),
            "overrides",
            "unset",
            "--project-id",
            "123",
            "--key",
            "SEC-001",
            "--reason",
            "应急结束",
        ],
    )
    assert unset.exit_code == 0
    assert json.loads(unset.stdout)["data"]["status"] == "disabled"


def test_sync_and_rebuild_commands_emit_json(tmp_path: Path) -> None:
    db_path = tmp_path / "knowledge.db"
    common = [
        "--project-id",
        "123",
        "--project-name",
        "payments",
        "--checklist",
        "tests/fixtures/valid-checklist.md",
    ]

    synced = runner.invoke(app, ["--db", str(db_path), "sync", *common])
    rebuilt = runner.invoke(app, ["--db", str(db_path), "rebuild", *common])

    assert synced.exit_code == 0, synced.output
    assert json.loads(synced.stdout)["data"]["knowledge_status"] == "created"
    assert rebuilt.exit_code == 0, rebuilt.output
    assert json.loads(rebuilt.stdout)["data"]["knowledge_status"] == "rebuilt"


def test_override_resolve_command_accepts_changed_source(tmp_path: Path) -> None:
    db_path = tmp_path / "knowledge.db"
    checklist = tmp_path / "review-checklist.md"
    checklist.write_text(Path("tests/fixtures/valid-checklist.md").read_text())
    common = [
        "--project-id",
        "123",
        "--project-name",
        "payments",
        "--checklist",
        str(checklist),
    ]
    runner.invoke(app, ["--db", str(db_path), "prepare", *common])
    runner.invoke(
        app,
        ["--db", str(db_path), "overrides", "set", "--input", "-"],
        input=json.dumps(
            {
                "project_id": "123",
                "key": "SEC-001",
                "reason": "应急",
                "content": "临时规则",
            }
        ),
    )
    checklist.write_text(checklist.read_text().replace("参数化处理", "安全参数绑定"))
    conflicted = runner.invoke(app, ["--db", str(db_path), "prepare", *common])
    assert conflicted.exit_code == 4

    resolved = runner.invoke(
        app,
        [
            "--db",
            str(db_path),
            "overrides",
            "resolve",
            "--project-id",
            "123",
            "--key",
            "SEC-001",
            "--strategy",
            "accept-source",
            "--checklist",
            str(checklist),
            "--reason",
            "接受源规则",
        ],
    )

    assert resolved.exit_code == 0, resolved.output
    assert json.loads(resolved.stdout)["data"]["override_resolution"]["strategy"] == "accept-source"


def test_database_query_backup_restore_and_migrate_commands(tmp_path: Path) -> None:
    db_path = tmp_path / "knowledge.db"
    backup_path = tmp_path / "knowledge.backup.db"
    prepared = runner.invoke(
        app,
        [
            "--db",
            str(db_path),
            "prepare",
            "--project-id",
            "123",
            "--project-name",
            "payments",
            "--checklist",
            "tests/fixtures/valid-checklist.md",
        ],
    )
    assert prepared.exit_code == 0, prepared.output

    queried = runner.invoke(
        app,
        [
            "--db",
            str(db_path),
            "db",
            "query",
            "--view",
            "rules",
            "--project-id",
            "123",
            "--query",
            "SEC-001",
        ],
    )
    backed_up = runner.invoke(
        app,
        ["--db", str(db_path), "db", "backup", "--output", str(backup_path)],
    )
    restored = runner.invoke(
        app,
        ["--db", str(db_path), "db", "restore", "--input", str(backup_path)],
    )
    migrated = runner.invoke(app, ["--db", str(db_path), "db", "migrate"])

    assert queried.exit_code == 0, queried.output
    assert json.loads(queried.stdout)["data"]["rows"][0]["key"] == "SEC-001"
    assert backed_up.exit_code == 0, backed_up.output
    assert backup_path.exists()
    assert restored.exit_code == 0, restored.output
    assert Path(json.loads(restored.stdout)["data"]["safety_backup"]).exists()
    assert migrated.exit_code == 0, migrated.output
    assert json.loads(migrated.stdout)["data"]["schema_version"] == 2

    info = runner.invoke(app, ["--db", str(db_path), "db", "info"])
    assert json.loads(info.stdout)["data"]["schema_version"] == 2
