//! Command-tree parity guard.
//!
//! `cli_parity` only diffs commands that are explicitly listed in its scenario
//! table, so it cannot automatically detect "Python grew a new command (or
//! subcommand) that the Rust twin lacks" — that requires a human to remember to
//! add a scenario. This test closes that gap: it recursively walks each binary's
//! `--help` output (root + every group), builds the full command-path set, and
//! asserts the two binaries expose the *same* tree. Any new command added to
//! one side but not the other fails here.
//!
//! Parsing is intentionally simple: under a `Commands:` heading, each indented
//! line's first token is a command name. clap adds a synthetic `help`
//! subcommand (and Typer does not), so `help` is dropped before comparing. We
//! compare command *paths* (e.g. `db`, `db info`, `rules`, `rules get`) so a
//! command being a group on one side but a leaf on the other is also caught.
//!
//! Requires `uv`; skipped (not failed) if unavailable.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has a parent")
        .to_path_buf()
}

fn python_available() -> bool {
    Command::new("uv")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `--help` (at the given command path) on one binary; return stdout.
fn run_help(is_python: bool, path: &[&str]) -> String {
    let (program, lead): (&str, Vec<String>) = if is_python {
        ("uv", vec!["run".into(), "review-kb".into()])
    } else {
        (env!("CARGO_BIN_EXE_review-kb"), Vec::new())
    };
    let mut cmd = Command::new(program);
    cmd.args(&lead).args(path).arg("--help");
    cmd.current_dir(repo_root());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.spawn().expect("spawn").wait_with_output().expect("wait");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Parse the command names listed under a `Commands:` heading. Returns empty
/// for leaf commands (no subcommands). Drops clap's synthetic `help`.
fn parse_commands(help: &str) -> Vec<String> {
    let mut cmds = Vec::new();
    let mut in_section = false;
    for line in help.lines() {
        let trimmed = line.trim();
        if trimmed == "Commands:" {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        // A blank line or a non-indented line (e.g. "Options:") ends the block.
        if trimmed.is_empty() || (!line.starts_with(' ') && !line.starts_with('\t')) {
            break;
        }
        if let Some(name) = trimmed.split_whitespace().next() {
            if name != "help" {
                cmds.push(name.to_string());
            }
        }
    }
    cmds
}

/// Recursively collect every command path (e.g. `db`, `db info`) visible from
/// `--help` starting at `path`.
fn collect(is_python: bool, path: &[String]) -> BTreeSet<String> {
    let path_refs: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
    let mut set = BTreeSet::new();
    for name in parse_commands(&run_help(is_python, &path_refs)) {
        let mut child = path.to_vec();
        child.push(name);
        set.insert(child.join(" "));
        set.extend(collect(is_python, &child));
    }
    set
}

#[test]
fn both_binaries_expose_the_same_command_tree() {
    if !python_available() {
        eprintln!("skipping: `uv` not available");
        return;
    }
    let py = collect(true, &[]);
    let rs = collect(false, &[]);

    let only_python: Vec<&String> = py.difference(&rs).collect();
    let only_rust: Vec<&String> = rs.difference(&py).collect();
    assert!(
        only_python.is_empty() && only_rust.is_empty(),
        "command tree diverges between Python and Rust binaries\n\
         only in python: {only_python:?}\n\
         only in rust:   {only_rust:?}\n\
         (a new command was added to one implementation but not the other — \
         port it and/or add a cli_parity scenario)"
    );

    // Sanity: the guard itself must observe a non-trivial tree, otherwise a
    // parsing regression could make it pass vacuously.
    assert!(
        rs.contains("db backup") && rs.contains("rules get") && rs.contains("config set"),
        "expected well-known commands missing from parsed tree: {rs:?}"
    );

    eprintln!("command-tree parity ok ({} paths)", rs.len());
}
