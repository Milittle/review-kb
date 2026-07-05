from pathlib import Path

import pytest

from review_kb.repository import Repository
from review_kb.service import KnowledgeService


@pytest.fixture
def checklist_path(tmp_path: Path) -> Path:
    path = tmp_path / "review-checklist.md"
    path.write_text(Path("tests/fixtures/valid-checklist.md").read_text())
    return path


@pytest.fixture
def service(tmp_path: Path):
    repository = Repository.open(tmp_path / "knowledge.db")
    repository.migrate()
    yield KnowledgeService(repository)
    repository.close()


def test_prepare_creates_then_reuses(
    service: KnowledgeService,
    checklist_path: Path,
) -> None:
    created = service.prepare("123", "payments", checklist_path)
    reused = service.prepare("123", "payments", checklist_path)

    assert created["knowledge_status"] == "created"
    assert reused["knowledge_status"] == "reused"
    assert created["selection_context"] == reused["selection_context"]
    assert created["rule_count"] == 2


def test_content_change_without_version_bump_refreshes_with_warning(
    service: KnowledgeService,
    checklist_path: Path,
) -> None:
    service.prepare("123", "payments", checklist_path)
    checklist_path.write_text(checklist_path.read_text().replace("参数化", "绑定参数"))

    result = service.prepare("123", "payments", checklist_path)

    assert result["knowledge_status"] == "refreshed"
    assert result["warnings"] == ["CONTENT_CHANGED_WITHOUT_VERSION_BUMP"]


def test_project_rename_updates_metadata_without_changing_revision(
    service: KnowledgeService,
    checklist_path: Path,
) -> None:
    before = service.prepare("123", "old-name", checklist_path)
    after = service.prepare("123", "new-name", checklist_path)

    assert after["knowledge_status"] == "reused"
    assert before["selection_context"] == after["selection_context"]
    assert service.show_project("123")["project_name"] == "new-name"
    assert service.get_description("123")["project"]["name"] == "new-name"


def test_status_reports_missing_then_current(
    service: KnowledgeService,
    checklist_path: Path,
) -> None:
    assert service.status("123", checklist_path)["status"] == "missing"
    service.prepare("123", "payments", checklist_path)
    assert service.status("123", checklist_path)["status"] == "current"


def test_rebuild_forces_replacement_when_source_is_unchanged(
    service: KnowledgeService,
    checklist_path: Path,
) -> None:
    created = service.prepare("123", "payments", checklist_path)

    rebuilt = service.rebuild("123", "payments", checklist_path)

    assert rebuilt["knowledge_status"] == "rebuilt"
    assert rebuilt["selection_context"] == created["selection_context"]


def test_sync_reuses_current_source(
    service: KnowledgeService,
    checklist_path: Path,
) -> None:
    service.prepare("123", "payments", checklist_path)

    synced = service.sync("123", "payments", checklist_path)

    assert synced["knowledge_status"] == "reused"


def test_status_reports_override_conflict_without_mutating_database(
    service: KnowledgeService,
    checklist_path: Path,
) -> None:
    service.prepare("123", "payments", checklist_path)
    service.set_override(
        "123", "SEC-001", {"content": "临时规则"}, reason="应急"
    )
    checklist_path.write_text(
        checklist_path.read_text().replace("参数化处理", "安全参数绑定")
    )

    result = service.status("123", checklist_path)

    assert result["status"] == "conflict"
    assert result["conflict_keys"] == ["SEC-001"]
    assert service.list_overrides("123")[0]["status"] == "active"
