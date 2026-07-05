from __future__ import annotations

import hashlib
import json
from typing import Any

from .models import Checklist


def canonical_json(value: Any) -> str:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    )


def _rule_summary(rule: Any) -> dict[str, Any]:
    return {
        "key": rule.key,
        "summary": rule.summary,
        "tags": rule.tags,
        "paths": rule.paths,
        "languages": rule.languages,
    }


def build_description(
    project_id: str,
    project_name: str,
    checklist: Checklist,
) -> dict[str, Any]:
    summaries = [_rule_summary(rule) for rule in checklist.rules]
    effective_knowledge = {
        "global_description": checklist.global_description,
        "rules": [
            {
                **summary,
                "content": rule.content,
            }
            for summary, rule in zip(summaries, checklist.rules)
        ],
    }
    revision = "sha256:" + hashlib.sha256(
        canonical_json(effective_knowledge).encode("utf-8")
    ).hexdigest()
    return {
        "project": {"id": project_id, "name": project_name},
        "checklist": {
            "schema_version": checklist.schema_version,
            "version": checklist.checklist_version,
            "content_hash": checklist.content_hash,
            "knowledge_revision": revision,
        },
        "global_description": checklist.global_description,
        "rules": summaries,
    }
