from __future__ import annotations

from typing import Any

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator


class Rule(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")

    key: str
    summary: str
    content: str
    tags: list[str] = Field(default_factory=list)
    paths: list[str] = Field(default_factory=list)
    languages: list[str] = Field(default_factory=list)
    source_rule_hash: str


class Checklist(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")

    schema_version: int
    checklist_version: str
    global_description: str
    content_hash: str
    rules: list[Rule]


class Selection(BaseModel):
    model_config = ConfigDict(extra="forbid")

    project_id: str
    knowledge_revision: str
    keys: list[str]

    @field_validator("project_id", "knowledge_revision")
    @classmethod
    def non_empty_string(cls, value: str) -> str:
        if not value.strip():
            raise ValueError("must not be empty")
        return value

    @field_validator("keys")
    @classmethod
    def validate_keys(cls, keys: list[str]) -> list[str]:
        if not keys:
            raise ValueError("must contain at least one key")
        if any(not key or key != key.strip() for key in keys):
            raise ValueError("keys must be non-empty exact strings without surrounding whitespace")
        if len(set(keys)) != len(keys):
            raise ValueError("keys must not contain duplicates")
        return keys


class ResponseEnvelope(BaseModel):
    model_config = ConfigDict(extra="forbid")

    ok: bool
    data: Any | None = None
    error: dict[str, Any] | None = None
    warnings: list[str] = Field(default_factory=list)
    meta: dict[str, Any] = Field(default_factory=dict)

    @model_validator(mode="after")
    def check_success_or_error(self) -> "ResponseEnvelope":
        if self.ok == (self.error is not None):
            raise ValueError("successful responses cannot have errors and failures require one")
        return self

