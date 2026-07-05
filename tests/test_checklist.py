from pathlib import Path

import pytest

from review_kb.checklist import parse_checklist
from review_kb.errors import ErrorCode, ReviewKBError


FIXTURE = Path("tests/fixtures/valid-checklist.md")


def test_parse_valid_checklist_preserves_rule_order() -> None:
    parsed = parse_checklist(FIXTURE)

    assert parsed.checklist_version == "2026.07.1"
    assert [rule.key for rule in parsed.rules] == ["SEC-001", "DB-004"]
    assert parsed.rules[0].tags == ["security", "database"]
    assert parsed.content_hash.startswith("sha256:")


def test_duplicate_key_is_rejected(tmp_path: Path) -> None:
    path = tmp_path / "review-checklist.md"
    path.write_text(FIXTURE.read_text().replace("DB-004", "SEC-001"))

    with pytest.raises(ReviewKBError) as caught:
        parse_checklist(path)

    assert caught.value.code is ErrorCode.CHECKLIST_INVALID
    assert "duplicate" in caught.value.message


@pytest.mark.parametrize(
    ("source", "message"),
    [
        ("# no front matter", "Front Matter"),
        (
            FIXTURE.read_text().replace("schema_version: 1", "schema_version: 2"),
            "schema_version",
        ),
        (
            FIXTURE.read_text().replace("global_description: |-", "extra: true\nglobal_description: |-"),
            "unknown",
        ),
        (FIXTURE.read_text().replace("## SEC-001", "## bad key!"), "rule key"),
        (
            FIXTURE.read_text().replace(
                "summary: 检查事务边界是否覆盖完整业务操作",
                "summary: ''",
            ),
            "summary",
        ),
    ],
)
def test_invalid_checklist_is_rejected(tmp_path: Path, source: str, message: str) -> None:
    path = tmp_path / "review-checklist.md"
    path.write_text(source)

    with pytest.raises(ReviewKBError) as caught:
        parse_checklist(path)

    assert caught.value.code is ErrorCode.CHECKLIST_INVALID
    assert message.lower() in caught.value.message.lower()
