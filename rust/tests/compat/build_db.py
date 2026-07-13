"""Compat oracle: build a review-kb DB with the Python implementation.

The Rust test reads the same DB and asserts byte-identical query output.
Usage: `python build_db.py <db_path>` prints a canonical-JSON object with the
expected results of get_project / list_rules / list_overrides /
integrity_check / schema_version.
"""

import json
import sys

from review_kb.description import canonical_json
from review_kb.repository import Repository
from review_kb.service import KnowledgeService


def main() -> None:
    db = sys.argv[1]
    repo = Repository.open(db)
    repo.migrate()
    service = KnowledgeService(repo)
    service.prepare("123", "payments", "tests/fixtures/valid-checklist.md")
    service.set_override(
        "123",
        "SEC-001",
        {"summary": "覆盖摘要", "content": "覆盖内容", "tags": ["a", "b"]},
        reason="测试覆盖",
    )

    project = repo.get_project("123")
    rules = repo.list_rules("123")
    overrides = repo.list_overrides("123")
    print(
        json.dumps(
            {
                "project": canonical_json(project),
                "rules": canonical_json(rules),
                "overrides": canonical_json(overrides),
                "integrity": repo.integrity_check(),
                "schema": repo.schema_version(),
            }
        )
    )


if __name__ == "__main__":
    main()
