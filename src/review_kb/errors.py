from __future__ import annotations

from enum import Enum
from typing import Any


class ErrorCode(str, Enum):
    INVALID_ARGUMENT = "INVALID_ARGUMENT"
    CHECKLIST_NOT_FOUND = "CHECKLIST_NOT_FOUND"
    CHECKLIST_INVALID = "CHECKLIST_INVALID"
    PROJECT_NOT_FOUND = "PROJECT_NOT_FOUND"
    RULE_NOT_FOUND = "RULE_NOT_FOUND"
    INVALID_SELECTION = "INVALID_SELECTION"
    KNOWLEDGE_REVISION_MISMATCH = "KNOWLEDGE_REVISION_MISMATCH"
    OVERRIDE_CONFLICT = "OVERRIDE_CONFLICT"
    DB_LOCKED = "DB_LOCKED"
    DB_INTEGRITY_ERROR = "DB_INTEGRITY_ERROR"
    DB_SCHEMA_UNSUPPORTED = "DB_SCHEMA_UNSUPPORTED"
    BACKUP_INVALID = "BACKUP_INVALID"
    INTERNAL_ERROR = "INTERNAL_ERROR"


_EXIT_CODES: dict[ErrorCode, int] = {
    ErrorCode.INVALID_ARGUMENT: 2,
    ErrorCode.CHECKLIST_NOT_FOUND: 2,
    ErrorCode.CHECKLIST_INVALID: 2,
    ErrorCode.INVALID_SELECTION: 2,
    ErrorCode.PROJECT_NOT_FOUND: 3,
    ErrorCode.RULE_NOT_FOUND: 3,
    ErrorCode.KNOWLEDGE_REVISION_MISMATCH: 4,
    ErrorCode.OVERRIDE_CONFLICT: 4,
    ErrorCode.DB_LOCKED: 5,
    ErrorCode.DB_INTEGRITY_ERROR: 5,
    ErrorCode.DB_SCHEMA_UNSUPPORTED: 5,
    ErrorCode.BACKUP_INVALID: 5,
    ErrorCode.INTERNAL_ERROR: 1,
}


class ReviewKBError(Exception):
    def __init__(
        self,
        code: ErrorCode,
        message: str,
        details: dict[str, Any] | None = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.details = details or {}

    @property
    def exit_code(self) -> int:
        return _EXIT_CODES[self.code]

    def as_dict(self) -> dict[str, Any]:
        return {
            "code": self.code.value,
            "message": self.message,
            "details": self.details,
        }
