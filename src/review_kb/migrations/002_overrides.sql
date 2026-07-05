CREATE TABLE IF NOT EXISTS rule_overrides (
    project_id TEXT NOT NULL,
    rule_key TEXT NOT NULL,
    summary TEXT,
    content TEXT,
    tags_json TEXT,
    paths_json TEXT,
    languages_json TEXT,
    base_source_rule_hash TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('active', 'conflict', 'disabled')),
    reason TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (project_id, rule_key),
    FOREIGN KEY (project_id) REFERENCES projects(project_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS audit_log (
    audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id TEXT NOT NULL,
    rule_key TEXT,
    action TEXT NOT NULL,
    reason TEXT NOT NULL,
    change_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_overrides_project_status
ON rule_overrides(project_id, status);

CREATE INDEX IF NOT EXISTS idx_audit_project_created
ON audit_log(project_id, created_at);

