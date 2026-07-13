//! Checklist parser — byte-faithful port of `review_kb/checklist.py`.
//!
//! The hardest parity surface: the parser consumes markdown-it's token stream
//! (h2 headings incl. setext, and `yaml review-rule` fences with their line
//! maps). Rather than depend on a CommonMark crate's event/offset model, we run
//! a targeted line scanner over `markdown.splitlines()` and emit exactly the
//! `(heading_line, key)` and `(fence_open, fence_end, info)` tuples the Python
//! code reads from markdown-it tokens. Parity is proven by the golden corpus
//! (tests/golden), not by argument.

use std::path::Path;

use regex::Regex;
use serde_json::{Map, Value};

use crate::errors::{ErrorCode, ReviewKBError};
use crate::json_util::canonical_hash;
use crate::json_util::sha256_hex;
use crate::models::{Checklist, Rule};

const FILE_FIELDS: &[&str] = &["schema_version", "checklist_version", "global_description"];
const RULE_FIELDS: &[&str] = &["summary", "tags", "paths", "languages"];

fn front_matter_re() -> Regex {
    // (?s)\A---\r?\n(.*?)\r?\n---(?:\r?\n|\z)
    Regex::new(r"(?s)\A---\r?\n(.*?)\r?\n---(?:\r?\n|\z)").expect("front matter regex")
}

fn key_re() -> Regex {
    // fullmatch of [A-Za-z0-9][A-Za-z0-9._-]{0,127}
    Regex::new(r"\A[A-Za-z0-9][A-Za-z0-9._-]{0,127}\z").expect("key regex")
}

fn invalid(message: impl Into<String>, path: &Path, details: Map<String, Value>) -> ReviewKBError {
    let mut d = details;
    d.insert("path".into(), Value::String(path.to_string_lossy().into_owned()));
    ReviewKBError::new(ErrorCode::ChecklistInvalid, message, d)
}

fn invalid_line(message: impl Into<String>, path: &Path, line: usize, details: Map<String, Value>) -> ReviewKBError {
    let mut d = details;
    d.insert("path".into(), Value::String(path.to_string_lossy().into_owned()));
    d.insert("line".into(), Value::Number(line.into()));
    ReviewKBError::new(ErrorCode::ChecklistInvalid, message, d)
}

/// Python `str.splitlines()` — splits on a broader set of boundaries than
/// Rust's `str::lines()` (notably bare `\r`, `\x0b`, `\x0c`, `\x1c-1e`, NEL,
/// LS, PS). Real checklists use `\n`/`\r\n` where all of {splitlines, lines,
/// markdown-it} agree; we mirror splitlines to reproduce Python exactly.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let line_end_len = match bytes[i] {
            b'\n' => Some(1),
            b'\r' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    Some(2)
                } else {
                    Some(1)
                }
            }
            b'\x0b' | b'\x0c' | b'\x1c' | b'\x1d' | b'\x1e' => Some(1),
            _ if s.is_char_boundary(i) => {
                let ch = s[i..].chars().next().unwrap();
                let c = ch as u32;
                if c == 0x85 || c == 0x2028 || c == 0x2029 {
                    Some(ch.len_utf8())
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(len) = line_end_len {
            out.push(&s[start..i]);
            i += len;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

// ---------------------------------------------------------------- YAML bridge

/// Convert a `serde_yml::Value` to a `serde_json::Value`.
///
/// NOTE: PyYAML uses YAML 1.1 resolution (bare `yes/no/on/off` → bool). serde_yml
/// follows YAML 1.2 (those stay strings). For real checklists the values used
/// (quoted strings, block scalars, ints, normal identifiers) resolve identically
/// under both, so plain conversion matches here. The golden corpus will surface
/// any divergent case (then we add 1.1 scalar resolution).
fn yml_to_json(v: serde_yml::Value) -> Value {
    use serde_yml::Value as Y;
    match v {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        Y::String(s) => Value::String(s),
        Y::Sequence(seq) => Value::Array(seq.into_iter().map(yml_to_json).collect()),
        Y::Mapping(map) => {
            let mut m = Map::new();
            for (k, val) in map {
                let key = match k {
                    Y::String(s) => s,
                    Y::Bool(b) => if b { "true".to_string() } else { "false".to_string() },
                    Y::Number(n) => n.to_string(),
                    Y::Null => "~".to_string(),
                    other => yml_to_json(other).to_string(),
                };
                m.insert(key, yml_to_json(val));
            }
            Value::Object(m)
        }
        // YAML explicit tags (e.g. `!!str`) do not occur in checklists; treat
        // any tagged node as null rather than depend on serde_yml's tagged API.
        Y::Tagged(_) => Value::Null,
    }
}

/// Parse a YAML document string into a JSON object (mapping). Mirrors
/// `_yaml_mapping`: YAMLError → CHECKLIST_INVALID; non-mapping → CHECKLIST_INVALID.
fn yaml_mapping(source: &str, label: &str, path: &Path) -> Result<Map<String, Value>, ReviewKBError> {
    let parsed: serde_yml::Value = serde_yml::from_str(source).map_err(|error| {
        invalid(
            format!("invalid YAML in {label}: {error}"),
            path,
            Map::new(),
        )
    })?;
    let value = yml_to_json(parsed);
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(invalid(
            format!("{label} must contain a YAML mapping"),
            path,
            Map::new(),
        )),
    }
}

// ------------------------------------------------------------------- scanner

#[derive(Debug)]
struct ScannedHeading {
    line: usize, // map[0], 0-based
    key: String,
}

#[derive(Debug)]
struct ScannedFence {
    open: usize,   // map[0]
    end: usize,    // map[1] = close + 1 (line after the closing fence)
    info: String,  // trimmed info string
    content: String, // de-indented text between fences (for YAML)
}

#[derive(Default)]
struct Scan {
    headings: Vec<ScannedHeading>,
    fences: Vec<ScannedFence>,
}

/// Count leading ASCII spaces (capped; callers check <= 3 for block starts).
fn leading_spaces(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

/// `(level, content)` if `line` is an ATX heading (1-6), else None. Content is
/// the heading text after the leading `#`s, with leading spaces skipped and the
/// trailing closing `#` sequence stripped (CommonMark / markdown-it).
fn parse_atx(line: &str) -> Option<(u8, String)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let after = &rest[hashes..];
    // Must be followed by a space/tab or end-of-line.
    if !after.is_empty() {
        let next = after.chars().next().unwrap();
        if next != ' ' && next != '\t' {
            return None;
        }
    }
    // Skip leading spaces/tabs.
    let mut content_start = 0usize;
    let bytes = after.as_bytes();
    while content_start < bytes.len() && (bytes[content_start] == b' ' || bytes[content_start] == b'\t') {
        content_start += 1;
    }
    let mut content = &after[content_start..];
    // Strip trailing closing `#` sequence: optional trailing spaces, then `#`s
    // (only if non-empty and preceded by a space / start). Then trim end.
    let trimmed_end = content.trim_end_matches([' ', '\t']);
    content = trimmed_end;
    if !content.is_empty() && content.as_bytes().last() == Some(&b'#') {
        // find the run of trailing '#'
        let mut cut = content.len();
        while cut > 0 && content.as_bytes()[cut - 1] == b'#' {
            cut -= 1;
        }
        // closing sequence requires a space before it (or start)
        let before_ok = cut == 0
            || content.as_bytes().get(cut - 1) == Some(&b' ')
            || content.as_bytes().get(cut - 1) == Some(&b'\t');
        if before_ok {
            content = content[..cut].trim_end_matches([' ', '\t']);
        }
    }
    Some((hashes as u8, content.trim().to_string()))
}

/// `(fence_char, fence_len, info)` if `line` opens a fenced code block.
fn parse_fence_open(line: &str) -> Option<(char, usize, String)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let first = rest.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let fb = first as u8;
    let len = rest.bytes().take_while(|&b| b == fb).count();
    if len < 3 {
        return None;
    }
    // backtick fences can't be closed by info containing backticks (markdown-it
    // treats the whole line as a code span if a stray backtick follows), but for
    // opening detection we accept the info string as the remainder.
    let info_raw = &rest[len..];
    // For backtick fences, markdown-it rejects if the info string contains a
    // backtick (then it's not a fence). Reproduce that.
    if first == '`' && info_raw.contains('`') {
        return None;
    }
    Some((first, len, info_raw.trim().to_string()))
}

/// True if `line` is a closing fence for the given `(char, len)` (indent <= 3,
/// then a run of >= len of the fence char, optionally trailing spaces).
fn is_closing_fence(line: &str, fchar: char, flen: usize) -> bool {
    let indent = leading_spaces(line);
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let mut chars = rest.chars();
    if chars.next() != Some(fchar) {
        return false;
    }
    let count = rest.bytes().take_while(|&b| b == fchar as u8).count();
    if count < flen {
        return false;
    }
    let after = &rest[count..];
    after.trim().is_empty()
}

/// `(level,)` if `line` is a setext underline (1 for `=`, 2 for `-`), else None.
/// Accepts interspersed spaces/tabs; requires >= 1 of the chosen char.
fn parse_setext_underline(line: &str) -> Option<u8> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    if rest.trim().is_empty() {
        return None;
    }
    let mut level: Option<u8> = None;
    for b in rest.bytes() {
        match b {
            b'=' => {
                if level == Some(2) {
                    return None;
                }
                level = Some(1);
            }
            b'-' => {
                if level == Some(1) {
                    return None;
                }
                level = Some(2);
            }
            b' ' | b'\t' => {}
            _ => return None,
        }
    }
    level
}

/// True if `line` is a thematic break (>= 3 of `*`, `-` or `_`, interspersed
/// with optional spaces, nothing else), per CommonMark.
fn is_thematic_break(line: &str) -> bool {
    let indent = leading_spaces(line);
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    if rest.trim().is_empty() {
        return false;
    }
    let mut char_for: Option<u8> = None;
    let mut count = 0usize;
    for b in rest.bytes() {
        match b {
            b'*' | b'-' | b'_' => {
                if let Some(c) = char_for {
                    if c != b {
                        return false;
                    }
                } else {
                    char_for = Some(b);
                }
                count += 1;
            }
            b' ' | b'\t' => {}
            _ => return false,
        }
    }
    count >= 3
}

fn scan_markdown(lines: &[&str]) -> Scan {
    let mut scan = Scan::default();
    let mut fence: Option<(char, usize, usize)> = None; // (char, len, open_index)
    let mut para_start: Option<usize> = None;
    let mut para_lines: Vec<String> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];

        if let Some((fchar, flen, open_index)) = fence {
            if is_closing_fence(line, fchar, flen) {
                // finalize the fence
                let open = open_index;
                let close = i;
                let indent = leading_spaces(lines[open]);
                // de-indented content between fences
                let mut content_lines = Vec::new();
                for cl in &lines[open + 1..close] {
                    let strip = std::cmp::min(indent, leading_spaces(cl));
                    content_lines.push(&cl[strip..]);
                }
                let content = content_lines.join("\n");
                // find info from the opening line
                let info = parse_fence_open(lines[open]).map(|(_, _, i)| i).unwrap_or_default();
                scan.fences.push(ScannedFence {
                    open,
                    end: close + 1,
                    info,
                    content,
                });
                fence = None;
                para_start = None;
            }
            // else: inside fence content; ignore the line.
            i += 1;
            continue;
        }

        // not inside a fence
        if let Some((fchar, flen, info)) = parse_fence_open(line) {
            fence = Some((fchar, flen, i));
            // info captured at finalization; remember nothing else
            let _ = info;
            para_start = None;
            i += 1;
            continue;
        }

        if let Some((level, content)) = parse_atx(line) {
            if level == 2 {
                scan.headings.push(ScannedHeading { line: i, key: content });
            }
            para_start = None;
            i += 1;
            continue;
        }

        if line.trim().is_empty() {
            para_start = None;
            i += 1;
            continue;
        }

        if para_start.is_some() {
            if let Some(level) = parse_setext_underline(line) {
                if level == 2 {
                    let key = para_lines.join(" ");
                    let key = key.trim().to_string();
                    scan.headings.push(ScannedHeading {
                        line: para_start.unwrap(),
                        key,
                    });
                }
                para_start = None;
                i += 1;
                continue;
            }
            if is_thematic_break(line) {
                para_start = None;
                i += 1;
                continue;
            }
            // paragraph continuation
            para_lines.push(line.trim().to_string());
            i += 1;
            continue;
        }

        // no active paragraph
        if is_thematic_break(line) {
            i += 1;
            continue;
        }
        // start a new paragraph
        para_start = Some(i);
        para_lines.clear();
        para_lines.push(line.trim().to_string());
        i += 1;
    }

    // Unclosed fence runs to EOF: markdown-it treats the rest as fenced content
    // (map = [open, lines.len()]). It won't match a closing fence, so it has no
    // `end`; we record it ending at EOF.
    if let Some((fchar, flen, open_index)) = fence {
        let open = open_index;
        let indent = leading_spaces(lines[open]);
        let mut content_lines = Vec::new();
        for cl in &lines[open + 1..] {
            let strip = std::cmp::min(indent, leading_spaces(cl));
            content_lines.push(&cl[strip..]);
        }
        let content = content_lines.join("\n");
        let info = parse_fence_open(lines[open]).map(|(_, _, i)| i).unwrap_or_default();
        scan.fences.push(ScannedFence {
            open,
            end: lines.len(),
            info,
            content,
        });
        let _ = (fchar, flen);
    }

    scan
}

// --------------------------------------------------------------- string lists

/// Mirrors `_string_list`: None → []; must be a list of non-empty (after strip)
/// strings; dedup preserving first-seen order (exact equality).
fn string_list(value: Option<&Value>, field: &str, path: &Path) -> Result<Vec<String>, ReviewKBError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let arr = match value {
        Value::Array(a) => a,
        _ => return Err(invalid(format!("{field} must be an array of strings"), path, Map::new())),
    };
    let mut result: Vec<String> = Vec::new();
    for item in arr {
        let s = match item {
            Value::String(s) => s,
            _ => return Err(invalid(format!("{field} must be an array of strings"), path, Map::new())),
        };
        if s.trim().is_empty() {
            return Err(invalid(format!("{field} entries must not be empty"), path, Map::new()));
        }
        if !result.iter().any(|existing| existing == s) {
            result.push(s.clone());
        }
    }
    Ok(result)
}

// -------------------------------------------------------------------- repr

/// Approximate Python `str` repr (single-quoted). Faithful for printable ASCII;
/// diverges from Python's quote-preference rule for strings containing `'`.
fn py_repr(s: &str) -> String {
    let mut out = String::from("'");
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

// --------------------------------------------------------------- parse entry

/// Parse a checklist file into a `Checklist`. Mirrors `parse_checklist`.
pub fn parse_checklist(path: impl AsRef<Path>) -> Result<Checklist, ReviewKBError> {
    let path = path.as_ref();
    let raw = std::fs::read(path).map_err(|_| {
        ReviewKBError::new(
            ErrorCode::ChecklistNotFound,
            format!("checklist not found: {}", path.display()),
            crate::details! { "path" => Value::String(path.to_string_lossy().into_owned()) },
        )
    })?;
    let source = std::str::from_utf8(&raw).map_err(|_| {
        invalid("checklist must be valid UTF-8", path, Map::new())
    })?;

    let fm = front_matter_re();
    let caps = fm.captures(source).ok_or_else(|| {
        invalid("checklist must start with YAML Front Matter", path, Map::new())
    })?;
    let fm_body = caps.get(1).unwrap().as_str();
    let body_offset = caps.get(0).unwrap().end();
    let mut file_meta = yaml_mapping(fm_body, "Front Matter", path)?;

    let unknown_file_fields: Vec<&str> = file_meta
        .keys()
        .filter(|k| !FILE_FIELDS.contains(&k.as_str()))
        .map(String::as_str)
        .collect();
    // Python sorts the set of unknown fields for a stable message.
    let mut unknown_sorted: Vec<&str> = unknown_file_fields.clone();
    unknown_sorted.sort_unstable();
    if !unknown_sorted.is_empty() {
        return Err(invalid(
            format!("unknown Front Matter fields: {}", unknown_sorted.join(", ")),
            path,
            Map::new(),
        ));
    }

    let schema_version = file_meta.remove("schema_version");
    let schema_ok = matches!(&schema_version, Some(Value::Number(n)) if n.as_i64() == Some(1));
    if !schema_ok {
        return Err(invalid("schema_version must be 1", path, Map::new()));
    }

    let checklist_version = match file_meta.remove("checklist_version") {
        Some(Value::String(s)) if !s.trim().is_empty() => s,
        _ => return Err(invalid("checklist_version must be a non-empty string", path, Map::new())),
    };
    let global_description = match file_meta.remove("global_description") {
        Some(Value::String(s)) if !s.trim().is_empty() => s,
        _ => return Err(invalid("global_description must be a non-empty string", path, Map::new())),
    };

    let markdown = &source[body_offset..];
    let lines = py_splitlines(markdown);
    let scan = scan_markdown(&lines);

    if scan.headings.is_empty() {
        return Err(invalid(
            "checklist must contain at least one level-two rule",
            path,
            Map::new(),
        ));
    }

    let key_regex = key_re();
    let mut rules: Vec<Rule> = Vec::new();
    let mut seen_keys: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for (position, heading) in scan.headings.iter().enumerate() {
        let heading_line = heading.line;
        let key = &heading.key;
        if key_regex.is_match(key) {
            // ok
        } else {
            return Err(invalid_line(
                format!("invalid rule key: {}", py_repr(key)),
                path,
                heading_line + 1,
                Map::new(),
            ));
        }
        let folded = key.to_lowercase(); // casefold == lowercase for [A-Za-z0-9._-]
        if let Some(existing) = seen_keys.get(&folded) {
            return Err(invalid_line(
                format!("duplicate rule key: {key} conflicts with {existing}"),
                path,
                heading_line + 1,
                Map::new(),
            ));
        }
        seen_keys.insert(folded, key.clone());

        let end_line = if position + 1 < scan.headings.len() {
            scan.headings[position + 1].line
        } else {
            lines.len()
        };

        // fences in this section with the right info string
        let section_fences: Vec<&ScannedFence> = scan
            .fences
            .iter()
            .filter(|f| heading_line < f.open && f.open < end_line && f.info == "yaml review-rule")
            .collect();
        if section_fences.len() != 1 {
            return Err(invalid_line(
                format!(
                    "rule {key} must contain exactly one `yaml review-rule` metadata block"
                ),
                path,
                heading_line + 1,
                Map::new(),
            ));
        }
        let fence = section_fences[0];

        // metadata block must appear before content: between H2 line and the
        // opening fence, every line must be blank.
        for between in &lines[heading_line + 1..fence.open] {
            if !between.trim().is_empty() {
                return Err(invalid_line(
                    format!("rule {key} metadata block must appear before rule content"),
                    path,
                    heading_line + 1,
                    Map::new(),
                ));
            }
        }

        let mut rule_meta = yaml_mapping(&fence.content, &format!("rule {key} metadata"), path)?;
        let unknown_rule_fields: Vec<String> = rule_meta
            .keys()
            .filter(|k| !RULE_FIELDS.contains(&k.as_str()))
            .cloned()
            .collect();
        let mut unknown_rule_sorted = unknown_rule_fields.clone();
        unknown_rule_sorted.sort();
        if !unknown_rule_sorted.is_empty() {
            return Err(invalid_line(
                format!("unknown fields for rule {key}: {}", unknown_rule_sorted.join(", ")),
                path,
                fence.open + 1,
                Map::new(),
            ));
        }

        let summary = match rule_meta.remove("summary") {
            Some(Value::String(s)) if !s.trim().is_empty() && !s.contains('\n') => s,
            _ => {
                return Err(invalid(
                    format!("rule {key} summary must be a non-empty single line"),
                    path,
                    Map::new(),
                ))
            }
        };

        let tags = string_list(rule_meta.get("tags"), "tags", path)?;
        let paths = string_list(rule_meta.get("paths"), "paths", path)?;
        let languages = string_list(rule_meta.get("languages"), "languages", path)?;
        if languages.iter().any(|l| l.as_str() != l.to_lowercase().as_str()) {
            return Err(invalid(
                format!("rule {key} languages must be lowercase"),
                path,
                Map::new(),
            ));
        }

        let content = lines[fence.end..end_line].join("\n");
        let content = content.trim().to_string();
        if content.is_empty() {
            return Err(invalid(
                format!("rule {key} content must not be empty"),
                path,
                Map::new(),
            ));
        }

        let summary_stripped = summary.trim().to_string();
        // effective dict for source_rule_hash (canonical JSON of
        // {content, key, languages, paths, summary, tags}).
        let mut effective = Map::new();
        effective.insert("key".into(), Value::String(key.clone()));
        effective.insert("summary".into(), Value::String(summary_stripped.clone()));
        effective.insert("content".into(), Value::String(content.clone()));
        effective.insert("tags".into(), Value::Array(tags.iter().map(|s| Value::String(s.clone())).collect()));
        effective.insert("paths".into(), Value::Array(paths.iter().map(|s| Value::String(s.clone())).collect()));
        effective.insert(
            "languages".into(),
            Value::Array(languages.iter().map(|s| Value::String(s.clone())).collect()),
        );
        let source_rule_hash = canonical_hash(&Value::Object(effective));

        rules.push(Rule {
            key: key.clone(),
            summary: summary_stripped,
            content,
            tags,
            paths,
            languages,
            source_rule_hash,
        });
    }

    let content_hash = sha256_hex(&raw);
    Ok(Checklist {
        schema_version: 1,
        checklist_version: checklist_version.trim().to_string(),
        global_description: global_description.trim().to_string(),
        content_hash,
        rules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden parity (go/no-go gate). `source_rule_hash` is the sha256 of the
    /// canonical JSON of {key, summary, content, tags, paths, languages}, so a
    /// match proves the scanner extracted every rule field byte-for-byte. Values
    /// captured from the Python `parse_checklist`.
    #[test]
    fn example_checklist_matches_python_hashes() {
        let cl = parse_checklist("../examples/review-checklist.md").expect("example parses");
        assert_eq!(cl.schema_version, 1);
        assert_eq!(cl.checklist_version, "2026.07.1");
        assert_eq!(cl.content_hash, "sha256:3771e811688140df1bcc45a6cfe213a3c1f4b255ea6292fadb6b96f87b4857df");
        assert_eq!(cl.rules.len(), 2);
        assert_eq!(cl.rules[0].key, "SEC-001");
        assert_eq!(
            cl.rules[0].source_rule_hash,
            "sha256:741c3dd771b85a3c185192e40c01506a79be54f83a6fe8aa572615541524e06f"
        );
        assert_eq!(cl.rules[1].key, "DB-004");
        assert_eq!(
            cl.rules[1].source_rule_hash,
            "sha256:5cd3b7bd02ede797a07658022e090207e8fccbc9f3c80e165be4f080a5ff2a29"
        );
    }

    #[test]
    fn valid_fixture_matches_python_hashes() {
        let cl = parse_checklist("../tests/fixtures/valid-checklist.md").expect("fixture parses");
        assert_eq!(cl.content_hash, "sha256:47c9a24f5dc7f0a76aa56319d304f488e47d36c49af6983f697a78dfa02384d1");
        assert_eq!(cl.rules.len(), 2);
        assert_eq!(
            cl.rules[0].source_rule_hash,
            "sha256:350d095551551337b6e53c8d6e4dcb1f5c7c775855e6eeac620996e292d403c9"
        );
        // DB-004 in the fixture has paths: [] and a single-line content (no H3).
        assert_eq!(cl.rules[1].paths, Vec::<String>::new());
        assert_eq!(
            cl.rules[1].source_rule_hash,
            "sha256:b41b01455da655a112bca9cf6364eb121cf38a527a0629730e299504d81b0d04"
        );
    }
}
