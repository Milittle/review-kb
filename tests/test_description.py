import json

import pytest
from pydantic import ValidationError

from review_kb.checklist import parse_checklist
from review_kb.description import build_description, canonical_json
from review_kb.models import Selection


def test_description_is_deterministic_and_omits_rule_bodies() -> None:
    checklist = parse_checklist("tests/fixtures/valid-checklist.md")

    first = build_description("123", "payments", checklist)
    second = build_description("123", "payments", checklist)

    assert canonical_json(first) == canonical_json(second)
    assert "qualified_key" not in first["rules"][0]
    assert "content" not in first["rules"][0]
    assert first["checklist"]["knowledge_revision"].startswith("sha256:")
    assert json.loads(canonical_json(first))["rules"][0]["key"] == "SEC-001"


def test_project_name_does_not_change_knowledge_revision() -> None:
    checklist = parse_checklist("tests/fixtures/valid-checklist.md")

    before = build_description("123", "old-name", checklist)
    after = build_description("123", "new-name", checklist)

    assert before["checklist"]["knowledge_revision"] == after["checklist"]["knowledge_revision"]


def test_selection_rejects_duplicate_keys() -> None:
    with pytest.raises(ValidationError):
        Selection(
            project_id="123",
            knowledge_revision="sha256:value",
            keys=["SEC-001", "SEC-001"],
        )
