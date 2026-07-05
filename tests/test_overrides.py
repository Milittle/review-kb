from pathlib import Path

import pytest

from review_kb.checklist import parse_checklist
from review_kb.description import build_description
from review_kb.errors import ErrorCode, ReviewKBError
from review_kb.repository import Repository
from review_kb.service import KnowledgeService


@pytest.fixture
def repository(tmp_path: Path):
    repository = Repository.open(tmp_path / "knowledge.db")
    repository.migrate()
    checklist = parse_checklist("tests/fixtures/valid-checklist.md")
    repository.replace_project(
        "123",
        "payments",
        "/repo/review-checklist.md",
        checklist,
        build_description("123", "payments", checklist),
    )
    yield repository
    repository.close()


def test_repository_override_is_sparse_audited_and_does_not_mutate_source(
    repository: Repository,
) -> None:
    source_before = repository.get_rules("123", ["SEC-001"])[0]

    repository.upsert_override(
        "123",
        "SEC-001",
        {"content": "临时要求：所有 SQL 必须使用参数绑定。"},
        reason="应急修复规则描述",
    )

    override = repository.get_override("123", "SEC-001")
    source_after = repository.get_rules("123", ["SEC-001"])[0]
    assert override is not None
    assert override["status"] == "active"
    assert override["content"] == "临时要求：所有 SQL 必须使用参数绑定。"
    assert override["summary"] is None
    assert source_after == source_before
    assert repository.list_audit_log("123")[0]["action"] == "override.set"

    repository.disable_override("123", "SEC-001", reason="源规则已修复")
    assert repository.get_override("123", "SEC-001")["status"] == "disabled"
    assert repository.list_audit_log("123")[0]["action"] == "override.unset"


def test_service_override_changes_effective_description_and_selected_content(
    repository: Repository,
) -> None:
    service = KnowledgeService(repository)
    before = service.get_description("123")["checklist"]["knowledge_revision"]

    result = service.set_override(
        "123",
        "SEC-001",
        {
            "summary": "临时加强 SQL 安全检查",
            "content": "所有 SQL 必须使用参数绑定，并检查动态标识符白名单。",
        },
        reason="安全事件应急规则",
    )

    after = service.get_description("123")
    assert result["status"] == "active"
    assert after["rules"][0]["summary"] == "临时加强 SQL 安全检查"
    assert after["checklist"]["knowledge_revision"] != before
    selected = service.get_selected_rules(
        {
            "project_id": "123",
            "knowledge_revision": after["checklist"]["knowledge_revision"],
            "keys": ["SEC-001"],
        }
    )
    assert selected["rules"][0]["content"].startswith("所有 SQL")

    service.unset_override("123", "SEC-001", reason="应急结束")
    restored = service.get_description("123")
    assert restored["checklist"]["knowledge_revision"] == before


def test_source_change_to_overridden_rule_aborts_refresh(
    repository: Repository,
    tmp_path: Path,
) -> None:
    service = KnowledgeService(repository)
    service.set_override(
        "123",
        "SEC-001",
        {"content": "临时安全规则"},
        reason="应急修复",
    )
    project_before = service.show_project("123")
    changed = tmp_path / "review-checklist.md"
    changed.write_text(
        Path("tests/fixtures/valid-checklist.md")
        .read_text()
        .replace("参数化处理", "参数安全绑定")
    )

    with pytest.raises(ReviewKBError) as caught:
        service.prepare("123", "payments", changed)

    assert caught.value.code is ErrorCode.OVERRIDE_CONFLICT
    assert caught.value.details["keys"] == ["SEC-001"]
    assert service.show_project("123")["content_hash"] == project_before["content_hash"]
    assert service.list_overrides("123")[0]["status"] == "conflict"


def test_unrelated_source_refresh_preserves_override_without_mutating_source(
    repository: Repository,
    tmp_path: Path,
) -> None:
    service = KnowledgeService(repository)
    original_source = repository.get_rules("123", ["SEC-001"])[0]["content"]
    service.set_override(
        "123",
        "SEC-001",
        {"content": "临时安全规则"},
        reason="应急修复",
    )
    changed = tmp_path / "review-checklist.md"
    changed.write_text(
        Path("tests/fixtures/valid-checklist.md")
        .read_text()
        .replace("检查事务边界是否覆盖完整业务操作", "检查事务提交与回滚边界")
        .replace('checklist_version: "2026.07.1"', 'checklist_version: "2026.07.2"')
    )

    result = service.prepare("123", "payments", changed)

    assert result["knowledge_status"] == "refreshed"
    assert repository.get_rules("123", ["SEC-001"])[0]["content"] == original_source
    revision = result["selection_context"]["knowledge_revision"]
    selected = service.get_selected_rules(
        {"project_id": "123", "knowledge_revision": revision, "keys": ["SEC-001"]}
    )
    assert selected["rules"][0]["content"] == "临时安全规则"


def _conflicting_checklist(tmp_path: Path) -> Path:
    changed = tmp_path / "review-checklist.md"
    changed.write_text(
        Path("tests/fixtures/valid-checklist.md")
        .read_text()
        .replace("参数化处理", "参数安全绑定")
        .replace('checklist_version: "2026.07.1"', 'checklist_version: "2026.07.2"')
    )
    return changed


def test_resolve_override_accept_source_disables_override_and_refreshes(
    repository: Repository,
    tmp_path: Path,
) -> None:
    service = KnowledgeService(repository)
    service.set_override(
        "123", "SEC-001", {"content": "临时安全规则"}, reason="应急修复"
    )
    changed = _conflicting_checklist(tmp_path)
    with pytest.raises(ReviewKBError):
        service.prepare("123", "payments", changed)

    result = service.resolve_override(
        "123",
        "SEC-001",
        strategy="accept-source",
        checklist_path=changed,
        reason="接受仓库新规则",
    )

    assert result["knowledge_status"] == "refreshed"
    assert service.list_overrides("123")[0]["status"] == "disabled"
    assert "参数安全绑定" in repository.get_rules("123", ["SEC-001"])[0]["summary"]


def test_resolve_override_keep_rebases_override_and_refreshes(
    repository: Repository,
    tmp_path: Path,
) -> None:
    service = KnowledgeService(repository)
    service.set_override(
        "123", "SEC-001", {"content": "继续保留的临时规则"}, reason="应急修复"
    )
    changed = _conflicting_checklist(tmp_path)
    with pytest.raises(ReviewKBError):
        service.prepare("123", "payments", changed)

    result = service.resolve_override(
        "123",
        "SEC-001",
        strategy="keep-override",
        checklist_path=changed,
        reason="确认覆盖仍然有效",
    )

    override = service.list_overrides("123")[0]
    assert override["status"] == "active"
    assert result["knowledge_status"] == "refreshed"
    selected = service.get_selected_rules(
        {**result["selection_context"], "keys": ["SEC-001"]}
    )
    assert selected["rules"][0]["content"] == "继续保留的临时规则"
