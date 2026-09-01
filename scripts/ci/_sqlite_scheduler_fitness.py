"""Fitness rules for the one event-loop-safe synchronous SQLite scheduler."""

from __future__ import annotations

import re
from pathlib import Path


ASYNC_SQLITE_ADAPTERS = (
    "crates/stores/awaken-resource-store/src/lib.rs",
    "crates/server/awaken-environment-image-build/src/sqlite.rs",
)


def async_sqlite_scheduler_violations(source: str) -> list[str]:
    """Reject async rusqlite adapters that recreate a blocking policy."""

    production = re.split(
        r"(?m)^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*]", source, maxsplit=1
    )[0]
    errors: list[str] = []
    if "SharedSqliteConnection" not in production or "with_connection" not in production:
        errors.append("async SQLite adapter bypasses SharedSqliteConnection scheduling")
    if re.search(r"\bMutex\s*<\s*Connection\s*>", production):
        errors.append("async SQLite adapter blocks Tokio workers on Mutex<Connection>")
    return errors


def selftest() -> None:
    # Cause/effect decision table: C1 an async repository uses rusqlite; C2 it
    # owns SharedSqliteConnection and calls with_connection. R1=C1+C2 accepts
    # event-loop isolation; R2=C1+raw mutex or missing scheduler rejects the
    # second scheduling authority and reports both observable defects.
    assert async_sqlite_scheduler_violations(
        "struct Store { connection: SharedSqliteConnection } with_connection("
    ) == []  # R1
    assert async_sqlite_scheduler_violations(
        "use std::sync::Mutex; struct Store { connection: Mutex<Connection> }"
    ) == [
        "async SQLite adapter bypasses SharedSqliteConnection scheduling",
        "async SQLite adapter blocks Tokio workers on Mutex<Connection>",
    ]  # R2


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    for source_path in ASYNC_SQLITE_ADAPTERS:
        for error in async_sqlite_scheduler_violations(
            (repo_root / source_path).read_text(encoding="utf-8")
        ):
            errors.append(f"{source_path}: {error}")
    return errors
