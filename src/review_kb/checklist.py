from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path
from typing import Any

import yaml
from markdown_it import MarkdownIt

from .errors import ErrorCode, ReviewKBError
from .models import Checklist, Rule


_FRONT_MATTER = re.compile(r"\A---\r?\n(.*?)\r?\n---(?:\r?\n|\Z)", re.DOTALL)
_KEY = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}\Z")
_FILE_FIELDS = {"schema_version", "checklist_version", "global_description"}
_RULE_FIELDS = {"summary", "tags", "paths", "languages"}


def _invalid(message: str, path: Path, **details: Any) -> ReviewKBError:
    return ReviewKBError(
        ErrorCode.CHECKLIST_INVALID,
        message,
        {"path": str(path), **details},
    )


def _canonical_hash(value: Any) -> str:
    encoded = json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return f"sha256:{hashlib.sha256(encoded).hexdigest()}"


def _string_list(value: Any, field: str, path: Path) -> list[str]:
    if value is None:
        return []
    if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
        raise _invalid(f"{field} must be an array of strings", path)
    result: list[str] = []
    seen: set[str] = set()
    for item in value:
        if not item.strip():
            raise _invalid(f"{field} entries must not be empty", path)
        if item not in seen:
            seen.add(item)
            result.append(item)
    return result


def _yaml_mapping(source: str, label: str, path: Path) -> dict[str, Any]:
    try:
        value = yaml.safe_load(source)
    except yaml.YAMLError as error:
        raise _invalid(f"invalid YAML in {label}: {error}", path) from error
    if not isinstance(value, dict):
        raise _invalid(f"{label} must contain a YAML mapping", path)
    return value


def parse_checklist(path: str | Path) -> Checklist:
    checklist_path = Path(path)
    try:
        raw = checklist_path.read_bytes()
    except FileNotFoundError as error:
        raise ReviewKBError(
            ErrorCode.CHECKLIST_NOT_FOUND,
            f"checklist not found: {checklist_path}",
            {"path": str(checklist_path)},
        ) from error
    try:
        source = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise _invalid("checklist must be valid UTF-8", checklist_path) from error

    match = _FRONT_MATTER.match(source)
    if match is None:
        raise _invalid("checklist must start with YAML Front Matter", checklist_path)
    file_meta = _yaml_mapping(match.group(1), "Front Matter", checklist_path)
    unknown_file_fields = sorted(set(file_meta) - _FILE_FIELDS)
    if unknown_file_fields:
        raise _invalid(
            f"unknown Front Matter fields: {', '.join(unknown_file_fields)}",
            checklist_path,
        )
    if file_meta.get("schema_version") != 1:
        raise _invalid("schema_version must be 1", checklist_path)
    checklist_version = file_meta.get("checklist_version")
    global_description = file_meta.get("global_description")
    if not isinstance(checklist_version, str) or not checklist_version.strip():
        raise _invalid("checklist_version must be a non-empty string", checklist_path)
    if not isinstance(global_description, str) or not global_description.strip():
        raise _invalid("global_description must be a non-empty string", checklist_path)

    markdown = source[match.end() :]
    lines = markdown.splitlines()
    tokens = MarkdownIt("commonmark").parse(markdown)
    headings: list[tuple[int, int, str]] = []
    for index, token in enumerate(tokens):
        if token.type != "heading_open" or token.tag != "h2" or token.map is None:
            continue
        inline = tokens[index + 1] if index + 1 < len(tokens) else None
        key = inline.content.strip() if inline is not None and inline.type == "inline" else ""
        headings.append((index, token.map[0], key))
    if not headings:
        raise _invalid("checklist must contain at least one level-two rule", checklist_path)

    rules: list[Rule] = []
    seen_keys: dict[str, str] = {}
    for position, (token_index, heading_line, key) in enumerate(headings):
        if _KEY.fullmatch(key) is None:
            raise _invalid(f"invalid rule key: {key!r}", checklist_path, line=heading_line + 1)
        folded_key = key.casefold()
        if folded_key in seen_keys:
            raise _invalid(
                f"duplicate rule key: {key} conflicts with {seen_keys[folded_key]}",
                checklist_path,
                line=heading_line + 1,
            )
        seen_keys[folded_key] = key

        end_line = headings[position + 1][1] if position + 1 < len(headings) else len(lines)
        section_fences = [
            token
            for token in tokens[token_index + 1 :]
            if token.type == "fence"
            and token.map is not None
            and heading_line < token.map[0] < end_line
            and token.info.strip() == "yaml review-rule"
        ]
        if len(section_fences) != 1:
            raise _invalid(
                f"rule {key} must contain exactly one `yaml review-rule` metadata block",
                checklist_path,
                line=heading_line + 1,
            )
        fence = section_fences[0]
        assert fence.map is not None
        if any(line.strip() for line in lines[heading_line + 1 : fence.map[0]]):
            raise _invalid(
                f"rule {key} metadata block must appear before rule content",
                checklist_path,
                line=heading_line + 1,
            )
        rule_meta = _yaml_mapping(fence.content, f"rule {key} metadata", checklist_path)
        unknown_rule_fields = sorted(set(rule_meta) - _RULE_FIELDS)
        if unknown_rule_fields:
            raise _invalid(
                f"unknown fields for rule {key}: {', '.join(unknown_rule_fields)}",
                checklist_path,
                line=fence.map[0] + 1,
            )
        summary = rule_meta.get("summary")
        if not isinstance(summary, str) or not summary.strip() or "\n" in summary:
            raise _invalid(f"rule {key} summary must be a non-empty single line", checklist_path)
        tags = _string_list(rule_meta.get("tags"), "tags", checklist_path)
        paths = _string_list(rule_meta.get("paths"), "paths", checklist_path)
        languages = _string_list(rule_meta.get("languages"), "languages", checklist_path)
        if any(language != language.lower() for language in languages):
            raise _invalid(f"rule {key} languages must be lowercase", checklist_path)
        content = "\n".join(lines[fence.map[1] : end_line]).strip()
        if not content:
            raise _invalid(f"rule {key} content must not be empty", checklist_path)
        effective = {
            "key": key,
            "summary": summary.strip(),
            "content": content,
            "tags": tags,
            "paths": paths,
            "languages": languages,
        }
        rules.append(Rule(**effective, source_rule_hash=_canonical_hash(effective)))

    return Checklist(
        schema_version=1,
        checklist_version=checklist_version.strip(),
        global_description=global_description.strip(),
        content_hash=f"sha256:{hashlib.sha256(raw).hexdigest()}",
        rules=rules,
    )
