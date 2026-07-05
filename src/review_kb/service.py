from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from pydantic import ValidationError

from .checklist import parse_checklist
from .description import build_description
from .errors import ErrorCode, ReviewKBError
from .models import Checklist, Rule, Selection
from .repository import Repository
from .suggestions import suggest_keys


class KnowledgeService:
    def __init__(self, repository: Repository) -> None:
        self.repository = repository

    @staticmethod
    def _required(value: str, name: str) -> str:
        if not value.strip():
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                f"{name} must not be empty",
                {"field": name},
            )
        return value

    def prepare(
        self,
        project_id: str,
        project_name: str,
        checklist_path: str | Path,
        *,
        force: bool = False,
    ) -> dict[str, Any]:
        project_id = self._required(project_id, "project_id")
        project_name = self._required(project_name, "project_name")
        path = str(Path(checklist_path).expanduser().resolve())
        source_checklist = parse_checklist(path)
        current = self.repository.get_project(project_id)
        warnings: list[str] = []

        source_changed = current is not None and (
            current["checklist_version"] != source_checklist.checklist_version
            or current["content_hash"] != source_checklist.content_hash
        )

        if current is not None and (source_changed or force):
            incoming_hashes = {
                rule.key: rule.source_rule_hash for rule in source_checklist.rules
            }
            conflicts = []
            for override in self.repository.list_overrides(
                project_id, ("active", "conflict")
            ):
                if (
                    override["status"] == "conflict"
                    or incoming_hashes.get(override["rule_key"])
                    != override["base_source_rule_hash"]
                ):
                    conflicts.append(override["rule_key"])
            if conflicts:
                self.repository.mark_override_conflicts(project_id, conflicts)
                raise ReviewKBError(
                    ErrorCode.OVERRIDE_CONFLICT,
                    "source rules changed while local overrides are active",
                    {"project_id": project_id, "keys": conflicts},
                )

        effective_checklist = self._apply_overrides(
            source_checklist,
            self.repository.list_overrides(project_id, ("active",))
            if current is not None
            else [],
        )
        description = build_description(project_id, project_name, effective_checklist)

        if current is None:
            status = "created"
            self.repository.replace_project(
                project_id,
                project_name,
                path,
                source_checklist,
                description,
                action="create",
            )
        elif source_changed or force:
            status = "rebuilt" if force else "refreshed"
            if (
                current["checklist_version"] == source_checklist.checklist_version
                and current["content_hash"] != source_checklist.content_hash
            ):
                warnings.append("CONTENT_CHANGED_WITHOUT_VERSION_BUMP")
            self.repository.replace_project(
                project_id,
                project_name,
                path,
                source_checklist,
                description,
                action="rebuild" if force else "refresh",
                warnings=warnings,
            )
        else:
            status = "reused"
            if current["project_name"] != project_name or current["checklist_path"] != path:
                warnings.append("PROJECT_METADATA_UPDATED")
                self.repository.update_project_metadata(
                    project_id,
                    project_name,
                    path,
                    description,
                )

        revision = description["checklist"]["knowledge_revision"]
        return {
            "description": description,
            "selection_context": {
                "project_id": project_id,
                "knowledge_revision": revision,
            },
            "knowledge_status": status,
            "checklist_version": source_checklist.checklist_version,
            "content_hash": source_checklist.content_hash,
            "rule_count": len(source_checklist.rules),
            "warnings": warnings,
        }

    def sync(
        self,
        project_id: str,
        project_name: str,
        checklist_path: str | Path,
    ) -> dict[str, Any]:
        return self.prepare(project_id, project_name, checklist_path)

    def rebuild(
        self,
        project_id: str,
        project_name: str,
        checklist_path: str | Path,
    ) -> dict[str, Any]:
        return self.prepare(project_id, project_name, checklist_path, force=True)

    def status(self, project_id: str, checklist_path: str | Path) -> dict[str, Any]:
        project_id = self._required(project_id, "project_id")
        checklist = parse_checklist(checklist_path)
        current = self.repository.get_project(project_id)
        conflict_keys: list[str] = []
        if current is None:
            status = "missing"
        else:
            incoming_hashes = {
                rule.key: rule.source_rule_hash for rule in checklist.rules
            }
            conflict_keys = [
                override["rule_key"]
                for override in self.repository.list_overrides(
                    project_id, ("active", "conflict")
                )
                if override["status"] == "conflict"
                or incoming_hashes.get(override["rule_key"])
                != override["base_source_rule_hash"]
            ]
        if conflict_keys:
            status = "conflict"
        elif current is not None and (
            current["checklist_version"] == checklist.checklist_version
            and current["content_hash"] == checklist.content_hash
        ):
            status = "current"
        elif current is not None:
            status = "stale"
        return {
            "project_id": project_id,
            "status": status,
            "source_version": checklist.checklist_version,
            "source_hash": checklist.content_hash,
            "stored_version": current["checklist_version"] if current else None,
            "stored_hash": current["content_hash"] if current else None,
            "conflict_keys": conflict_keys,
        }

    def _project(self, project_id: str) -> dict[str, Any]:
        project = self.repository.get_project(project_id)
        if project is None:
            raise ReviewKBError(
                ErrorCode.PROJECT_NOT_FOUND,
                f"project not found: {project_id}",
                {"project_id": project_id},
            )
        return project

    def show_project(self, project_id: str) -> dict[str, Any]:
        project = self._project(project_id)
        project.pop("description_json", None)
        return project

    def list_projects(self) -> list[dict[str, Any]]:
        projects = self.repository.list_projects()
        for project in projects:
            project.pop("description_json", None)
        return projects

    def get_description(self, project_id: str) -> dict[str, Any]:
        return json.loads(self._project(project_id)["description_json"])

    def list_rules(self, project_id: str) -> list[dict[str, Any]]:
        self._project(project_id)
        result = self.repository.list_rules(project_id)
        for rule in result:
            rule.pop("content", None)
            rule.pop("source_rule_hash", None)
        return result

    def search_rules(self, project_id: str, query: str) -> list[dict[str, Any]]:
        self._project(project_id)
        if not query:
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "query must not be empty",
                {"field": "query"},
            )
        return self.repository.search_rules(project_id, query)

    def get_selected_rules(self, payload: dict[str, Any]) -> dict[str, Any]:
        try:
            selection = Selection.model_validate(payload)
        except ValidationError as error:
            raise ReviewKBError(
                ErrorCode.INVALID_SELECTION,
                "invalid rule selection",
                {"validation_errors": error.errors(include_url=False)},
            ) from error

        with self.repository.read_transaction():
            return self._get_selected_rules_from_snapshot(selection)

    def _get_selected_rules_from_snapshot(
        self,
        selection: Selection,
    ) -> dict[str, Any]:

        project = self._project(selection.project_id)
        if project["knowledge_revision"] != selection.knowledge_revision:
            raise ReviewKBError(
                ErrorCode.KNOWLEDGE_REVISION_MISMATCH,
                "knowledge revision changed; run prepare and select rules again",
                {
                    "project_id": selection.project_id,
                    "requested_revision": selection.knowledge_revision,
                    "current_revision": project["knowledge_revision"],
                },
            )

        available_rules = self.repository.list_rules(selection.project_id)
        available_keys = [rule["key"] for rule in available_rules]
        available_set = set(available_keys)
        not_found = [key for key in selection.keys if key not in available_set]
        if not_found:
            raise ReviewKBError(
                ErrorCode.RULE_NOT_FOUND,
                "one or more selected rule keys do not exist",
                {
                    "not_found": not_found,
                    "suggestions": {
                        key: suggest_keys(key, available_keys) for key in not_found
                    },
                },
            )

        selected_rules = self.repository.get_rules(selection.project_id, selection.keys)
        overrides = {
            override["rule_key"]: override
            for override in self.repository.list_overrides(selection.project_id, ("active",))
        }
        for rule in selected_rules:
            override = overrides.get(rule["key"])
            if override is None:
                continue
            for field in ("summary", "content", "tags", "paths", "languages"):
                if override[field] is not None:
                    rule[field] = override[field]
        return {
            "project_id": selection.project_id,
            "knowledge_revision": selection.knowledge_revision,
            "rules": selected_rules,
        }

    @staticmethod
    def _apply_overrides(
        checklist: Checklist,
        overrides: list[dict[str, Any]],
    ) -> Checklist:
        by_key = {override["rule_key"]: override for override in overrides}
        rules: list[Rule] = []
        for rule in checklist.rules:
            override = by_key.get(rule.key)
            if override is None:
                rules.append(rule)
                continue
            updates = {
                field: override[field]
                for field in ("summary", "content", "tags", "paths", "languages")
                if override[field] is not None
            }
            rules.append(rule.model_copy(update=updates))
        return checklist.model_copy(update={"rules": rules})

    def _source_checklist_from_database(self, project_id: str) -> Checklist:
        project = self._project(project_id)
        stored_description = json.loads(project["description_json"])
        rules = [
            Rule(
                key=rule["key"],
                summary=rule["summary"],
                content=rule["content"],
                tags=rule["tags"],
                paths=rule["paths"],
                languages=rule["languages"],
                source_rule_hash=rule["source_rule_hash"],
            )
            for rule in self.repository.list_rules(project_id)
        ]
        return Checklist(
            schema_version=project["schema_version"],
            checklist_version=project["checklist_version"],
            global_description=stored_description["global_description"],
            content_hash=project["content_hash"],
            rules=rules,
        )

    def _refresh_effective_description(self, project_id: str) -> dict[str, Any]:
        project = self._project(project_id)
        source = self._source_checklist_from_database(project_id)
        effective = self._apply_overrides(
            source,
            self.repository.list_overrides(project_id, ("active",)),
        )
        description = build_description(
            project_id,
            project["project_name"],
            effective,
        )
        self.repository.update_effective_description(project_id, description)
        return description

    @staticmethod
    def _validate_override_fields(fields: dict[str, Any]) -> dict[str, Any]:
        allowed = {"summary", "content", "tags", "paths", "languages"}
        if not fields or set(fields) - allowed:
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "override requires at least one supported field",
                {"allowed_fields": sorted(allowed)},
            )
        validated: dict[str, Any] = {}
        for field, value in fields.items():
            if field in {"summary", "content"}:
                if not isinstance(value, str) or not value.strip():
                    raise ReviewKBError(
                        ErrorCode.INVALID_ARGUMENT,
                        f"override {field} must be a non-empty string",
                        {"field": field},
                    )
                validated[field] = value.strip()
            else:
                if not isinstance(value, list) or any(
                    not isinstance(item, str) or not item.strip() for item in value
                ):
                    raise ReviewKBError(
                        ErrorCode.INVALID_ARGUMENT,
                        f"override {field} must be an array of non-empty strings",
                        {"field": field},
                    )
                validated[field] = list(dict.fromkeys(value))
        return validated

    def set_override(
        self,
        project_id: str,
        rule_key: str,
        fields: dict[str, Any],
        *,
        reason: str,
    ) -> dict[str, Any]:
        self._project(project_id)
        if not reason.strip():
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "override reason must not be empty",
                {"field": "reason"},
            )
        validated = self._validate_override_fields(fields)
        self.repository.upsert_override(
            project_id,
            rule_key,
            validated,
            reason=reason.strip(),
        )
        description = self._refresh_effective_description(project_id)
        return {
            "project_id": project_id,
            "key": rule_key,
            "status": "active",
            "knowledge_revision": description["checklist"]["knowledge_revision"],
        }

    def list_overrides(self, project_id: str) -> list[dict[str, Any]]:
        self._project(project_id)
        return self.repository.list_overrides(project_id)

    def show_override(self, project_id: str, rule_key: str) -> dict[str, Any]:
        self._project(project_id)
        override = self.repository.get_override(project_id, rule_key)
        if override is None:
            raise ReviewKBError(
                ErrorCode.RULE_NOT_FOUND,
                f"override not found: {rule_key}",
                {"project_id": project_id, "key": rule_key},
            )
        source = self.repository.get_rules(project_id, [rule_key])
        return {"source": source[0] if source else None, "override": override}

    def unset_override(
        self,
        project_id: str,
        rule_key: str,
        *,
        reason: str,
    ) -> dict[str, Any]:
        self._project(project_id)
        if not reason.strip():
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "override reason must not be empty",
                {"field": "reason"},
            )
        self.repository.disable_override(project_id, rule_key, reason=reason.strip())
        description = self._refresh_effective_description(project_id)
        return {
            "project_id": project_id,
            "key": rule_key,
            "status": "disabled",
            "knowledge_revision": description["checklist"]["knowledge_revision"],
        }

    def resolve_override(
        self,
        project_id: str,
        rule_key: str,
        *,
        strategy: str,
        checklist_path: str | Path,
        reason: str,
    ) -> dict[str, Any]:
        project = self._project(project_id)
        if strategy not in {"keep-override", "accept-source"}:
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "override resolution strategy must be keep-override or accept-source",
                {"strategy": strategy},
            )
        if not reason.strip():
            raise ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                "override resolution reason must not be empty",
                {"field": "reason"},
            )
        checklist = parse_checklist(checklist_path)
        incoming = next((rule for rule in checklist.rules if rule.key == rule_key), None)
        keep = strategy == "keep-override"
        self.repository.resolve_override(
            project_id,
            rule_key,
            keep=keep,
            base_source_rule_hash=incoming.source_rule_hash if incoming else None,
            reason=reason.strip(),
        )
        result = self.prepare(
            project_id,
            project["project_name"],
            checklist_path,
        )
        result["override_resolution"] = {
            "key": rule_key,
            "strategy": strategy,
        }
        return result
