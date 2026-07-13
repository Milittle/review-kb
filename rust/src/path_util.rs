//! Path helpers matching Python's `os.path.expanduser` and `Path.resolve`
//! (non-strict) for the cases review-kb uses.

use std::path::{Path, PathBuf};

/// Expand a leading `~/` (or bare `~`) using `$HOME`. Mirrors Python's
/// `os.path.expanduser` for the `~` case on Unix (`~user` is not supported —
/// irrelevant for config/db paths).
pub fn expanduser(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Ok(home) = std::env::var("HOME") {
        if s == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = s.strip_prefix("~/") {
            return PathBuf::from(format!("{home}/{rest}"));
        }
    }
    path.to_path_buf()
}

/// Same as `expanduser` but taking/returning a string.
pub fn expanduser_str(s: &str) -> String {
    expanduser(Path::new(s)).to_string_lossy().into_owned()
}

/// Lexical `Path.resolve(strict=False)`: make absolute via cwd, then normalize
/// `.` and `..` components. Matches Python for non-symlinked paths (the temp
/// dirs tests use). Symlinked components would diverge from Python's iterative
/// resolution — not exercised by the test suite.
pub fn lexpath_resolve(path: &Path) -> PathBuf {
    let expanded = expanduser(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        cwd.join(&expanded)
    };
    normalize_lexical(&absolute)
}

fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // pop a Normal if present; at root, `..` stays root (Unix)
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.iter().any(|c| {
                    matches!(c, Component::RootDir | Component::Prefix(_))
                }) {
                    out.push(comp);
                }
            }
            other => out.push(other),
        }
    }
    let mut result = PathBuf::new();
    for c in out {
        result.push(c.as_os_str());
    }
    if result.as_os_str().is_empty() {
        result.push("/");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_dots() {
        assert_eq!(lexpath_resolve(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(lexpath_resolve(Path::new("/a/b/./c")), PathBuf::from("/a/b/c"));
        // `..` at root stays root (Unix semantics)
        assert_eq!(lexpath_resolve(Path::new("/a/../../b")), PathBuf::from("/b"));
        assert_eq!(lexpath_resolve(Path::new("/a/b/c/../..")), PathBuf::from("/a"));
    }

    #[test]
    fn expanduser_basic() {
        std::env::set_var("HOME", "/root");
        assert_eq!(expanduser_str("~/x"), "/root/x");
        assert_eq!(expanduser_str("~"), "/root");
        assert_eq!(expanduser_str("/abs/x"), "/abs/x");
    }
}
