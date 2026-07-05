# Code Review Knowledge CLI Overrides Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add validated, auditable emergency rule overrides without silently losing changes during checklist refreshes.

**Architecture:** Source rules remain immutable imports in `rules`; sparse field overrides live in a separate table. Services merge active overrides when generating descriptions and returning rule bodies. Refresh compares each override's base source-rule hash with the incoming source and aborts with an explicit conflict before replacing data.

**Tech Stack:** Existing Python, Pydantic, sqlite3, Typer, pytest stack.

---

### Task 1: Override persistence

**Files:**
- Create: `src/review_kb/migrations/002_overrides.sql`
- Modify: `src/review_kb/repository.py`
- Create: `tests/test_overrides.py`

- [ ] Write a failing repository test that sets a sparse content override with a required reason, lists it as active, disables it, and verifies the source rule row never changes.
- [ ] Run `uv run pytest tests/test_overrides.py -q`; expect failure because override methods do not exist.
- [ ] Add migration tables `rule_overrides` and `audit_log`. Override identity is `(project_id, rule_key)`; nullable override fields distinguish untouched values; base source hash and `active/conflict/disabled` status are mandatory.
- [ ] Implement repository methods `upsert_override`, `get_override`, `list_overrides`, `disable_override`, `mark_override_conflicts`, and `update_effective_description`. Every mutation writes an audit row in the same transaction.
- [ ] Run the repository tests; expect all to pass.

### Task 2: Effective rules and refresh conflict safety

**Files:**
- Modify: `src/review_kb/models.py`
- Modify: `src/review_kb/service.py`
- Modify: `tests/test_overrides.py`

- [ ] Write failing service tests proving: an override changes description summary and returned content; knowledge revision changes; unset restores source; changing the overridden source rule causes `OVERRIDE_CONFLICT` and leaves the old project snapshot intact.
- [ ] Run the focused tests and confirm failures are caused by missing service behavior.
- [ ] Add `OverrideInput` validation and service methods `set_override`, `list_overrides`, `show_override`, and `unset_override`. Apply sparse override fields to source rules before calling the existing deterministic description builder.
- [ ] Before refresh, compare incoming rule hashes with active override base hashes. Mark mismatches conflict, abort with key details, and do not call `replace_project`. Unchanged overridden rules survive unrelated checklist updates.
- [ ] Make `get_selected_rules` apply active overrides to returned rule bodies.
- [ ] Run override, selection, service, and description tests; expect all to pass.

### Task 3: JSON CLI and integration documentation

**Files:**
- Modify: `src/review_kb/cli.py`
- Modify: `tests/test_cli.py`
- Modify: `docs/integration-guide.md`

- [ ] Write failing CLI tests for `overrides set --input -`, `overrides list/show`, and `overrides unset --project-id --key`; assert one JSON output document and stable conflict error code `4`.
- [ ] Add the `overrides` command group. Set input is `{project_id,key,reason,summary?,content?,tags?,paths?,languages?}`; at least one override field is required. No arbitrary SQL writes are introduced.
- [ ] Extend the tutorial with emergency repair, auditability, revision invalidation, unset, and source-change conflict recovery.
- [ ] Run `uv run pytest -q`, CLI help, and a prepare → override → rules-get smoke flow; all must pass.

## Completion Gate

- Source rows remain unchanged by overrides.
- Effective description and fetched bodies reflect active overrides and share one revision.
- Any existing Agent selection becomes stale after override changes.
- A source change to an overridden rule cannot silently overwrite or preserve an unreviewed override.
- All write failures remain atomic and machine-readable.

