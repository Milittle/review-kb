from pathlib import Path

import pytest

from review_kb.errors import ErrorCode, ReviewKBError
from review_kb.repository import Repository
from review_kb.service import KnowledgeService


@pytest.fixture
def prepared_service(tmp_path: Path):
    repository = Repository.open(tmp_path / "knowledge.db")
    repository.migrate()
    service = KnowledgeService(repository)
    service.prepare("123", "payments", "tests/fixtures/valid-checklist.md")
    yield service
    repository.close()


def selection(service: KnowledgeService, keys: list[str]) -> dict[str, object]:
    revision = service.get_description("123")["checklist"]["knowledge_revision"]
    return {
        "project_id": "123",
        "knowledge_revision": revision,
        "keys": keys,
    }


def test_selected_rules_preserve_input_order(prepared_service: KnowledgeService) -> None:
    result = prepared_service.get_selected_rules(
        selection(prepared_service, ["DB-004", "SEC-001"])
    )

    assert [rule["key"] for rule in result["rules"]] == ["DB-004", "SEC-001"]


def test_missing_key_returns_suggestion_without_partial_rules(
    prepared_service: KnowledgeService,
) -> None:
    with pytest.raises(ReviewKBError) as caught:
        prepared_service.get_selected_rules(
            selection(prepared_service, ["SEC-01", "DB-004"])
        )

    assert caught.value.code is ErrorCode.RULE_NOT_FOUND
    assert caught.value.details["not_found"] == ["SEC-01"]
    assert caught.value.details["suggestions"]["SEC-01"][0] == "SEC-001"
    assert "rules" not in caught.value.details


def test_stale_revision_requires_prepare(prepared_service: KnowledgeService) -> None:
    with pytest.raises(ReviewKBError) as caught:
        prepared_service.get_selected_rules(
            {
                "project_id": "123",
                "knowledge_revision": "sha256:old",
                "keys": ["SEC-001"],
            }
        )

    assert caught.value.code is ErrorCode.KNOWLEDGE_REVISION_MISMATCH


def test_duplicate_keys_are_invalid_selection(prepared_service: KnowledgeService) -> None:
    with pytest.raises(ReviewKBError) as caught:
        prepared_service.get_selected_rules(
            selection(prepared_service, ["SEC-001", "SEC-001"])
        )

    assert caught.value.code is ErrorCode.INVALID_SELECTION


def test_selection_reads_revision_rules_and_overrides_from_one_snapshot(
    prepared_service: KnowledgeService,
    monkeypatch,
) -> None:
    request = selection(prepared_service, ["SEC-001"])
    repository = prepared_service.repository
    original_list_rules = repository.list_rules
    interleaved = False

    def list_rules_with_concurrent_override(project_id: str):
        nonlocal interleaved
        if not interleaved:
            interleaved = True
            writer = Repository.open(repository.path)
            writer.migrate()
            try:
                KnowledgeService(writer).set_override(
                    "123",
                    "SEC-001",
                    {"content": "并发覆盖规则"},
                    reason="模拟并发写入",
                )
            finally:
                writer.close()
        return original_list_rules(project_id)

    monkeypatch.setattr(repository, "list_rules", list_rules_with_concurrent_override)

    result = prepared_service.get_selected_rules(request)

    assert result["rules"][0]["content"] != "并发覆盖规则"
