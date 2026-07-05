from __future__ import annotations

import json
import os
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover - exercised on Python 3.10
    import tomli as tomllib

from .errors import ErrorCode, ReviewKBError


@dataclass(frozen=True)
class DatabaseLocation:
    path: Path
    source: str


def default_config_path(environ: Mapping[str, str]) -> Path:
    if environ.get("REVIEW_KB_CONFIG"):
        return Path(environ["REVIEW_KB_CONFIG"]).expanduser()
    root = Path(environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    return root / "review-kb" / "config.toml"


def default_database_path(environ: Mapping[str, str]) -> Path:
    root = Path(environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
    return root / "review-kb" / "knowledge.db"


def read_configured_database_path(config_path: Path) -> Path | None:
    if not config_path.exists():
        return None
    try:
        config = tomllib.loads(config_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ReviewKBError(
            ErrorCode.INVALID_ARGUMENT,
            f"invalid config file: {error}",
            {"path": str(config_path)},
        ) from error
    db_path = config.get("db_path")
    if not isinstance(db_path, str) or not db_path.strip():
        raise ReviewKBError(
            ErrorCode.INVALID_ARGUMENT,
            "config db_path must be a non-empty string",
            {"path": str(config_path)},
        )
    return Path(db_path).expanduser()


def write_database_path(
    database_path: Path,
    *,
    config_path: Path | None = None,
    environ: Mapping[str, str] | None = None,
) -> Path:
    env = os.environ if environ is None else environ
    target = config_path or default_config_path(env)
    value = str(database_path).strip()
    if not value:
        raise ReviewKBError(
            ErrorCode.INVALID_ARGUMENT,
            "db_path must not be empty",
            {"field": "db_path"},
        )
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=target.parent,
            prefix=f".{target.name}.tmp-",
            delete=False,
        ) as handle:
            handle.write(f"db_path = {json.dumps(value, ensure_ascii=False)}\n")
            temporary = Path(handle.name)
        temporary.replace(target)
    except OSError as error:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
        raise ReviewKBError(
            ErrorCode.INVALID_ARGUMENT,
            f"could not write config file: {error}",
            {"path": str(target)},
        ) from error
    return target


def resolve_database_path(
    cli_path: Path | None = None,
    *,
    environ: Mapping[str, str] | None = None,
    config_path: Path | None = None,
) -> DatabaseLocation:
    env = os.environ if environ is None else environ
    if cli_path is not None:
        return DatabaseLocation(cli_path.expanduser(), "command_line")
    if env.get("REVIEW_KB_DB"):
        return DatabaseLocation(Path(env["REVIEW_KB_DB"]).expanduser(), "environment")
    path = config_path or default_config_path(env)
    configured = read_configured_database_path(path)
    if configured is not None:
        return DatabaseLocation(configured, "config_file")
    return DatabaseLocation(default_database_path(env), "platform_default")
