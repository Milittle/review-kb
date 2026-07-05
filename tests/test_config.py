from pathlib import Path

from review_kb.config import (
    read_configured_database_path,
    write_database_path,
)


def test_write_and_read_database_path_atomically(tmp_path: Path) -> None:
    config_path = tmp_path / "config" / "review-kb.toml"
    database_path = tmp_path / 'data with "quotes"' / "knowledge.db"

    written = write_database_path(database_path, config_path=config_path)

    assert written == config_path
    assert read_configured_database_path(config_path) == database_path
    assert not list(config_path.parent.glob("*.tmp-*"))
