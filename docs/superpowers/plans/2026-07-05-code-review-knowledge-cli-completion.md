# Code Review Knowledge CLI Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Complete the operational commands and integration documentation required to run `review-kb` directly in an MR review workflow.

**Architecture:** Extend the existing Typer → `KnowledgeService` → `Repository` boundaries. Sync and override resolution remain domain operations; configuration writes live in `config.py`; database maintenance stays behind validated repository APIs. Every command preserves the single-JSON stdout contract.

**Tech Stack:** Python 3.10+, Typer, Pydantic 2, stdlib sqlite3 backup API, pytest.

---

The workspace is not a valid Git repository, so this plan cannot use worktrees or commit checkpoints. Changes are applied incrementally in the current workspace and verified after every task.

## File Map

- `src/review_kb/service.py`: force rebuild and override conflict resolution workflows.
- `src/review_kb/repository.py`: override resolution, migration information, safe read-only views, backup and restore.
- `src/review_kb/config.py`: read and atomically write the configured database path.
- `src/review_kb/cli.py`: expose sync/rebuild, resolve, config, and DB operations through JSON commands.
- `tests/test_service.py`: rebuild lifecycle tests.
- `tests/test_overrides.py`: conflict resolution tests.
- `tests/test_repository.py`: query, backup, restore, and migration tests.
- `tests/test_cli.py`: command-level JSON contracts.
- `tests/test_e2e.py`: complete MR integration and recovery smoke flows.
- `docs/integration-guide.md`: installation, automation state machine, maintenance, recovery, and production checklist.

### Task 1: Explicit sync and force rebuild

- [x] Add failing service tests proving `rebuild` writes a new sync-history action even when source version/hash are unchanged, and still rejects unresolved override conflicts.
- [x] Run `uv run pytest tests/test_service.py -q` and confirm failure because `KnowledgeService.rebuild` does not exist.
- [x] Extract the existing prepare import path into an internal method with `force` and `action`; expose `sync` as explicit prepare semantics and `rebuild` as forced transactional replacement.
- [x] Add failing CLI tests for `sync` and `rebuild` with the same project arguments as `prepare`.
- [x] Add Typer commands that return the standard JSON envelope and run focused service/CLI tests to green.

### Task 2: Resolve override conflicts

- [x] Add failing tests for `resolve_override(..., strategy="accept-source")` disabling the override and importing the changed source, and `strategy="keep-override"` rebasing it onto the changed source while preserving effective content.
- [x] Run `uv run pytest tests/test_overrides.py -q` and confirm both tests fail because resolution APIs are missing.
- [x] Add repository method `resolve_override(project_id, rule_key, base_source_rule_hash, keep, reason)` that updates status/base hash or disables the override and writes an audit row in the same transaction.
- [x] Add service validation for strategies `keep-override` and `accept-source`; parse the supplied checklist, require the key to exist for keep, resolve, then call prepare using the stored project name.
- [x] Add `overrides resolve --project-id --key --strategy --checklist --reason` and CLI JSON tests, then run override and CLI suites to green.

### Task 3: Configuration get/set

- [x] Add failing config tests proving `write_database_path` creates a TOML file atomically and `read_configured_database_path` returns the explicit configured value.
- [x] Implement safe TOML string escaping, parent creation, temporary-file write, and `Path.replace`; reject blank values.
- [x] Add `config get db_path` and `config set db_path PATH`, with optional `REVIEW_KB_CONFIG` support so automation can choose a config location without writing under the user home.
- [x] Add CLI tests using an isolated config path and run them to green.

### Task 4: Database maintenance

- [x] Add failing repository tests for schema version reporting, allowlisted read-only view queries, consistent backup, and restore from a valid database.
- [x] Implement `schema_version`, `query_view(view, project_id=None, query=None, limit=100)`, `backup(output)`, and class-level `restore(source, destination)` using SQLite backup plus integrity/schema validation.
- [x] Restrict query names to `projects`, `rules`, `overrides`, `sync_history`, and `audit_log`; do not accept raw SQL. Apply deterministic ordering and a validated limit from 1 to 1000.
- [x] Add `db query --view ...`, `db backup --output ...`, `db restore --input ...`, and `db migrate`; restore must create a timestamped safety backup of an existing destination before replacement.
- [x] Add CLI tests for success and invalid view/backup, then run repository and CLI suites to green.

### Task 5: Direct integration tutorial and end-to-end flow

- [x] Extend `docs/integration-guide.md` with release installation, exact database provisioning, prepare → Agent selection → rules get examples, retry behavior, sync/rebuild, override resolution, backup/restore, config commands, exit-code handling, and a production-readiness checklist.
- [x] Add an executable-style end-to-end pytest that invokes prepare, reads selection context, fetches rules, applies an override, rejects the stale selection, creates a new selection, backs up the DB, and checks integrity.
- [x] Run the end-to-end test and correct only documented/API mismatches until green.

### Task 6: Completion verification

- [x] Run `uv run pytest -q`; require zero failures.
- [x] Build with the declared hatchling backend; require wheel and source distribution success. In this restricted environment hatchling ran from the local uv cache because PyPI access was unavailable.
- [x] Install the wheel into a temporary isolated environment and run `review-kb --help`.
- [x] Run a shell smoke flow against `examples/review-checklist.md`: prepare, rules get, rules search, db backup, db check.
- [x] Compare CLI help with the design command table and document any intentional exclusions. No required command may remain absent.
