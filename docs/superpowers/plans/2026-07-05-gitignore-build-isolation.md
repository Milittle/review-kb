# Git Ignore and Build Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add safe repository ignore rules, prove the distributable is isolated from local files, and publish the complete project through the configured SSH remote.

**Architecture:** A root `.gitignore` defines the version-control boundary without broad patterns that could hide valid fixtures. Existing Hatch packaging remains unchanged unless archive inspection shows a concrete defect. Verification covers tests, both distribution formats, archive contents, and installation into a clean temporary virtual environment.

**Tech Stack:** Git, Python 3.10+, uv, Hatchling, pytest

---

### Task 1: Define the repository boundary

**Files:**
- Create: `.gitignore`

- [ ] **Step 1:** Add targeted patterns for Python caches, virtual environments, test and coverage caches, build outputs, local indexes, environment files, editors, and operating-system metadata.
- [ ] **Step 2:** Run `git status --short --untracked-files=all` and confirm source, tests, migrations, documentation, `pyproject.toml`, and `uv.lock` remain visible while generated local files do not.

### Task 2: Verify build and installation isolation

**Files:**
- Verify: `pyproject.toml`
- Verify: `src/review_kb/migrations/001_initial.sql`
- Verify: `src/review_kb/migrations/002_overrides.sql`

- [ ] **Step 1:** Run `uv run pytest` and require zero failures.
- [ ] **Step 2:** Run `uv build` and require successful wheel and source-distribution creation.
- [ ] **Step 3:** List both archives; require package source and SQL migrations, and reject cache, virtual-environment, local-index, `.env`, and nested `dist/` entries.
- [ ] **Step 4:** Install the wheel into a fresh environment under `/tmp` and require `review-kb --help` to exit successfully.

### Task 3: Commit and publish the complete project

**Files:**
- Add: `.gitignore`, `README.md`, `pyproject.toml`, `uv.lock`, `src/`, `tests/`, `examples/`, `docs/`

- [ ] **Step 1:** Add `git@github.com:Milittle/review-kb.git` as `origin` and verify the URL.
- [ ] **Step 2:** Stage only the explicit project paths and inspect the staged file list for ignored artifacts.
- [ ] **Step 3:** Commit with message `chore: prepare review-kb project`.
- [ ] **Step 4:** Push the current `master` branch to `origin` with upstream tracking using SSH.
