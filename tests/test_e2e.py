import json
import subprocess
import sys
from pathlib import Path


def run_cli(*args: str, input_json: dict | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "review_kb.cli", *args],
        input=json.dumps(input_json) if input_json is not None else None,
        text=True,
        capture_output=True,
        check=False,
    )


def test_agent_selection_recovery_and_revision_restart(tmp_path: Path) -> None:
    database = tmp_path / "knowledge.db"
    checklist = tmp_path / "review-checklist.md"
    checklist.write_text(Path("examples/review-checklist.md").read_text())

    prepared = run_cli(
        "--db",
        str(database),
        "prepare",
        "--project-id",
        "codehub-123",
        "--project-name",
        "payments",
        "--checklist",
        str(checklist),
    )
    assert prepared.returncode == 0, prepared.stderr
    context = json.loads(prepared.stdout)["data"]["selection_context"]

    typo = run_cli(
        "--db",
        str(database),
        "rules",
        "get",
        "--input",
        "-",
        input_json={**context, "keys": ["SEC-01"]},
    )
    assert typo.returncode == 3
    assert json.loads(typo.stdout)["error"]["details"]["suggestions"]["SEC-01"][0] == "SEC-001"

    corrected = run_cli(
        "--db",
        str(database),
        "rules",
        "get",
        "--input",
        "-",
        input_json={**context, "keys": ["SEC-001"]},
    )
    assert corrected.returncode == 0
    assert json.loads(corrected.stdout)["data"]["rules"][0]["key"] == "SEC-001"

    checklist.write_text(checklist.read_text().replace("参数化处理", "安全参数绑定"))
    refreshed = run_cli(
        "--db",
        str(database),
        "prepare",
        "--project-id",
        "codehub-123",
        "--project-name",
        "payments",
        "--checklist",
        str(checklist),
    )
    assert refreshed.returncode == 0

    stale = run_cli(
        "--db",
        str(database),
        "rules",
        "get",
        "--input",
        "-",
        input_json={**context, "keys": ["SEC-001"]},
    )
    assert stale.returncode == 4
    assert json.loads(stale.stdout)["error"]["code"] == "KNOWLEDGE_REVISION_MISMATCH"


def test_direct_integration_override_and_backup_flow(tmp_path: Path) -> None:
    database = tmp_path / "knowledge.db"
    backup = tmp_path / "knowledge.backup.db"
    checklist = tmp_path / "review-checklist.md"
    checklist.write_text(Path("examples/review-checklist.md").read_text())
    common = (
        "--db",
        str(database),
    )

    prepared = run_cli(
        *common,
        "prepare",
        "--project-id",
        "codehub-123",
        "--project-name",
        "payments",
        "--checklist",
        str(checklist),
    )
    old_context = json.loads(prepared.stdout)["data"]["selection_context"]

    overridden = run_cli(
        *common,
        "overrides",
        "set",
        "--input",
        "-",
        input_json={
            "project_id": "codehub-123",
            "key": "SEC-001",
            "reason": "应急规则",
            "content": "必须验证所有数据库输入。",
        },
    )
    assert overridden.returncode == 0, overridden.stderr
    new_revision = json.loads(overridden.stdout)["data"]["knowledge_revision"]

    stale = run_cli(
        *common,
        "rules",
        "get",
        "--input",
        "-",
        input_json={**old_context, "keys": ["SEC-001"]},
    )
    assert stale.returncode == 4

    fetched = run_cli(
        *common,
        "rules",
        "get",
        "--input",
        "-",
        input_json={
            "project_id": "codehub-123",
            "knowledge_revision": new_revision,
            "keys": ["SEC-001"],
        },
    )
    assert fetched.returncode == 0, fetched.stderr
    assert json.loads(fetched.stdout)["data"]["rules"][0]["content"] == "必须验证所有数据库输入。"

    backed_up = run_cli(*common, "db", "backup", "--output", str(backup))
    checked = run_cli(*common, "db", "check")
    assert backed_up.returncode == 0, backed_up.stderr
    assert backup.exists()
    assert json.loads(checked.stdout)["data"]["integrity"] == ["ok"]
