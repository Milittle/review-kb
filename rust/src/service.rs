//! Knowledge service — byte-faithful port of `review_kb/service.py`.
//!
//! Orchestrates the repository, checklist parser, and description builder into
//! the command-level operations (`prepare`/`sync`/`rebuild`/`status`/rules/
//! overrides/description). All return values are built as `serde_json::Value`
//! (never derived `Serialize`) so the CLI's `canonical_json` controls key order.
//!
//! Error model (preserves Python's two exit paths):
//! - `ServiceError::Kb(ReviewKBError)` → the JSON failure envelope + `exit(code)`.
//! - `ServiceError::Raw(_)` → the raw-escape path: stderr + `exit(1)`, **empty
//!   stdout**. This matches Python's uncaught exceptions: raw sqlite/IO errors
//!   bubbling up from the repository writers, and the selection `value_error`
//!   case whose non-serializable `ctx` crashes `canonical_json` at emit time
//!   (see the `selection-value-error-contract` memory). The carried message is
//!   for diagnostics only — stderr text is not part of the byte contract.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::checklist::parse_checklist;
use crate::description::{build_description, knowledge_revision_of};
use crate::details;
use crate::errors::{ErrorCode, ReviewKBError};
use crate::models::{validate_selection, Checklist, Rule, Selection, SelectionError};
use crate::path_util::{expanduser, lexpath_resolve};
use crate::repository::{Repository, RepositoryError};
use crate::suggestions::suggest_keys;

/// Service-layer error.
///
/// `Kb` flows through the JSON envelope; `Raw` is the stderr-and-exit-1 escape
/// path (empty stdout). See the module docs for the parity rationale.
#[derive(Debug)]
pub enum ServiceError {
    Kb(ReviewKBError),
    Raw(String),
}

impl From<ReviewKBError> for ServiceError {
    fn from(e: ReviewKBError) -> Self {
        ServiceError::Kb(e)
    }
}

impl From<RepositoryError> for ServiceError {
    fn from(e: RepositoryError) -> Self {
        match e {
            RepositoryError::Kb(e) => ServiceError::Kb(e),
            // Python lets these propagate as uncaught exceptions (traceback,
            // exit 1, empty stdout). We surface them on the raw-escape path.
            RepositoryError::Sqlite(e) => ServiceError::Raw(format!("{e}")),
            RepositoryError::Io(e) => ServiceError::Raw(format!("{e}")),
        }
    }
}

/// The knowledge service. Borrows the repository for its lifetime, mirroring
/// the Python `KnowledgeService(repository)` which holds a reference. `pub
/// repository` so the CLI's `db info`/`check`/`query`/`migrate` commands reach
/// through it the way Python's `service.repository` does.
pub struct KnowledgeService<'a> {
    pub repository: &'a Repository,
}

impl<'a> KnowledgeService<'a> {
    pub fn new(repository: &'a Repository) -> Self {
        Self { repository }
    }

    /// `value if value.strip() else INVALID_ARGUMENT`. Returns the original
    /// (untrimmed) value, matching Python's `_required`.
    fn required(value: &str, name: &str) -> Result<String, ServiceError> {
        if value.trim().is_empty() {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                format!("{name} must not be empty"),
                details!("field" => Value::String(name.into())),
            )
            .into());
        }
        Ok(value.to_string())
    }

    /// Resolve a project row or raise `PROJECT_NOT_FOUND`. Mirrors `_project`.
    fn project(&self, project_id: &str) -> Result<Value, ServiceError> {
        match self.repository.get_project(project_id)? {
            Some(project) => Ok(project),
            None => Err(ReviewKBError::new(
                ErrorCode::ProjectNotFound,
                format!("project not found: {project_id}"),
                details!("project_id" => Value::String(project_id.into())),
            )
            .into()),
        }
    }

    /// `prepare` / `sync` / `rebuild` share this core (force=False/True).
    /// Mirrors `KnowledgeService.prepare`.
    pub fn prepare(
        &self,
        project_id: &str,
        project_name: &str,
        checklist_path: &str,
        force: bool,
    ) -> Result<Value, ServiceError> {
        let project_id = Self::required(project_id, "project_id")?;
        let project_name = Self::required(project_name, "project_name")?;
        // Python: str(Path(checklist_path).expanduser().resolve()).
        let resolved = lexpath_resolve(&expanduser(Path::new(checklist_path)));
        let path = resolved.to_string_lossy().into_owned();
        let source_checklist = parse_checklist(&path)?;
        let current = self.repository.get_project(&project_id)?;
        let cur = current.as_ref();
        let mut warnings: Vec<String> = Vec::new();

        let source_changed = cur.is_some_and(|c| {
            get_str(c, "checklist_version") != Some(source_checklist.checklist_version.as_str())
                || get_str(c, "content_hash") != Some(source_checklist.content_hash.as_str())
        });

        // Conflict gate: only when an existing project is being refreshed.
        if cur.is_some() && (source_changed || force) {
            let incoming_hashes: HashMap<String, String> = source_checklist
                .rules
                .iter()
                .map(|r| (r.key.clone(), r.source_rule_hash.clone()))
                .collect();
            let mut conflicts: Vec<String> = Vec::new();
            let statuses = vec!["active".to_string(), "conflict".to_string()];
            for override_row in self.repository.list_overrides(&project_id, Some(&statuses))? {
                let status = get_str(&override_row, "status").unwrap_or("");
                let rule_key = get_str(&override_row, "rule_key").unwrap_or("");
                let base = get_str(&override_row, "base_source_rule_hash");
                if status == "conflict" || incoming_hashes.get(rule_key).map(|s| s.as_str()) != base
                {
                    conflicts.push(rule_key.to_string());
                }
            }
            if !conflicts.is_empty() {
                self.repository.mark_override_conflicts(&project_id, &conflicts)?;
                return Err(ReviewKBError::new(
                    ErrorCode::OverrideConflict,
                    "source rules changed while local overrides are active",
                    details!(
                        "project_id" => Value::String(project_id.clone()),
                        "keys" => Value::Array(
                            conflicts.into_iter().map(Value::String).collect()
                        )
                    ),
                )
                .into());
            }
        }

        // Effective checklist = source + active overrides (only if a project
        // already exists; a fresh create has no overrides yet).
        let active_overrides = if cur.is_some() {
            let statuses = vec!["active".to_string()];
            self.repository.list_overrides(&project_id, Some(&statuses))?
        } else {
            Vec::new()
        };
        let effective_checklist = apply_overrides(&source_checklist, &active_overrides);
        let description = build_description(&project_id, &project_name, &effective_checklist);

        let status;
        match cur {
            None => {
                status = "created";
                self.repository.replace_project(
                    &project_id,
                    &project_name,
                    &path,
                    &source_checklist,
                    &description,
                    "create",
                    &[],
                )?;
            }
            Some(c) if source_changed || force => {
                status = if force { "rebuilt" } else { "refreshed" };
                if get_str(c, "checklist_version")
                    == Some(source_checklist.checklist_version.as_str())
                    && get_str(c, "content_hash") != Some(source_checklist.content_hash.as_str())
                {
                    warnings.push("CONTENT_CHANGED_WITHOUT_VERSION_BUMP".to_string());
                }
                let action = if force { "rebuild" } else { "refresh" };
                self.repository.replace_project(
                    &project_id,
                    &project_name,
                    &path,
                    &source_checklist,
                    &description,
                    action,
                    &warnings,
                )?;
            }
            Some(c) => {
                status = "reused";
                if get_str(c, "project_name") != Some(project_name.as_str())
                    || get_str(c, "checklist_path") != Some(path.as_str())
                {
                    warnings.push("PROJECT_METADATA_UPDATED".to_string());
                    self.repository.update_project_metadata(
                        &project_id,
                        &project_name,
                        &path,
                        &description,
                    )?;
                }
            }
        }

        let revision = knowledge_revision_of(&description)
            .expect("build_description always sets checklist.knowledge_revision")
            .to_string();
        Ok(json!({
            "description": description,
            "selection_context": {
                "project_id": project_id,
                "knowledge_revision": revision,
            },
            "knowledge_status": status,
            "checklist_version": source_checklist.checklist_version,
            "content_hash": source_checklist.content_hash,
            "rule_count": source_checklist.rules.len(),
            "warnings": warnings,
        }))
    }

    /// `sync` = `prepare` without force. Mirrors `KnowledgeService.sync`.
    pub fn sync(
        &self,
        project_id: &str,
        project_name: &str,
        checklist_path: &str,
    ) -> Result<Value, ServiceError> {
        self.prepare(project_id, project_name, checklist_path, false)
    }

    /// `rebuild` = `prepare` with force. Mirrors `KnowledgeService.rebuild`.
    pub fn rebuild(
        &self,
        project_id: &str,
        project_name: &str,
        checklist_path: &str,
    ) -> Result<Value, ServiceError> {
        self.prepare(project_id, project_name, checklist_path, true)
    }

    /// Mirrors `KnowledgeService.status`. Note: `status` parses the RAW
    /// checklist path (no resolve), unlike `prepare` — a Python quirk mirrored
    /// exactly. Output values are path-independent (content_hash, version).
    pub fn status(&self, project_id: &str, checklist_path: &str) -> Result<Value, ServiceError> {
        let project_id = Self::required(project_id, "project_id")?;
        let checklist = parse_checklist(checklist_path)?;
        let current = self.repository.get_project(&project_id)?;
        let mut conflict_keys: Vec<String> = Vec::new();

        let status;
        match &current {
            None => status = "missing",
            Some(c) => {
                let incoming_hashes: HashMap<String, String> = checklist
                    .rules
                    .iter()
                    .map(|r| (r.key.clone(), r.source_rule_hash.clone()))
                    .collect();
                let statuses = vec!["active".to_string(), "conflict".to_string()];
                for o in self.repository.list_overrides(&project_id, Some(&statuses))? {
                    let st = get_str(&o, "status").unwrap_or("");
                    let rk = get_str(&o, "rule_key").unwrap_or("");
                    let base = get_str(&o, "base_source_rule_hash");
                    if st == "conflict" || incoming_hashes.get(rk).map(|s| s.as_str()) != base {
                        conflict_keys.push(rk.to_string());
                    }
                }
                if !conflict_keys.is_empty() {
                    status = "conflict";
                } else if get_str(c, "checklist_version")
                    == Some(checklist.checklist_version.as_str())
                    && get_str(c, "content_hash") == Some(checklist.content_hash.as_str())
                {
                    status = "current";
                } else {
                    status = "stale";
                }
            }
        }

        let stored_version = current
            .as_ref()
            .and_then(|c| c.get("checklist_version").cloned())
            .unwrap_or(Value::Null);
        let stored_hash = current
            .as_ref()
            .and_then(|c| c.get("content_hash").cloned())
            .unwrap_or(Value::Null);

        Ok(json!({
            "project_id": project_id,
            "status": status,
            "source_version": checklist.checklist_version,
            "source_hash": checklist.content_hash,
            "stored_version": stored_version,
            "stored_hash": stored_hash,
            "conflict_keys": conflict_keys,
        }))
    }

    /// Mirrors `show_project`: project row with `description_json` removed.
    pub fn show_project(&self, project_id: &str) -> Result<Value, ServiceError> {
        let mut project = self.project(project_id)?;
        if let Value::Object(m) = &mut project {
            m.remove("description_json");
        }
        Ok(project)
    }

    /// Mirrors `list_projects`: each row with `description_json` removed.
    pub fn list_projects(&self) -> Result<Vec<Value>, ServiceError> {
        let mut projects = self.repository.list_projects()?;
        for p in &mut projects {
            if let Value::Object(m) = p {
                m.remove("description_json");
            }
        }
        Ok(projects)
    }

    /// Mirrors `get_description`: `json.loads(project["description_json"])`.
    pub fn get_description(&self, project_id: &str) -> Result<Value, ServiceError> {
        let project = self.project(project_id)?;
        let raw = get_str(&project, "description_json").unwrap_or("");
        // Python: json.loads(...) — a corrupt value raises (uncaught → exit 1).
        serde_json::from_str(raw)
            .map_err(|e| ServiceError::Raw(format!("invalid description_json: {e}")))
    }

    /// Mirrors `list_rules`: rules with `content` and `source_rule_hash`
    /// stripped (the summary view served to clients).
    pub fn list_rules(&self, project_id: &str) -> Result<Vec<Value>, ServiceError> {
        self.project(project_id)?;
        let mut result = self.repository.list_rules(project_id)?;
        for rule in &mut result {
            if let Value::Object(m) = rule {
                m.remove("content");
                m.remove("source_rule_hash");
            }
        }
        Ok(result)
    }

    /// Mirrors `search_rules`: empty query → `INVALID_ARGUMENT`.
    pub fn search_rules(&self, project_id: &str, query: &str) -> Result<Vec<Value>, ServiceError> {
        self.project(project_id)?;
        if query.is_empty() {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "query must not be empty",
                details!("field" => Value::String("query".into())),
            )
            .into());
        }
        Ok(self.repository.search_rules(project_id, query)?)
    }

    /// Mirrors `get_selected_rules`. Validation splits two ways:
    /// - clean errors (`Envelope`) → `INVALID_SELECTION` envelope (exit 2);
    /// - any `value_error` (`Raw`) → the raw-escape path (exit 1, empty stdout),
    ///   matching Python's `canonical_json` crash on the non-serializable ctx.
    pub fn get_selected_rules(&self, payload: &Value) -> Result<Value, ServiceError> {
        let selection = match validate_selection(payload) {
            Ok(s) => s,
            Err(SelectionError::Envelope(errors)) => {
                return Err(ReviewKBError::new(
                    ErrorCode::InvalidSelection,
                    "invalid rule selection",
                    details!("validation_errors" => Value::Array(errors)),
                )
                .into());
            }
            Err(SelectionError::Raw) => {
                return Err(ServiceError::Raw(
                    "selection payload failed value validation".to_string(),
                ));
            }
        };
        // Read-only snapshot scope (Python wraps _get_selected_rules_from_snapshot
        // in `with self.repository.read_transaction()`).
        let _tx = self.repository.read_transaction()?;
        self.selected_rules_from_snapshot(&selection)
    }

    /// Mirrors `_get_selected_rules_from_snapshot`.
    fn selected_rules_from_snapshot(
        &self,
        selection: &Selection,
    ) -> Result<Value, ServiceError> {
        let project = self.project(&selection.project_id)?;
        let current_revision = get_str(&project, "knowledge_revision").unwrap_or("");
        if current_revision != selection.knowledge_revision.as_str() {
            return Err(ReviewKBError::new(
                ErrorCode::KnowledgeRevisionMismatch,
                "knowledge revision changed; run prepare and select rules again",
                details!(
                    "project_id" => Value::String(selection.project_id.clone()),
                    "requested_revision" => Value::String(selection.knowledge_revision.clone()),
                    "current_revision" => Value::String(current_revision.to_string())
                ),
            )
            .into());
        }

        let available_rules = self.repository.list_rules(&selection.project_id)?;
        let available_keys: Vec<String> = available_rules
            .iter()
            .filter_map(|r| get_str(r, "key").map(String::from))
            .collect();
        let available_set: HashSet<&str> =
            available_keys.iter().map(|s| s.as_str()).collect();
        let not_found: Vec<String> = selection
            .keys
            .iter()
            .filter(|k| !available_set.contains(k.as_str()))
            .cloned()
            .collect();
        if !not_found.is_empty() {
            let suggestions: Map<String, Value> = not_found
                .iter()
                .map(|key| {
                    let hits = suggest_keys(key, &available_keys, 3);
                    (
                        key.clone(),
                        Value::Array(hits.into_iter().map(Value::String).collect()),
                    )
                })
                .collect();
            return Err(ReviewKBError::new(
                ErrorCode::RuleNotFound,
                "one or more selected rule keys do not exist",
                details!(
                    "not_found" => Value::Array(
                        not_found.iter().cloned().map(Value::String).collect()
                    ),
                    "suggestions" => Value::Object(suggestions)
                ),
            )
            .into());
        }

        let mut selected_rules = self
            .repository
            .get_rules(&selection.project_id, &selection.keys)?;
        let active_statuses = vec!["active".to_string()];
        let overrides: Map<String, Value> = self
            .repository
            .list_overrides(&selection.project_id, Some(&active_statuses))?
            .into_iter()
            .filter_map(|o| {
                let key = get_str(&o, "rule_key")?.to_string();
                Some((key, o))
            })
            .collect();
        for rule in &mut selected_rules {
            let key = match get_str(rule, "key").map(String::from) {
                Some(k) => k,
                None => continue,
            };
            let override_row = match overrides.get(&key) {
                Some(o) => o,
                None => continue,
            };
            if let Value::Object(m) = rule {
                for field in ["summary", "content", "tags", "paths", "languages"] {
                    if let Some(val) = override_row.get(field) {
                        if !val.is_null() {
                            m.insert(field.to_string(), val.clone());
                        }
                    }
                }
            }
        }

        Ok(json!({
            "project_id": selection.project_id,
            "knowledge_revision": selection.knowledge_revision,
            "rules": selected_rules,
        }))
    }

    /// Mirrors `_source_checklist_from_database`: rebuild the source `Checklist`
    /// from the stored project + rule rows (rules carry content + source_hash).
    fn source_checklist_from_database(
        &self,
        project_id: &str,
    ) -> Result<Checklist, ServiceError> {
        let project = self.project(project_id)?;
        let stored_description = {
            let raw = get_str(&project, "description_json").unwrap_or("");
            serde_json::from_str::<Value>(raw)
                .map_err(|e| ServiceError::Raw(format!("invalid description_json: {e}")))?
        };
        let global_description = get_str(&stored_description, "global_description")
            .unwrap_or("")
            .to_string();
        let schema_version = project
            .get("schema_version")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let checklist_version = get_str(&project, "checklist_version")
            .unwrap_or("")
            .to_string();
        let content_hash = get_str(&project, "content_hash").unwrap_or("").to_string();
        let rules = self
            .repository
            .list_rules(project_id)?
            .into_iter()
            .map(|r| Rule {
                key: get_str(&r, "key").unwrap_or("").to_string(),
                summary: get_str(&r, "summary").unwrap_or("").to_string(),
                content: get_str(&r, "content").unwrap_or("").to_string(),
                tags: str_array_owned(r.get("tags")),
                paths: str_array_owned(r.get("paths")),
                languages: str_array_owned(r.get("languages")),
                source_rule_hash: get_str(&r, "source_rule_hash")
                    .unwrap_or("")
                    .to_string(),
            })
            .collect();
        Ok(Checklist {
            schema_version,
            checklist_version,
            global_description,
            content_hash,
            rules,
        })
    }

    /// Mirrors `_refresh_effective_description`: recompute the effective
    /// description from stored source + active overrides and persist it.
    fn refresh_effective_description(&self, project_id: &str) -> Result<Value, ServiceError> {
        let project = self.project(project_id)?;
        let source = self.source_checklist_from_database(project_id)?;
        let statuses = vec!["active".to_string()];
        let effective = apply_overrides(
            &source,
            &self.repository.list_overrides(project_id, Some(&statuses))?,
        );
        let project_name = get_str(&project, "project_name")
            .unwrap_or("")
            .to_string();
        let description = build_description(project_id, &project_name, &effective);
        self.repository
            .update_effective_description(project_id, &description)?;
        Ok(description)
    }

    /// Mirrors `set_override`.
    pub fn set_override(
        &self,
        project_id: &str,
        rule_key: &str,
        fields: &Map<String, Value>,
        reason: &str,
    ) -> Result<Value, ServiceError> {
        self.project(project_id)?;
        if reason.trim().is_empty() {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "override reason must not be empty",
                details!("field" => Value::String("reason".into())),
            )
            .into());
        }
        let validated = validate_override_fields(fields)?;
        // upsert_override re-raises raw sqlite errors → raw-escape path (exit 1).
        self.repository
            .upsert_override(project_id, rule_key, &validated, reason.trim())?;
        let description = self.refresh_effective_description(project_id)?;
        let revision = knowledge_revision_of(&description)
            .expect("build_description always sets checklist.knowledge_revision")
            .to_string();
        Ok(json!({
            "project_id": project_id,
            "key": rule_key,
            "status": "active",
            "knowledge_revision": revision,
        }))
    }

    /// Mirrors `list_overrides` (all statuses).
    pub fn list_overrides(&self, project_id: &str) -> Result<Vec<Value>, ServiceError> {
        self.project(project_id)?;
        Ok(self.repository.list_overrides(project_id, None)?)
    }

    /// Mirrors `show_override`.
    pub fn show_override(
        &self,
        project_id: &str,
        rule_key: &str,
    ) -> Result<Value, ServiceError> {
        self.project(project_id)?;
        let override_row = match self.repository.get_override(project_id, rule_key)? {
            Some(o) => o,
            None => {
                return Err(ReviewKBError::new(
                    ErrorCode::RuleNotFound,
                    format!("override not found: {rule_key}"),
                    details!(
                        "project_id" => Value::String(project_id.into()),
                        "key" => Value::String(rule_key.into())
                    ),
                )
                .into());
            }
        };
        let source = self
            .repository
            .get_rules(project_id, &[rule_key.to_string()])?;
        let source_value = source.into_iter().next().unwrap_or(Value::Null);
        Ok(json!({ "source": source_value, "override": override_row }))
    }

    /// Mirrors `unset_override`.
    pub fn unset_override(
        &self,
        project_id: &str,
        rule_key: &str,
        reason: &str,
    ) -> Result<Value, ServiceError> {
        self.project(project_id)?;
        if reason.trim().is_empty() {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "override reason must not be empty",
                details!("field" => Value::String("reason".into())),
            )
            .into());
        }
        self.repository
            .disable_override(project_id, rule_key, reason.trim())?;
        let description = self.refresh_effective_description(project_id)?;
        let revision = knowledge_revision_of(&description)
            .expect("build_description always sets checklist.knowledge_revision")
            .to_string();
        Ok(json!({
            "project_id": project_id,
            "key": rule_key,
            "status": "disabled",
            "knowledge_revision": revision,
        }))
    }

    /// Mirrors `resolve_override`.
    pub fn resolve_override(
        &self,
        project_id: &str,
        rule_key: &str,
        strategy: &str,
        checklist_path: &str,
        reason: &str,
    ) -> Result<Value, ServiceError> {
        let project = self.project(project_id)?;
        if strategy != "keep-override" && strategy != "accept-source" {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "override resolution strategy must be keep-override or accept-source",
                details!("strategy" => Value::String(strategy.into())),
            )
            .into());
        }
        if reason.trim().is_empty() {
            return Err(ReviewKBError::new(
                ErrorCode::InvalidArgument,
                "override resolution reason must not be empty",
                details!("field" => Value::String("reason".into())),
            )
            .into());
        }
        let checklist = parse_checklist(checklist_path)?;
        let incoming_hash = checklist
            .rules
            .iter()
            .find(|r| r.key == rule_key)
            .map(|r| r.source_rule_hash.clone());
        let keep = strategy == "keep-override";
        // resolve_override re-raises raw sqlite errors → raw-escape path.
        self.repository.resolve_override(
            project_id,
            rule_key,
            keep,
            incoming_hash.as_deref(),
            reason.trim(),
        )?;
        let project_name = get_str(&project, "project_name")
            .unwrap_or("")
            .to_string();
        let mut result = self.prepare(project_id, &project_name, checklist_path, false)?;
        if let Value::Object(m) = &mut result {
            m.insert(
                "override_resolution".into(),
                json!({ "key": rule_key, "strategy": strategy }),
            );
        }
        Ok(result)
    }
}

// ---- free-function helpers (Python `@staticmethod`s) ----

/// Mirrors `_apply_overrides`: returns a new `Checklist` with each rule's
/// non-null override fields substituted in. Active overrides are indexed by
/// `rule_key`; rules without an override are passed through unchanged.
fn apply_overrides(checklist: &Checklist, overrides: &[Value]) -> Checklist {
    let mut by_key: HashMap<String, &Value> = HashMap::new();
    for o in overrides {
        if let Some(k) = get_str(o, "rule_key") {
            by_key.insert(k.to_string(), o);
        }
    }
    let rules = checklist
        .rules
        .iter()
        .map(|rule| match by_key.get(&rule.key) {
            None => rule.clone(),
            Some(o) => apply_rule_override(rule, o),
        })
        .collect();
    let mut out = checklist.clone();
    out.rules = rules;
    out
}

/// Substitute a single rule's non-null override fields.
fn apply_rule_override(rule: &Rule, o: &Value) -> Rule {
    let mut r = rule.clone();
    for field in ["summary", "content", "tags", "paths", "languages"] {
        let val = match o.get(field) {
            Some(v) if !v.is_null() => v,
            _ => continue,
        };
        match field {
            "summary" => {
                if let Some(s) = val.as_str() {
                    r.summary = s.to_string();
                }
            }
            "content" => {
                if let Some(s) = val.as_str() {
                    r.content = s.to_string();
                }
            }
            "tags" => r.tags = str_array_owned(Some(val)),
            "paths" => r.paths = str_array_owned(Some(val)),
            "languages" => r.languages = str_array_owned(Some(val)),
            _ => {}
        }
    }
    r
}

/// Mirrors `_validate_override_fields`.
///
/// - empty input, or any unknown key → `INVALID_ARGUMENT` (allowed_fields sorted);
/// - `summary`/`content` must be non-empty strings (stored trimmed);
/// - `tags`/`paths`/`languages` must be arrays of non-empty strings (stored
///   deduped, first-occurrence order).
fn validate_override_fields(fields: &Map<String, Value>) -> Result<Map<String, Value>, ReviewKBError> {
    // Field-application order (membership check only). Python's error emits
    // `sorted(allowed)`; `canonical_json` sorts object keys but NOT array
    // elements, so the error details must list these already sorted.
    const ALLOWED: &[&str] = &["summary", "content", "tags", "paths", "languages"];
    // sorted(["summary","content","tags","paths","languages"]) — byte-faithful.
    const ALLOWED_SORTED: &[&str] = &["content", "languages", "paths", "summary", "tags"];
    if fields.is_empty() || fields.keys().any(|k| !ALLOWED.contains(&k.as_str())) {
        return Err(ReviewKBError::new(
            ErrorCode::InvalidArgument,
            "override requires at least one supported field",
            details!(
                "allowed_fields" => Value::Array(
                    ALLOWED_SORTED
                        .iter()
                        .map(|s| Value::String((*s).to_string()))
                        .collect()
                )
            ),
        ));
    }
    let mut validated = Map::new();
    for (field, value) in fields {
        match field.as_str() {
            "summary" | "content" => {
                let s = match value.as_str() {
                    Some(s) if !s.trim().is_empty() => s,
                    _ => {
                        return Err(ReviewKBError::new(
                            ErrorCode::InvalidArgument,
                            format!("override {field} must be a non-empty string"),
                            details!("field" => Value::String(field.clone())),
                        ));
                    }
                };
                validated.insert(field.clone(), Value::String(s.trim().to_string()));
            }
            // tags / paths / languages
            _ => {
                let arr = match value.as_array() {
                    Some(arr) if !arr.iter().any(|item| item.as_str().map_or(true, |s| s.trim().is_empty())) => arr,
                    _ => {
                        return Err(ReviewKBError::new(
                            ErrorCode::InvalidArgument,
                            format!("override {field} must be an array of non-empty strings"),
                            details!("field" => Value::String(field.clone())),
                        ));
                    }
                };
                let deduped = dedupe_strings(arr);
                validated.insert(
                    field.clone(),
                    Value::Array(deduped.into_iter().map(Value::String).collect()),
                );
            }
        }
    }
    Ok(validated)
}

/// `list(dict.fromkeys(value))`: dedupe strings preserving first-occurrence order.
fn dedupe_strings(arr: &[Value]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for item in arr {
        if let Some(s) = item.as_str() {
            if seen.insert(s.to_string()) {
                out.push(s.to_string());
            }
        }
    }
    out
}

/// Read `obj[field]` as `&str`, if present and a string. The borrow is tied to
/// `v` (the returned slice is the `Value`'s string data, not the `field` arg).
fn get_str<'a>(v: &'a Value, field: &str) -> Option<&'a str> {
    v.get(field).and_then(|x| x.as_str())
}

/// A JSON array value → `Vec<String>` (empty for null / non-array / non-strings).
fn str_array_owned(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_util::canonical_json;
    use crate::models::Selection;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    /// A minimal valid checklist (2 rules) for service round-trip tests.
    const CHECKLIST_SRC: &str = "\
---
schema_version: 1
checklist_version: \"2026.07.1\"
global_description: project-wide guidance
---

## SEC-001

```yaml review-rule
summary: Check SQL parameterization
tags:
  - security
paths:
  - \"src/**/*.py\"
languages:
  - python
```

Do not concatenate SQL strings.

## DB-004

```yaml review-rule
summary: Check transaction boundaries
tags:
  - database
paths:
  - \"src/**/services/*.py\"
languages:
  - python
```

Wrap full business operations in a transaction.
";

    /// Write `CHECKLIST_SRC` to a fresh temp file and return its path.
    fn write_checklist(dir: &tempfile::TempDir, name: &str, src: &str) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, src).expect("write checklist");
        path
    }

    /// An in-memory migrated repository.
    fn mem_repo() -> Repository {
        let repo = Repository::in_memory().expect("in_memory");
        repo.migrate().expect("migrate");
        repo
    }

    fn assert_envelope<T: std::fmt::Debug>(
        res: Result<T, ServiceError>,
        code: ErrorCode,
        message_contains: &str,
    ) -> ReviewKBError {
        match res {
            Err(ServiceError::Kb(e)) => {
                assert_eq!(e.code, code, "wrong code: {:?}", e.code);
                assert!(
                    e.message.contains(message_contains),
                    "message {:?} does not contain {:?}",
                    e.message,
                    message_contains
                );
                e
            }
            other => panic!("expected Kb({code:?}), got {other:?}"),
        }
    }

    // ---- prepare: create / reuse / refresh / rebuild ----

    #[test]
    fn prepare_creates_new_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);

        let res = svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();
        assert_eq!(res["knowledge_status"], "created");
        assert_eq!(res["checklist_version"], "2026.07.1");
        assert_eq!(res["rule_count"], 2);
        assert_eq!(res["warnings"], json!([]));
        assert_eq!(res["description"]["project"]["name"], "payments");
        assert_eq!(
            res["selection_context"]["project_id"],
            "p1"
        );
        // knowledge_revision is echoed in both places.
        assert_eq!(
            res["selection_context"]["knowledge_revision"],
            res["description"]["checklist"]["knowledge_revision"]
        );

        // The project row now exists.
        let project = repo.get_project("p1").unwrap().unwrap();
        assert_eq!(project["project_name"], "payments");
        assert_eq!(project["rule_count"], 2);
    }

    #[test]
    fn prepare_reuses_when_unchanged_and_silent_on_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);

        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();
        let second = svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();
        assert_eq!(second["knowledge_status"], "reused");
        assert_eq!(second["warnings"], json!([]));
    }

    #[test]
    fn prepare_reused_warns_on_metadata_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);

        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();
        let second =
            svc.prepare("p1", "payments-renamed", path.to_str().unwrap(), false).unwrap();
        assert_eq!(second["knowledge_status"], "reused");
        assert_eq!(
            second["warnings"],
            json!(["PROJECT_METADATA_UPDATED"])
        );
        // project_name was updated.
        let project = repo.get_project("p1").unwrap().unwrap();
        assert_eq!(project["project_name"], "payments-renamed");
    }

    #[test]
    fn prepare_refreshes_on_content_change_same_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);

        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        // Same front-matter version, different rule content → content_hash changes.
        let changed = CHECKLIST_SRC.replace(
            "Do not concatenate SQL strings.",
            "Do not concatenate SQL strings. Use bind parameters everywhere.",
        );
        let path2 = write_checklist(&dir, "cl2.md", &changed);
        let second = svc.prepare("p1", "payments", path2.to_str().unwrap(), false).unwrap();
        assert_eq!(second["knowledge_status"], "refreshed");
        assert_eq!(
            second["warnings"],
            json!(["CONTENT_CHANGED_WITHOUT_VERSION_BUMP"])
        );
    }

    #[test]
    fn prepare_rebuild_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);

        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();
        let rebuilt = svc.prepare("p1", "payments", path.to_str().unwrap(), true).unwrap();
        assert_eq!(rebuilt["knowledge_status"], "rebuilt");
    }

    #[test]
    fn prepare_requires_nonempty_args() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);

        let e = assert_envelope(
            svc.prepare("  ", "payments", path.to_str().unwrap(), false),
            ErrorCode::InvalidArgument,
            "project_id must not be empty",
        );
        assert_eq!(e.details["field"], "project_id");
        let e2 = assert_envelope(
            svc.prepare("p1", "", path.to_str().unwrap(), false),
            ErrorCode::InvalidArgument,
            "project_name must not be empty",
        );
        assert_eq!(e2.details["field"], "project_name");
    }

    // ---- conflict gate ----

    #[test]
    fn prepare_conflict_when_active_override_and_source_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);

        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        // Set an override on SEC-001 (changing its summary).
        let mut fields = Map::new();
        fields.insert("summary".into(), json!("OVER-RIDDEN summary"));
        svc.set_override("p1", "SEC-001", &fields, "why").unwrap();

        // Now change the source rule's content → its source_rule_hash changes.
        let changed = CHECKLIST_SRC.replace(
            "Do not concatenate SQL strings.",
            "Do not concatenate SQL strings. Ever.",
        );
        let path2 = write_checklist(&dir, "cl2.md", &changed);
        let e = assert_envelope(
            svc.prepare("p1", "payments", path2.to_str().unwrap(), false),
            ErrorCode::OverrideConflict,
            "source rules changed while local overrides are active",
        );
        assert_eq!(e.details["project_id"], "p1");
        assert_eq!(e.details["keys"], json!(["SEC-001"]));

        // The override was marked conflict in the DB.
        let ov = svc.show_override("p1", "SEC-001").unwrap();
        assert_eq!(ov["override"]["status"], "conflict");
    }

    // ---- status ----

    #[test]
    fn status_missing_current_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);

        // No project yet → missing, stored_* null.
        let s = svc.status("p1", path.to_str().unwrap()).unwrap();
        assert_eq!(s["status"], "missing");
        assert_eq!(s["stored_version"], Value::Null);
        assert_eq!(s["stored_hash"], Value::Null);

        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();
        let s = svc.status("p1", path.to_str().unwrap()).unwrap();
        assert_eq!(s["status"], "current");
        assert_eq!(s["stored_version"], "2026.07.1");

        // Different version on disk → stale.
        let bumped = CHECKLIST_SRC.replace("\"2026.07.1\"", "\"2026.08.1\"");
        let path2 = write_checklist(&dir, "cl2.md", &bumped);
        let s = svc.status("p1", path2.to_str().unwrap()).unwrap();
        assert_eq!(s["status"], "stale");
        assert_eq!(s["source_version"], "2026.08.1");
    }

    #[test]
    fn status_conflict_when_override_base_hash_diverged() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);

        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();
        let mut fields = Map::new();
        fields.insert("summary".into(), json!("changed summary"));
        svc.set_override("p1", "SEC-001", &fields, "why").unwrap();

        // Source content for SEC-001 changes (same version) → base hash mismatch.
        let changed = CHECKLIST_SRC.replace(
            "Do not concatenate SQL strings.",
            "Do not concatenate SQL strings. Ever.",
        );
        let path2 = write_checklist(&dir, "cl2.md", &changed);
        let s = svc.status("p1", path2.to_str().unwrap()).unwrap();
        assert_eq!(s["status"], "conflict");
        assert_eq!(s["conflict_keys"], json!(["SEC-001"]));
    }

    // ---- project / rules / description views ----

    #[test]
    fn show_project_and_list_projects_strip_description() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        let shown = svc.show_project("p1").unwrap();
        assert!(shown.get("description_json").is_none());
        assert_eq!(shown["project_id"], "p1");

        let listed = svc.list_projects().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].get("description_json").is_none());
    }

    #[test]
    fn list_rules_strips_content_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        let rules = svc.list_rules("p1").unwrap();
        assert_eq!(rules.len(), 2);
        for rule in &rules {
            assert!(rule.get("content").is_none(), "content must be stripped");
            assert!(
                rule.get("source_rule_hash").is_none(),
                "source_rule_hash must be stripped"
            );
            assert!(rule.get("key").is_some());
            assert!(rule.get("ordinal").is_some());
        }
        assert_eq!(rules[0]["key"], "SEC-001");
        assert_eq!(rules[1]["key"], "DB-004");
    }

    #[test]
    fn get_description_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        let desc = svc.get_description("p1").unwrap();
        assert_eq!(desc["project"]["id"], "p1");
        assert_eq!(desc["checklist"]["version"], "2026.07.1");
        assert_eq!(desc["rules"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn search_rules_requires_query_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        assert_envelope(
            svc.search_rules("p1", ""),
            ErrorCode::InvalidArgument,
            "query must not be empty",
        );
        let hits = svc.search_rules("p1", "transaction").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["key"], "DB-004");
    }

    #[test]
    fn project_not_found_paths() {
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        assert_envelope(
            svc.show_project("nope"),
            ErrorCode::ProjectNotFound,
            "project not found: nope",
        );
        assert_envelope(
            svc.get_description("nope"),
            ErrorCode::ProjectNotFound,
            "project not found: nope",
        );
        assert_envelope(
            svc.list_rules("nope"),
            ErrorCode::ProjectNotFound,
            "project not found: nope",
        );
    }

    // ---- get_selected_rules ----

    #[test]
    fn get_selected_rules_applies_active_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        let prep = svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();
        let _ = prep;

        // Override SEC-001 summary + tags. set_override refreshes the effective
        // description, so the knowledge_revision CHANGES — capture the new one.
        let mut fields = Map::new();
        fields.insert("summary".into(), json!("OVERRIDDEN"));
        fields.insert(
            "tags".into(),
            json!(["security", "security", "extra"]),
        );
        let set_result = svc.set_override("p1", "SEC-001", &fields, "why").unwrap();
        let active_revision = set_result["knowledge_revision"].clone();

        let payload = json!({
            "project_id": "p1",
            "knowledge_revision": active_revision,
            "keys": ["SEC-001", "DB-004"],
        });
        let out = svc.get_selected_rules(&payload).unwrap();
        assert_eq!(out["project_id"], "p1");
        let rules = out["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["key"], "SEC-001");
        // summary overridden; tags overridden + deduped.
        assert_eq!(rules[0]["summary"], "OVERRIDDEN");
        assert_eq!(rules[0]["tags"], json!(["security", "extra"]));
        // content/ordinal/source_rule_hash still present from the source rule.
        assert!(rules[0].get("content").is_some());
        assert!(rules[0].get("source_rule_hash").is_some());
        // DB-004 untouched.
        assert_eq!(rules[1]["key"], "DB-004");
        assert_ne!(rules[1]["summary"], "OVERRIDDEN");
    }

    #[test]
    fn get_selected_rules_revision_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        let prep = svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        let payload = json!({
            "project_id": "p1",
            "knowledge_revision": "sha256:stale-revision",
            "keys": ["SEC-001"],
        });
        let _ = prep; // silence unused warning if assertions change
        let e = assert_envelope(
            svc.get_selected_rules(&payload),
            ErrorCode::KnowledgeRevisionMismatch,
            "knowledge revision changed",
        );
        assert_eq!(e.details["requested_revision"], "sha256:stale-revision");
        assert!(e.details["current_revision"].is_string());
    }

    #[test]
    fn get_selected_rules_unknown_key_suggests() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        let prep = svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        let payload = json!({
            "project_id": "p1",
            "knowledge_revision": prep["selection_context"]["knowledge_revision"],
            "keys": ["SEC-001", "DB-999"],
        });
        let e = assert_envelope(
            svc.get_selected_rules(&payload),
            ErrorCode::RuleNotFound,
            "one or more selected rule keys do not exist",
        );
        assert_eq!(e.details["not_found"], json!(["DB-999"]));
        let suggestions = e.details["suggestions"].as_object().unwrap();
        assert!(suggestions.contains_key("DB-999"));
        // DB-004 is a near neighbor.
        assert!(suggestions["DB-999"].as_array().unwrap().iter().any(
            |v| v == "DB-004"
        ));
    }

    #[test]
    fn get_selected_rules_clean_validation_error_is_envelope() {
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        // Missing all fields → clean `missing` errors → INVALID_SELECTION envelope.
        let e = assert_envelope(
            svc.get_selected_rules(&json!({})),
            ErrorCode::InvalidSelection,
            "invalid rule selection",
        );
        let errs = e.details["validation_errors"].as_array().unwrap();
        assert_eq!(errs.len(), 3);
        assert_eq!(
            canonical_json(&e.details["validation_errors"]),
            concat!(
                r#"[{"input":{},"loc":["project_id"],"msg":"Field required","type":"missing"},"#,
                r#"{"input":{},"loc":["knowledge_revision"],"msg":"Field required","type":"missing"},"#,
                r#"{"input":{},"loc":["keys"],"msg":"Field required","type":"missing"}]"#,
            )
        );
    }

    #[test]
    fn get_selected_rules_value_error_is_raw_escape() {
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        // Empty project_id → value_error → raw-escape path (exit 1, empty stdout).
        match svc.get_selected_rules(&json!({
            "project_id": "",
            "knowledge_revision": "r",
            "keys": ["A"],
        })) {
            Err(ServiceError::Raw(_)) => {}
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    #[test]
    fn validate_selection_helper_still_builds_selection() {
        let sel = validate_selection(&json!({
            "project_id": "p",
            "knowledge_revision": "r",
            "keys": ["A", "B"],
        }))
        .expect("valid");
        assert_eq!(sel.project_id, "p");
        // ensure the Selection struct fields are exercised (compile-time guarantee).
        let Selection { project_id: _, knowledge_revision: _, keys: _ } = sel;
    }

    // ---- override lifecycle ----

    #[test]
    fn set_override_refreshes_effective_description() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        let prep = svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();
        let before_rev = prep["selection_context"]["knowledge_revision"].as_str().unwrap().to_string();

        let mut fields = Map::new();
        fields.insert("summary".into(), json!("New summary"));
        let res = svc.set_override("p1", "SEC-001", &fields, "reason").unwrap();
        assert_eq!(res["status"], "active");
        let after_rev = res["knowledge_revision"].as_str().unwrap().to_string();
        assert_ne!(before_rev, after_rev, "effective revision must change");

        // Effective description persisted on the project row.
        let project = repo.get_project("p1").unwrap().unwrap();
        assert_eq!(project["knowledge_revision"], after_rev);

        // list_overrides shows it active.
        let ovs = svc.list_overrides("p1").unwrap();
        assert_eq!(ovs.len(), 1);
        assert_eq!(ovs[0]["rule_key"], "SEC-001");
        assert_eq!(ovs[0]["status"], "active");
        assert_eq!(ovs[0]["summary"], "New summary");
    }

    #[test]
    fn set_override_rejects_invalid_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        // Unknown field.
        let mut fields = Map::new();
        fields.insert("bogus".into(), json!("x"));
        let e = assert_envelope(
            svc.set_override("p1", "SEC-001", &fields, "reason"),
            ErrorCode::InvalidArgument,
            "override requires at least one supported field",
        );
        assert_eq!(
            e.details["allowed_fields"],
            json!(["content", "languages", "paths", "summary", "tags"])
        );

        // Empty summary.
        let mut fields = Map::new();
        fields.insert("summary".into(), json!("   "));
        let e = assert_envelope(
            svc.set_override("p1", "SEC-001", &fields, "reason"),
            ErrorCode::InvalidArgument,
            "override summary must be a non-empty string",
        );
        assert_eq!(e.details["field"], "summary");

        // tags with a non-string element.
        let mut fields = Map::new();
        fields.insert("tags".into(), json!(["ok", 5]));
        let e = assert_envelope(
            svc.set_override("p1", "SEC-001", &fields, "reason"),
            ErrorCode::InvalidArgument,
            "override tags must be an array of non-empty strings",
        );
        assert_eq!(e.details["field"], "tags");

        // Empty reason.
        let mut fields = Map::new();
        fields.insert("summary".into(), json!("ok"));
        let e = assert_envelope(
            svc.set_override("p1", "SEC-001", &fields, "   "),
            ErrorCode::InvalidArgument,
            "override reason must not be empty",
        );
        assert_eq!(e.details["field"], "reason");

        // Unknown rule key → RULE_NOT_FOUND from upsert.
        let mut fields = Map::new();
        fields.insert("summary".into(), json!("ok"));
        assert_envelope(
            svc.set_override("p1", "NOPE", &fields, "reason"),
            ErrorCode::RuleNotFound,
            "rule not found: NOPE",
        );
    }

    #[test]
    fn show_override_and_unset() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        let mut fields = Map::new();
        fields.insert("summary".into(), json!("X"));
        svc.set_override("p1", "SEC-001", &fields, "why").unwrap();

        let shown = svc.show_override("p1", "SEC-001").unwrap();
        assert_eq!(shown["override"]["rule_key"], "SEC-001");
        assert_eq!(shown["source"]["key"], "SEC-001");

        // show on missing override → RULE_NOT_FOUND.
        assert_envelope(
            svc.show_override("p1", "DB-004"),
            ErrorCode::RuleNotFound,
            "override not found: DB-004",
        );

        let unset = svc.unset_override("p1", "SEC-001", "done").unwrap();
        assert_eq!(unset["status"], "disabled");
        let ov = svc.show_override("p1", "SEC-001").unwrap();
        assert_eq!(ov["override"]["status"], "disabled");
    }

    #[test]
    fn resolve_override_accept_source_disables_and_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        // Create a conflict: override + source content change.
        let mut fields = Map::new();
        fields.insert("summary".into(), json!("X"));
        svc.set_override("p1", "SEC-001", &fields, "why").unwrap();
        let changed = CHECKLIST_SRC.replace(
            "Do not concatenate SQL strings.",
            "Do not concatenate SQL strings. Ever.",
        );
        let path2 = write_checklist(&dir, "cl2.md", &changed);
        // Trigger the conflict mark (prepare raises OVERRIDE_CONFLICT).
        assert_envelope(
            svc.prepare("p1", "payments", path2.to_str().unwrap(), false),
            ErrorCode::OverrideConflict,
            "source rules changed",
        );
        assert_eq!(
            svc.show_override("p1", "SEC-001").unwrap()["override"]["status"],
            "conflict"
        );

        // Resolve by accepting the source — no conflict on prepare this time,
        // because the override is disabled (not active) before the refresh.
        let res = svc
            .resolve_override(
                "p1",
                "SEC-001",
                "accept-source",
                path2.to_str().unwrap(),
                "accepting upstream",
            )
            .unwrap();
        assert_eq!(res["knowledge_status"], "refreshed");
        assert_eq!(res["override_resolution"]["strategy"], "accept-source");
        assert_eq!(res["override_resolution"]["key"], "SEC-001");
        let ov = svc.show_override("p1", "SEC-001").unwrap();
        assert_eq!(ov["override"]["status"], "disabled");
    }

    #[test]
    fn resolve_override_rejects_bad_strategy_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_checklist(&dir, "cl.md", CHECKLIST_SRC);
        let repo = mem_repo();
        let svc = KnowledgeService::new(&repo);
        svc.prepare("p1", "payments", path.to_str().unwrap(), false).unwrap();

        assert_envelope(
            svc.resolve_override("p1", "SEC-001", "bogus", path.to_str().unwrap(), "r"),
            ErrorCode::InvalidArgument,
            "override resolution strategy must be keep-override or accept-source",
        );
        assert_envelope(
            svc.resolve_override("p1", "SEC-001", "keep-override", path.to_str().unwrap(), "  "),
            ErrorCode::InvalidArgument,
            "override resolution reason must not be empty",
        );
    }

    // ---- helper-function unit tests ----

    #[test]
    fn dedupe_strings_preserves_first_occurrence() {
        let arr = json!(["a", "b", "a", "c", "b"])
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            dedupe_strings(&arr),
            vec!["a".to_string(), "b".into(), "c".into()]
        );
    }

    #[test]
    fn validate_override_fields_dedupes_arrays() {
        let mut fields = Map::new();
        fields.insert("tags".into(), json!(["x", "x", "y"]));
        let v = validate_override_fields(&fields).unwrap();
        assert_eq!(v["tags"], json!(["x", "y"]));
    }

    #[test]
    fn validate_override_fields_trims_strings() {
        let mut fields = Map::new();
        fields.insert("summary".into(), json!("  hi  "));
        let v = validate_override_fields(&fields).unwrap();
        assert_eq!(v["summary"], "hi");
    }
}
