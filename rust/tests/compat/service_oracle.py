"""Cross-binary oracle: drive the real Python `KnowledgeService` through the key
Phase-4 flows against a fixed 2-rule checklist and emit canonical JSON.

Invoked by `rust/tests/service_compat.rs`. Prints one canonical-JSON line per
labelled result. Timestamps (created_at/updated_at) are stripped where they
appear so the comparison is deterministic.
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

from review_kb.config import resolve_database_path
from review_kb.description import canonical_json
from review_kb.repository import Repository
from review_kb.service import KnowledgeService


CHECKLIST_SRC = """\
---
schema_version: 1
checklist_version: "2026.07.1"
global_description: project-wide guidance
---

## SEC-001

```yaml review-rule
summary: Check SQL parameterization
tags:
  - security
paths:
  - "src/**/*.py"
languages:
  - python
```

Do not concatenate SQL strings.

## DB-004

```yaml review-rule
summary: Check transaction boundaries
tags:
  - database
paths:
  - "src/**/services/*.py"
languages:
  - python
```

Wrap full business operations in a transaction.
"""


def strip_timestamps(value):
    """Remove created_at/updated_at recursively so the diff is deterministic."""
    if isinstance(value, dict):
        return {
            k: strip_timestamps(v)
            for k, v in value.items()
            if k not in {"created_at", "updated_at"}
        }
    if isinstance(value, list):
        return [strip_timestamps(v) for v in value]
    return value


def main() -> None:
    db_path = Path(sys.argv[1])
    checklist_path = Path(sys.argv[2])
    checklist_path.write_text(CHECKLIST_SRC, encoding="utf-8")

    repository = Repository.open(db_path)
    repository.migrate()
    service = KnowledgeService(repository)

    results = {}

    prepare_create = service.prepare("p1", "payments", checklist_path)
    results["prepare_create"] = prepare_create

    results["prepare_reuse"] = service.prepare("p1", "payments", checklist_path)

    results["status_current"] = service.status("p1", checklist_path)

    set_result = service.set_override(
        "p1",
        "SEC-001",
        {"summary": "OVERRIDDEN", "tags": ["security", "security", "extra"]},
        reason="why",
    )
    results["set_override"] = set_result

    active_revision = set_result["knowledge_revision"]
    results["get_selected_rules"] = service.get_selected_rules(
        {
            "project_id": "p1",
            "knowledge_revision": active_revision,
            "keys": ["SEC-001", "DB-004"],
        }
    )

    results["show_project"] = service.show_project("p1")
    results["get_description"] = service.get_description("p1")
    results["list_rules"] = service.list_rules("p1")
    results["list_overrides"] = service.list_overrides("p1")

    repository.close()

    # Emit each result as a labelled canonical-JSON line: LABEL\t<json>.
    out = {label: strip_timestamps(value) for label, value in results.items()}
    print(json.dumps({k: canonical_json(v) for k, v in out.items()}))


if __name__ == "__main__":
    main()
