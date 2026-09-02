"""Fitness rules for the one event-loop-safe synchronous SQLite scheduler."""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace


ASYNC_SQLITE_ADAPTERS = (
    "crates/stores/awaken-resource-store/src/lib.rs",
    "crates/server/awaken-environment-image-build/src/sqlite.rs",
)


def async_sqlite_scheduler_violations(source: str) -> list[str]:
    """Reject async rusqlite adapters that recreate a blocking policy."""

    production = _crate_boundary_workspace.production_rust(source)
    errors: list[str] = []
    if "SharedSqliteConnection" not in production or "with_connection" not in production:
        errors.append("async SQLite adapter bypasses SharedSqliteConnection scheduling")
    if re.search(r"\bMutex\s*<\s*Connection\s*>", production):
        errors.append("async SQLite adapter blocks Tokio workers on Mutex<Connection>")
    return errors


def selftest() -> None:
    # Cause/effect decision table: C1 an async repository uses rusqlite; C2 it
    # owns SharedSqliteConnection and calls with_connection; C3 a raw Mutex is
    # provably test-only; C4 the same syntax can compile through a non-test
    # feature. Effects: E1 accept the canonical scheduler; E2 reject a second
    # scheduling authority with both observable defects; E3 ignore only C3;
    # E4 retain and reject C4.
    #
    # | Rule | C2 | raw Mutex | cfg(test)-only | feature-capable | Effect |
    # | R1   | T  | F         | F              | F               | E1     |
    # | R2   | F  | T         | F              | F               | E2     |
    # | R3   | T  | T         | T              | F               | E3     |
    # | R4   | F  | T         | F              | T               | E4     |
    assert async_sqlite_scheduler_violations(
        "struct Store { connection: SharedSqliteConnection } with_connection("
    ) == []  # R1
    assert async_sqlite_scheduler_violations(
        "use std::sync::Mutex; struct Store { connection: Mutex<Connection> }"
    ) == [
        "async SQLite adapter bypasses SharedSqliteConnection scheduling",
        "async SQLite adapter blocks Tokio workers on Mutex<Connection>",
    ]  # R2
    assert async_sqlite_scheduler_violations(
        "struct Store { connection: SharedSqliteConnection } with_connection(\n"
        "#[cfg(test)] struct Fixture { connection: Mutex<Connection> }"
    ) == []  # R3
    assert async_sqlite_scheduler_violations(
        '#[cfg(any(test, feature = "support"))] '
        "struct Store { connection: Mutex<Connection> }"
    ) == [
        "async SQLite adapter bypasses SharedSqliteConnection scheduling",
        "async SQLite adapter blocks Tokio workers on Mutex<Connection>",
    ]  # R4


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    for source_path in ASYNC_SQLITE_ADAPTERS:
        for error in async_sqlite_scheduler_violations(
            (repo_root / source_path).read_text(encoding="utf-8")
        ):
            errors.append(f"{source_path}: {error}")
    return errors
