# Code Review Knowledge CLI Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the first runnable `review-kb` vertical slice that deterministically imports a project checklist into SQLite and safely serves rule descriptions and selected rule bodies to an MR review agent.

**Architecture:** A Typer adapter calls a framework-independent service. The service composes a strict Markdown/YAML parser, deterministic description builder, and transactional SQLite repository. Every CLI invocation emits one JSON document; agent rule selection is bound to an effective knowledge revision and validated before any rule body is returned.

**Tech Stack:** Python 3.10+, Typer, Pydantic 2, markdown-it-py, PyYAML safe loader, stdlib sqlite3, pytest.

---

## Scope

This plan implements the design's phase-one review loop plus the read-only queries needed to diagnose it:

- checklist parsing and validation;
- SQLite initialization and schema migration;
- prepare/status and deterministic description;
- project/rule list, exact batch get, and literal search;
- structured selection validation, revision mismatch detection, and typo suggestions;
- database path configuration and unified JSON output;
- integration tutorial and end-to-end tests.

Override lifecycle and destructive database operations are independent operational subsystems. They will be implemented in a second plan after this vertical slice is validated. The phase-one schema includes migration support but does not create unused override/audit tables.

## File Map

- `pyproject.toml`: package metadata, dependencies, console entry point, pytest configuration.
- `src/review_kb/errors.py`: stable domain error codes and exit categories.
- `src/review_kb/models.py`: validated domain and response models.
- `src/review_kb/checklist.py`: strict Markdown/YAML parser and source hashing.
- `src/review_kb/description.py`: canonical JSON and effective knowledge revision.
- `src/review_kb/repository.py`: SQLite migration and transactional queries.
- `src/review_kb/service.py`: prepare/status/query orchestration.
- `src/review_kb/config.py`: database path precedence.
- `src/review_kb/cli.py`: Typer commands, stdin protocol, JSON/exit handling.
- `src/review_kb/migrations/001_initial.sql`: phase-one schema.
- `tests/`: focused unit, repository, service, and CLI contract tests.
- `docs/integration-guide.md`: MR workflow integration tutorial and recovery loop.

### Task 1: Package skeleton and stable error contract

**Files:**
- Create: `pyproject.toml`
- Create: `src/review_kb/__init__.py`
- Create: `src/review_kb/errors.py`
- Create: `tests/test_errors.py`

- [ ] **Step 1: Write the failing error-contract test**

```python
from review_kb.errors import ErrorCode, ReviewKBError


def test_domain_error_has_stable_json_shape_and_exit_code():
    error = ReviewKBError(ErrorCode.RULE_NOT_FOUND, "missing", {"keys": ["SEC-1"]})
    assert error.as_dict() == {
        "code": "RULE_NOT_FOUND",
        "message": "missing",
        "details": {"keys": ["SEC-1"]},
    }
    assert error.exit_code == 3
```

- [ ] **Step 2: Run the test and verify import failure**

Run: `uv run pytest tests/test_errors.py -q`  
Expected: FAIL because `review_kb` does not exist.

- [ ] **Step 3: Add package metadata and error types**

`pyproject.toml` must declare Python `>=3.10`, runtime dependencies `typer>=0.15,<1`, `pydantic>=2,<3`, `markdown-it-py>=4,<5`, `PyYAML>=6,<7`, and console script `review-kb = "review_kb.cli:main"`. Implement `ErrorCode` as a string enum and `ReviewKBError(code, message, details=None)` with mappings: input `2`, missing `3`, conflict `4`, database `5`, internal `1`.

Required codes for this plan are `INVALID_ARGUMENT`, `CHECKLIST_NOT_FOUND`, `CHECKLIST_INVALID`, `PROJECT_NOT_FOUND`, `RULE_NOT_FOUND`, `INVALID_SELECTION`, `KNOWLEDGE_REVISION_MISMATCH`, `DB_LOCKED`, `DB_INTEGRITY_ERROR`, and `INTERNAL_ERROR`.

- [ ] **Step 4: Run the focused test**

Run: `uv run pytest tests/test_errors.py -q`  
Expected: `1 passed`.

### Task 2: Domain models and strict checklist parser

**Files:**
- Create: `src/review_kb/models.py`
- Create: `src/review_kb/checklist.py`
- Create: `tests/fixtures/valid-checklist.md`
- Create: `tests/test_checklist.py`

- [ ] **Step 1: Write parser success and validation tests**

```python
from pathlib import Path
import pytest
from review_kb.checklist import parse_checklist
from review_kb.errors import ErrorCode, ReviewKBError


def test_parse_valid_checklist_preserves_rule_order():
    parsed = parse_checklist(Path("tests/fixtures/valid-checklist.md"))
    assert parsed.checklist_version == "2026.07.1"
    assert [rule.key for rule in parsed.rules] == ["SEC-001", "DB-004"]
    assert parsed.rules[0].tags == ["security", "database"]
    assert parsed.content_hash.startswith("sha256:")


def test_duplicate_key_is_rejected(tmp_path: Path):
    source = Path("tests/fixtures/valid-checklist.md").read_text()
    path = tmp_path / "review-checklist.md"
    path.write_text(source.replace("DB-004", "SEC-001"))
    with pytest.raises(ReviewKBError) as caught:
        parse_checklist(path)
    assert caught.value.code is ErrorCode.CHECKLIST_INVALID
```

- [ ] **Step 2: Verify the tests fail**

Run: `uv run pytest tests/test_checklist.py -q`  
Expected: FAIL because parser/models are missing.

- [ ] **Step 3: Implement models and parser**

Define immutable Pydantic models `Rule(key, summary, content, tags, paths, languages, source_rule_hash)` and `Checklist(schema_version, checklist_version, global_description, content_hash, rules)`.

`parse_checklist(path)` must:

1. read UTF-8 bytes and compute `sha256:<hex>` over exact bytes;
2. split mandatory YAML Front Matter and call `yaml.safe_load`;
3. reject unknown file and rule metadata fields;
4. tokenize Markdown with `MarkdownIt("commonmark")`;
5. treat each level-two heading as a key matching `[A-Za-z0-9][A-Za-z0-9._-]{0,127}`;
6. require exactly one following fenced block with info `yaml review-rule`;
7. require non-empty `summary` and Markdown body; normalize optional arrays by stable deduplication;
8. reject duplicate keys, including keys differing only by ASCII case;
9. compute each source rule hash from canonical JSON of its effective fields.

- [ ] **Step 4: Add table-driven invalid-format cases and run tests**

Cases: missing Front Matter, unsupported schema, empty global description, malformed YAML, unknown field, empty body, bad key, duplicate case-insensitive key, and second level-two heading inside a body.

Run: `uv run pytest tests/test_checklist.py -q`  
Expected: all parser tests pass.

### Task 3: Deterministic description and selection models

**Files:**
- Modify: `src/review_kb/models.py`
- Create: `src/review_kb/description.py`
- Create: `tests/test_description.py`

- [ ] **Step 1: Write deterministic output tests**

```python
from review_kb.checklist import parse_checklist
from review_kb.description import build_description, canonical_json


def test_description_is_deterministic_and_does_not_expose_qualified_key():
    checklist = parse_checklist("tests/fixtures/valid-checklist.md")
    first = build_description("123", "payments", checklist)
    second = build_description("123", "payments", checklist)
    assert canonical_json(first) == canonical_json(second)
    assert "qualified_key" not in first["rules"][0]
    assert first["checklist"]["knowledge_revision"].startswith("sha256:")
```

- [ ] **Step 2: Verify missing implementation failure**

Run: `uv run pytest tests/test_description.py -q`  
Expected: FAIL importing `review_kb.description`.

- [ ] **Step 3: Implement canonicalization and revision**

`canonical_json(value)` uses `json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))`. `build_description(project_id, project_name, checklist)` preserves rule order, omits rule bodies, and computes `knowledge_revision` as SHA-256 of canonical effective rule fields plus global description. Timestamps and filesystem metadata must not enter this hash.

Add `Selection(project_id, knowledge_revision, keys)` with non-empty keys, exact string values, and duplicate rejection.

- [ ] **Step 4: Run description tests**

Run: `uv run pytest tests/test_description.py -q`  
Expected: all tests pass.

### Task 4: SQLite migration and repository

**Files:**
- Create: `src/review_kb/migrations/__init__.py`
- Create: `src/review_kb/migrations/001_initial.sql`
- Create: `src/review_kb/repository.py`
- Create: `tests/test_repository.py`

- [ ] **Step 1: Write repository transaction tests**

```python
def test_replace_project_is_atomic(repository, parsed_checklist):
    description = build_description("123", "payments", parsed_checklist)
    repository.replace_project("123", "payments", "/repo/review-checklist.md", parsed_checklist, description)
    project = repository.get_project("123")
    assert project["rule_count"] == 2
    assert repository.get_rules("123", ["DB-004", "SEC-001"])[0]["key"] == "DB-004"


def test_same_rule_key_can_exist_in_different_projects(repository, parsed_checklist):
    repository.replace_project("123", "one", "/one/review-checklist.md", parsed_checklist,
                               build_description("123", "one", parsed_checklist))
    repository.replace_project("456", "two", "/two/review-checklist.md", parsed_checklist,
                               build_description("456", "two", parsed_checklist))
    assert repository.get_rules("123", ["SEC-001"])
    assert repository.get_rules("456", ["SEC-001"])
```

- [ ] **Step 2: Verify repository tests fail**

Run: `uv run pytest tests/test_repository.py -q`  
Expected: FAIL because repository is missing.

- [ ] **Step 3: Implement initial schema and repository**

Migration creates `schema_migrations`, `projects`, `rules`, and `sync_history` matching the design. Enable `PRAGMA foreign_keys=ON`, WAL for file databases, `busy_timeout=5000`, row factory, and explicit transactions.

Implement `Repository.open(path)`, `migrate()`, `get_project()`, `replace_project()`, `list_projects()`, `list_rules()`, `get_rules()`, `search_rules()`, and `integrity_check()`. `replace_project` writes project metadata, ordered rules, description JSON, content hash, and knowledge revision in one transaction. Translate SQLite busy/locked errors to `DB_LOCKED` and integrity failures to `DB_INTEGRITY_ERROR`.

- [ ] **Step 4: Run repository tests including rollback and literal search cases**

Run: `uv run pytest tests/test_repository.py -q`  
Expected: all tests pass.

### Task 5: Prepare, status, and query service

**Files:**
- Create: `src/review_kb/service.py`
- Create: `tests/test_service.py`

- [ ] **Step 1: Write lifecycle tests**

```python
def test_prepare_creates_then_reuses(service, checklist_path):
    created = service.prepare("123", "payments", checklist_path)
    reused = service.prepare("123", "payments", checklist_path)
    assert created["knowledge_status"] == "created"
    assert reused["knowledge_status"] == "reused"


def test_content_change_without_version_bump_refreshes_with_warning(service, checklist_path):
    service.prepare("123", "payments", checklist_path)
    checklist_path.write_text(checklist_path.read_text().replace("参数化", "绑定参数"))
    result = service.prepare("123", "payments", checklist_path)
    assert result["knowledge_status"] == "refreshed"
    assert "CONTENT_CHANGED_WITHOUT_VERSION_BUMP" in result["warnings"]
```

- [ ] **Step 2: Verify service tests fail**

Run: `uv run pytest tests/test_service.py -q`  
Expected: FAIL importing the service.

- [ ] **Step 3: Implement service orchestration**

`KnowledgeService` exposes `prepare`, `status`, `get_description`, `list_projects`, `show_project`, `list_rules`, and `search_rules`. `prepare` parses before opening a write transaction, compares version and source hash, updates project name without reporting a rule rebuild, and returns description plus `selection_context`, status, rule count, hash, and warnings.

`status` never creates the database and returns one of `missing/current/stale/invalid`. Parsing errors remain structured errors rather than being collapsed into `invalid` unless the command succeeds with an explicit status payload.

- [ ] **Step 4: Run lifecycle and query tests**

Run: `uv run pytest tests/test_service.py -q`  
Expected: all tests pass.

### Task 6: Safe batch selection and recovery suggestions

**Files:**
- Modify: `src/review_kb/service.py`
- Create: `src/review_kb/suggestions.py`
- Create: `tests/test_selection.py`

- [ ] **Step 1: Write strict selection tests**

```python
def test_missing_key_returns_suggestion_without_partial_rules(prepared_service):
    revision = prepared_service.get_description("123")["checklist"]["knowledge_revision"]
    with pytest.raises(ReviewKBError) as caught:
        prepared_service.get_selected_rules({
            "project_id": "123",
            "knowledge_revision": revision,
            "keys": ["SEC-01", "DB-004"],
        })
    assert caught.value.code is ErrorCode.RULE_NOT_FOUND
    assert caught.value.details["not_found"] == ["SEC-01"]
    assert caught.value.details["suggestions"]["SEC-01"][0] == "SEC-001"
    assert "rules" not in caught.value.details


def test_stale_revision_requires_prepare(prepared_service):
    with pytest.raises(ReviewKBError) as caught:
        prepared_service.get_selected_rules({
            "project_id": "123", "knowledge_revision": "sha256:old", "keys": ["SEC-001"]
        })
    assert caught.value.code is ErrorCode.KNOWLEDGE_REVISION_MISMATCH
```

- [ ] **Step 2: Verify selection tests fail**

Run: `uv run pytest tests/test_selection.py -q`  
Expected: FAIL because selection service is missing.

- [ ] **Step 3: Implement validation and deterministic suggestions**

Parse selection JSON through the Pydantic model. Compare revision before querying bodies. Resolve every key exactly and return nothing until all keys exist. Generate at most three suggestions ordered by: case-insensitive exact match, prefix relation, Levenshtein distance, then source ordinal as tie-breaker. Suggestions never change the requested keys.

- [ ] **Step 4: Run strict selection tests**

Run: `uv run pytest tests/test_selection.py -q`  
Expected: all tests pass.

### Task 7: Configuration and JSON-only CLI

**Files:**
- Create: `src/review_kb/config.py`
- Create: `src/review_kb/cli.py`
- Create: `tests/test_cli.py`

- [ ] **Step 1: Write CLI contract tests**

```python
def test_prepare_and_rules_get_emit_one_json_document(cli_runner, checklist_path, db_path):
    prepared = cli_runner.invoke(app, ["--db", str(db_path), "prepare", "--project-id", "123",
                                       "--project-name", "payments", "--checklist", str(checklist_path)])
    payload = json.loads(prepared.stdout)
    selection = payload["data"]["selection_context"] | {"keys": ["SEC-001"]}
    fetched = cli_runner.invoke(app, ["--db", str(db_path), "rules", "get", "--input", "-"],
                                input=json.dumps(selection))
    assert fetched.exit_code == 0
    assert json.loads(fetched.stdout)["data"]["rules"][0]["key"] == "SEC-001"


def test_bad_selection_has_json_error_and_exit_two(cli_runner, db_path):
    result = cli_runner.invoke(app, ["--db", str(db_path), "rules", "get", "--input", "-"], input="{}")
    assert result.exit_code == 2
    assert json.loads(result.stdout)["error"]["code"] == "INVALID_SELECTION"
```

- [ ] **Step 2: Verify CLI tests fail**

Run: `uv run pytest tests/test_cli.py -q`  
Expected: FAIL because CLI/config modules are missing.

- [ ] **Step 3: Implement database path precedence and commands**

Path precedence: global `--db`, `REVIEW_KB_DB`, TOML config `db_path`, then platform data default. Implement commands:

- `prepare`, `status`;
- `description get`;
- `projects list/show`;
- `rules list/get/search`;
- `db info/check`;
- `config show`.

Wrap all command results as `{ok,data,warnings,meta}`. Convert known domain errors to `{ok:false,error,warnings,meta}` and their stable exit code. stdout contains only JSON; diagnostics use stderr. `rules get --input -` reads exactly one JSON object and rejects trailing non-whitespace content.

- [ ] **Step 4: Run CLI contract tests**

Run: `uv run pytest tests/test_cli.py -q`  
Expected: all tests pass.

### Task 8: Integration tutorial and end-to-end verification

**Files:**
- Create: `docs/integration-guide.md`
- Create: `examples/review-checklist.md`
- Create: `tests/test_e2e.py`

- [ ] **Step 1: Write an end-to-end subprocess test**

The test creates a temporary DB, invokes the installed `review-kb prepare`, extracts `selection_context`, invokes `rules get --input -`, deliberately submits `SEC-01`, verifies `RULE_NOT_FOUND` and suggestion `SEC-001`, retries with the corrected key, and finally modifies the checklist and verifies the old revision produces `KNOWLEDGE_REVISION_MISMATCH`.

- [ ] **Step 2: Verify the end-to-end test fails before documentation/example wiring**

Run: `uv run pytest tests/test_e2e.py -q`  
Expected: FAIL until the fixture and subprocess invocation are complete.

- [ ] **Step 3: Write the integration guide**

Document these exact stages with executable JSON examples:

1. configure `REVIEW_KB_DB` or `--db`;
2. maintain the fixed checklist format;
3. call `prepare` using CodeHub project ID/name;
4. pass only description and selection contract to the Agent;
5. send Agent-selected keys through stdin JSON;
6. on `RULE_NOT_FOUND`, use suggestions for one explicit retry;
7. on `KNOWLEDGE_REVISION_MISMATCH`, discard the selection and restart from prepare;
8. use projects/rules/search/db-check commands for offline diagnosis.

The guide must explicitly warn against fuzzy auto-selection, comma-delimited Agent output, ignoring missing keys, and reusing a selection across revisions.

- [ ] **Step 4: Run complete verification**

Run: `uv run pytest -q`  
Expected: all tests pass.

Run: `uv run review-kb --help`  
Expected: exit `0`, with prepare, status, description, projects, rules, db, and config command groups visible.

Run: `uv run review-kb --db /tmp/review-kb-smoke.db prepare --project-id smoke --project-name smoke --checklist examples/review-checklist.md`  
Expected: JSON with `ok: true`, `knowledge_status: created`, two rules, and a `selection_context`.

## Completion Gate

Before declaring the core CLI complete:

- every acceptance criterion covered by this phase has a passing automated test;
- source, tests, and integration documentation contain no unresolved marker comments;
- running the same prepare twice returns `created` then `reused` and identical knowledge revisions;
- malformed or stale Agent selection never returns partial rule bodies;
- the tutorial commands work against a fresh temporary database.
