CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
    project_id TEXT PRIMARY KEY,
    project_name TEXT NOT NULL,
    checklist_path TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    checklist_version TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    knowledge_revision TEXT NOT NULL,
    description_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS rules (
    project_id TEXT NOT NULL,
    rule_key TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    summary TEXT NOT NULL,
    content TEXT NOT NULL,
    tags_json TEXT NOT NULL,
    paths_json TEXT NOT NULL,
    languages_json TEXT NOT NULL,
    source_rule_hash TEXT NOT NULL,
    PRIMARY KEY (project_id, rule_key),
    UNIQUE (project_id, ordinal),
    FOREIGN KEY (project_id) REFERENCES projects(project_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_rules_project_ordinal
ON rules(project_id, ordinal);

CREATE TABLE IF NOT EXISTS sync_history (
    sync_id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id TEXT NOT NULL,
    action TEXT NOT NULL,
    old_version TEXT,
    new_version TEXT NOT NULL,
    old_hash TEXT,
    new_hash TEXT NOT NULL,
    result TEXT NOT NULL,
    warnings_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
