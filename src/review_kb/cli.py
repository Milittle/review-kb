from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any, Callable, Optional

import typer

from .config import DatabaseLocation, resolve_database_path, write_database_path
from .description import canonical_json
from .errors import ErrorCode, ReviewKBError
from .repository import Repository
from .service import KnowledgeService


app = typer.Typer(
    name="review-kb",
    no_args_is_help=True,
    rich_markup_mode=None,
    pretty_exceptions_enable=False,
)
description_app = typer.Typer(no_args_is_help=True, rich_markup_mode=None)
projects_app = typer.Typer(no_args_is_help=True, rich_markup_mode=None)
rules_app = typer.Typer(no_args_is_help=True, rich_markup_mode=None)
overrides_app = typer.Typer(no_args_is_help=True, rich_markup_mode=None)
db_app = typer.Typer(no_args_is_help=True, rich_markup_mode=None)
config_app = typer.Typer(no_args_is_help=True, rich_markup_mode=None)
app.add_typer(description_app, name="description")
app.add_typer(projects_app, name="projects")
app.add_typer(rules_app, name="rules")
app.add_typer(overrides_app, name="overrides")
app.add_typer(db_app, name="db")
app.add_typer(config_app, name="config")


@app.callback()
def root(
    ctx: typer.Context,
    db: Optional[Path] = typer.Option(None, "--db", help="SQLite database path"),
) -> None:
    ctx.obj = {"database": resolve_database_path(db)}


def _emit(value: dict[str, Any]) -> None:
    typer.echo(canonical_json(value))


def _success(command: str, data: Any, warnings: list[str] | None = None) -> None:
    _emit(
        {
            "ok": True,
            "data": data,
            "warnings": warnings or [],
            "meta": {"command": command, "schema_version": 1},
        }
    )


def _failure(command: str, error: ReviewKBError) -> None:
    _emit(
        {
            "ok": False,
            "error": error.as_dict(),
            "warnings": [],
            "meta": {"command": command, "schema_version": 1},
        }
    )
    raise typer.Exit(error.exit_code)


def _location(ctx: typer.Context) -> DatabaseLocation:
    return ctx.obj["database"]


def _with_service(
    ctx: typer.Context,
    command: str,
    callback: Callable[[KnowledgeService], Any],
    *,
    create_database: bool = False,
) -> None:
    repository: Repository | None = None
    try:
        database_path = _location(ctx).path
        repository = (
            Repository.open(database_path)
            if create_database or database_path.exists()
            else Repository.in_memory()
        )
        repository.migrate()
        result = callback(KnowledgeService(repository))
        warnings = result.pop("warnings", []) if isinstance(result, dict) else []
        _success(command, result, warnings)
    except ReviewKBError as error:
        _failure(command, error)
    finally:
        if repository is not None:
            repository.close()


@app.command("prepare")
def prepare_command(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
    project_name: str = typer.Option(..., "--project-name"),
    checklist: Path = typer.Option(..., "--checklist"),
) -> None:
    _with_service(
        ctx,
        "prepare",
        lambda service: service.prepare(project_id, project_name, checklist),
        create_database=True,
    )


@app.command("status")
def status_command(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
    checklist: Path = typer.Option(..., "--checklist"),
) -> None:
    _with_service(ctx, "status", lambda service: service.status(project_id, checklist))


@app.command("sync")
def sync_command(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
    project_name: str = typer.Option(..., "--project-name"),
    checklist: Path = typer.Option(..., "--checklist"),
) -> None:
    _with_service(
        ctx,
        "sync",
        lambda service: service.sync(project_id, project_name, checklist),
        create_database=True,
    )


@app.command("rebuild")
def rebuild_command(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
    project_name: str = typer.Option(..., "--project-name"),
    checklist: Path = typer.Option(..., "--checklist"),
) -> None:
    _with_service(
        ctx,
        "rebuild",
        lambda service: service.rebuild(project_id, project_name, checklist),
        create_database=True,
    )


@description_app.command("get")
def description_get(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
) -> None:
    _with_service(
        ctx,
        "description get",
        lambda service: service.get_description(project_id),
    )


@projects_app.command("list")
def projects_list(ctx: typer.Context) -> None:
    _with_service(ctx, "projects list", lambda service: {"projects": service.list_projects()})


@projects_app.command("show")
def projects_show(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
) -> None:
    _with_service(ctx, "projects show", lambda service: service.show_project(project_id))


@rules_app.command("list")
def rules_list(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
) -> None:
    _with_service(
        ctx,
        "rules list",
        lambda service: {"rules": service.list_rules(project_id)},
    )


def _read_selection(input_path: str) -> dict[str, Any]:
    try:
        source = sys.stdin.read() if input_path == "-" else Path(input_path).read_text(encoding="utf-8")
        payload = json.loads(source)
    except (OSError, json.JSONDecodeError) as error:
        raise ReviewKBError(
            ErrorCode.INVALID_SELECTION,
            f"selection input must be one valid JSON document: {error}",
            {"input": input_path},
        ) from error
    if not isinstance(payload, dict):
        raise ReviewKBError(
            ErrorCode.INVALID_SELECTION,
            "selection input must be a JSON object",
            {"input": input_path},
        )
    return payload


def _override_input(payload: dict[str, Any]) -> tuple[str, str, str, dict[str, Any]]:
    required = ("project_id", "key", "reason")
    missing = [field for field in required if not isinstance(payload.get(field), str)]
    if missing:
        raise ReviewKBError(
            ErrorCode.INVALID_ARGUMENT,
            "override input is missing required string fields",
            {"fields": missing},
        )
    fields = {key: value for key, value in payload.items() if key not in required}
    return payload["project_id"], payload["key"], payload["reason"], fields


@rules_app.command("get")
def rules_get(
    ctx: typer.Context,
    input_path: str = typer.Option(..., "--input"),
) -> None:
    try:
        payload = _read_selection(input_path)
    except ReviewKBError as error:
        _failure("rules get", error)
    _with_service(
        ctx,
        "rules get",
        lambda service: service.get_selected_rules(payload),
    )


@rules_app.command("search")
def rules_search(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
    query: str = typer.Option(..., "--query"),
) -> None:
    _with_service(
        ctx,
        "rules search",
        lambda service: {"rules": service.search_rules(project_id, query)},
    )


@overrides_app.command("set")
def overrides_set(
    ctx: typer.Context,
    input_path: str = typer.Option(..., "--input"),
) -> None:
    try:
        project_id, rule_key, reason, fields = _override_input(
            _read_selection(input_path)
        )
    except ReviewKBError as error:
        _failure("overrides set", error)
    _with_service(
        ctx,
        "overrides set",
        lambda service: service.set_override(
            project_id,
            rule_key,
            fields,
            reason=reason,
        ),
    )


@overrides_app.command("list")
def overrides_list(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
) -> None:
    _with_service(
        ctx,
        "overrides list",
        lambda service: {"overrides": service.list_overrides(project_id)},
    )


@overrides_app.command("show")
def overrides_show(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
    rule_key: str = typer.Option(..., "--key"),
) -> None:
    _with_service(
        ctx,
        "overrides show",
        lambda service: service.show_override(project_id, rule_key),
    )


@overrides_app.command("unset")
def overrides_unset(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
    rule_key: str = typer.Option(..., "--key"),
    reason: str = typer.Option(..., "--reason"),
) -> None:
    _with_service(
        ctx,
        "overrides unset",
        lambda service: service.unset_override(
            project_id,
            rule_key,
            reason=reason,
        ),
    )


@overrides_app.command("resolve")
def overrides_resolve(
    ctx: typer.Context,
    project_id: str = typer.Option(..., "--project-id"),
    rule_key: str = typer.Option(..., "--key"),
    strategy: str = typer.Option(..., "--strategy"),
    checklist: Path = typer.Option(..., "--checklist"),
    reason: str = typer.Option(..., "--reason"),
) -> None:
    _with_service(
        ctx,
        "overrides resolve",
        lambda service: service.resolve_override(
            project_id,
            rule_key,
            strategy=strategy,
            checklist_path=checklist,
            reason=reason,
        ),
        create_database=False,
    )


@db_app.command("info")
def db_info(ctx: typer.Context) -> None:
    location = _location(ctx)

    def info(service: KnowledgeService) -> dict[str, Any]:
        projects = service.list_projects()
        return {
            "db_path": str(location.path),
            "path_source": location.source,
            "project_count": len(projects),
            "rule_count": sum(project["rule_count"] for project in projects),
            "schema_version": service.repository.schema_version(),
        }

    _with_service(ctx, "db info", info)


@db_app.command("check")
def db_check(ctx: typer.Context) -> None:
    _with_service(
        ctx,
        "db check",
        lambda service: {"integrity": service.repository.integrity_check()},
    )


@db_app.command("query")
def db_query(
    ctx: typer.Context,
    view: str = typer.Option(..., "--view"),
    project_id: Optional[str] = typer.Option(None, "--project-id"),
    query: Optional[str] = typer.Option(None, "--query"),
    limit: int = typer.Option(100, "--limit", min=1, max=1000),
) -> None:
    _with_service(
        ctx,
        "db query",
        lambda service: {
            "view": view,
            "rows": service.repository.query_view(
                view,
                project_id=project_id,
                query=query,
                limit=limit,
            ),
        },
    )


@db_app.command("backup")
def db_backup(
    ctx: typer.Context,
    output: Path = typer.Option(..., "--output"),
) -> None:
    database_path = _location(ctx).path
    repository: Repository | None = None
    try:
        if not database_path.is_file():
            raise ReviewKBError(
                ErrorCode.BACKUP_INVALID,
                f"database file not found: {database_path}",
                {"path": str(database_path)},
            )
        repository = Repository.open(database_path)
        repository.migrate()
        _success("db backup", repository.backup(output))
    except ReviewKBError as error:
        _failure("db backup", error)
    finally:
        if repository is not None:
            repository.close()


@db_app.command("restore")
def db_restore(
    ctx: typer.Context,
    input_path: Path = typer.Option(..., "--input"),
) -> None:
    try:
        result = Repository.restore(input_path, _location(ctx).path)
    except ReviewKBError as error:
        _failure("db restore", error)
    _success("db restore", result)


@db_app.command("migrate")
def db_migrate(ctx: typer.Context) -> None:
    _with_service(
        ctx,
        "db migrate",
        lambda service: {"schema_version": service.repository.schema_version()},
        create_database=True,
    )


@config_app.command("show")
def config_show(ctx: typer.Context) -> None:
    location = _location(ctx)
    _success(
        "config show",
        {"db_path": str(location.path), "source": location.source},
    )


@config_app.command("get")
def config_get(
    ctx: typer.Context,
    key: str = typer.Argument(...),
) -> None:
    if key != "db_path":
        _failure(
            "config get",
            ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                f"unsupported config key: {key}",
                {"allowed_keys": ["db_path"]},
            ),
        )
    location = _location(ctx)
    _success(
        "config get",
        {"key": key, "value": str(location.path), "source": location.source},
    )


@config_app.command("set")
def config_set(
    key: str = typer.Argument(...),
    value: Path = typer.Argument(...),
) -> None:
    if key != "db_path":
        _failure(
            "config set",
            ReviewKBError(
                ErrorCode.INVALID_ARGUMENT,
                f"unsupported config key: {key}",
                {"allowed_keys": ["db_path"]},
            ),
        )
    try:
        config_path = write_database_path(value)
    except ReviewKBError as error:
        _failure("config set", error)
    _success(
        "config set",
        {"key": key, "value": str(value), "config_path": str(config_path)},
    )


def main() -> None:
    app()


if __name__ == "__main__":
    main()
