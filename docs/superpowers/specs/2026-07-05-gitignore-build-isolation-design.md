# Git Ignore and Build Isolation Design

## Goal

Keep generated, machine-local, and sensitive files out of version control without excluding any source, migration, test fixture, documentation, lock file, or packaging metadata required to build and install `review-kb`.

## Ignore policy

Add a root `.gitignore` covering:

- Python bytecode and package metadata;
- virtual environments;
- pytest, type-checker, linter, and coverage caches;
- build outputs;
- the local `.code-review-graph` index;
- local environment files;
- common editor and operating-system metadata.

Avoid broad database rules such as `*.db`, because future committed database fixtures may be valid project inputs. Keep `uv.lock`, migrations, test fixtures, documentation, and all package source files visible to Git.

## Verification

1. Run the complete pytest suite.
2. Build both wheel and source distribution with `uv build`.
3. Inspect both archives and confirm they contain required package files, including SQL migrations, while excluding caches, virtual environments, local indexes, environment files, and prior build outputs.
4. Create an isolated environment under `/tmp`, install the wheel, and execute `review-kb --help`.
5. Check Git status to confirm ignored files no longer appear as candidates for submission.

No source-code or packaging-configuration change is planned unless verification exposes a concrete packaging defect.
