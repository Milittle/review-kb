//! Error model — byte-faithful port of `review_kb/errors.py`.
//!
//! `ErrorCode` variant names map to the exact uppercase strings Python emits in
//! the failure envelope's `error.code` field. Exit codes match the Python
//! `_EXIT_CODES` table.

use serde_json::{Map, Value};

/// The 13 error codes. `as_str()` returns the wire string (e.g. `"INVALID_ARGUMENT"`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidArgument,
    ChecklistNotFound,
    ChecklistInvalid,
    ProjectNotFound,
    RuleNotFound,
    InvalidSelection,
    KnowledgeRevisionMismatch,
    OverrideConflict,
    DbLocked,
    DbIntegrityError,
    DbSchemaUnsupported,
    BackupInvalid,
    InternalError,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::ChecklistNotFound => "CHECKLIST_NOT_FOUND",
            Self::ChecklistInvalid => "CHECKLIST_INVALID",
            Self::ProjectNotFound => "PROJECT_NOT_FOUND",
            Self::RuleNotFound => "RULE_NOT_FOUND",
            Self::InvalidSelection => "INVALID_SELECTION",
            Self::KnowledgeRevisionMismatch => "KNOWLEDGE_REVISION_MISMATCH",
            Self::OverrideConflict => "OVERRIDE_CONFLICT",
            Self::DbLocked => "DB_LOCKED",
            Self::DbIntegrityError => "DB_INTEGRITY_ERROR",
            Self::DbSchemaUnsupported => "DB_SCHEMA_UNSUPPORTED",
            Self::BackupInvalid => "BACKUP_INVALID",
            Self::InternalError => "INTERNAL_ERROR",
        }
    }

    /// Process exit code for this error, matching Python's `_EXIT_CODES`.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::InvalidArgument
            | Self::ChecklistNotFound
            | Self::ChecklistInvalid
            | Self::InvalidSelection => 2,
            Self::ProjectNotFound | Self::RuleNotFound => 3,
            Self::KnowledgeRevisionMismatch | Self::OverrideConflict => 4,
            Self::DbLocked
            | Self::DbIntegrityError
            | Self::DbSchemaUnsupported
            | Self::BackupInvalid => 5,
            Self::InternalError => 1,
        }
    }
}

/// A domain error. `details` is always an object (Python uses `details or {}`).
#[derive(Debug, Clone)]
pub struct ReviewKBError {
    pub code: ErrorCode,
    pub message: String,
    pub details: Map<String, Value>,
}

impl ReviewKBError {
    pub fn new(code: ErrorCode, message: impl Into<String>, details: Map<String, Value>) -> Self {
        Self {
            code,
            message: message.into(),
            details,
        }
    }

    /// Error with no details (empty object).
    pub fn plain(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(code, message, Map::new())
    }

    pub fn exit_code(&self) -> i32 {
        self.code.exit_code()
    }

    /// Render as the `error` object inside the failure envelope:
    /// `{"code": ..., "message": ..., "details": {...}}`.
    pub fn as_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("code".into(), Value::String(self.code.as_str().to_string()));
        m.insert("message".into(), Value::String(self.message.clone()));
        m.insert("details".into(), Value::Object(self.details.clone()));
        Value::Object(m)
    }
}

impl std::fmt::Display for ReviewKBError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ReviewKBError {}

/// Build a `details` object from `(name, value)` pairs.
#[macro_export]
macro_rules! details {
    ( $( $key:literal => $val:expr ),* $(,)? ) => {{
        let mut m = serde_json::Map::new();
        $(
            m.insert(($key).to_string(), ($val).into());
        )*
        m
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_have_exact_wire_strings() {
        let cases = &[
            (ErrorCode::InvalidArgument, "INVALID_ARGUMENT", 2),
            (ErrorCode::ChecklistNotFound, "CHECKLIST_NOT_FOUND", 2),
            (ErrorCode::ChecklistInvalid, "CHECKLIST_INVALID", 2),
            (ErrorCode::InvalidSelection, "INVALID_SELECTION", 2),
            (ErrorCode::ProjectNotFound, "PROJECT_NOT_FOUND", 3),
            (ErrorCode::RuleNotFound, "RULE_NOT_FOUND", 3),
            (ErrorCode::KnowledgeRevisionMismatch, "KNOWLEDGE_REVISION_MISMATCH", 4),
            (ErrorCode::OverrideConflict, "OVERRIDE_CONFLICT", 4),
            (ErrorCode::DbLocked, "DB_LOCKED", 5),
            (ErrorCode::DbIntegrityError, "DB_INTEGRITY_ERROR", 5),
            (ErrorCode::DbSchemaUnsupported, "DB_SCHEMA_UNSUPPORTED", 5),
            (ErrorCode::BackupInvalid, "BACKUP_INVALID", 5),
            (ErrorCode::InternalError, "INTERNAL_ERROR", 1),
        ];
        for (code, wire, exit) in cases {
            assert_eq!(code.as_str(), *wire);
            assert_eq!(code.exit_code(), *exit);
        }
    }

    #[test]
    fn as_value_shape() {
        let err = ReviewKBError::new(
            ErrorCode::RuleNotFound,
            "missing",
            details! { "key" => Value::String("X".into()) },
        );
        let v = err.as_value();
        assert_eq!(
            crate::json_util::canonical_json(&v),
            r#"{"code":"RULE_NOT_FOUND","details":{"key":"X"},"message":"missing"}"#
        );
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn plain_error_has_empty_details() {
        let err = ReviewKBError::plain(ErrorCode::InternalError, "boom");
        assert!(err.details.is_empty());
        assert_eq!(err.exit_code(), 1);
    }
}
