## review-kb — cross-implementation parity & test entrypoint.
##
## This repo ships two independent, byte-compatible implementations of the
## `review-kb` CLI: Python (`src/review_kb/`, run via `uv`) and Rust (`rust/`).
## The two runtimes never touch each other's build artifacts (`.venv/` vs
## `rust/target/`); the only coupling is the Rust *compat* integration tests,
## which shell out to `uv run review-kb` to diff Rust output against Python.
##
## Targets:
##   make test      — everything: Python pytest + full Rust suite (default)
##   make test-py   — Python pytest only (49 tests)
##   make test-rs   — Rust `cargo test` only (unit + integration + compat)
##   make compat    — only the cross-binary parity gates (Rust-vs-Python)
##   make build     — build the Rust binary (debug)
##   make clean     — remove Rust build artifacts

.PHONY: test test-py test-rs compat build clean

test: test-py test-rs

test-py:
	uv run pytest

# Cargo MUST run from inside `rust/` so the project-local
# `rust/.cargo/config.toml` sparse-mirror override is picked up — the global
# cargo registry mirror is broken in this environment. See the project memory
# note "cargo-ustc-mirror-gotcha".
test-rs:
	cd rust && cargo test --no-fail-fast

# The cross-binary parity gates: golden markdown corpus, library-level
# repository/service compat, the data-driven CLI stdout/exit-code harness that
# diffs the Rust binary against `uv run review-kb` across all commands, and the
# command-tree guard that fails if one binary grows a command the other lacks.
compat:
	cd rust && cargo test --test cli_parity --test command_tree_parity --test repository_compat --test service_compat --test golden_checklist --test cli_smoke

build:
	cd rust && cargo build

clean:
	rm -rf rust/target
