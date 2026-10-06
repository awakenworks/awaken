"""Fitness rules for the one embedded SQLite connection policy (ADR-0087).

An async SQLite adapter reaches the database either through the legacy rusqlite
bridge (`SharedSqliteConnection` + `with_connection`) while it awaits migration,
or through an sqlx pool opened by `awaken_sqlite_pool::EmbeddedSqlite`. Only
`awaken-sqlite-pool` may construct sqlx SQLite pools or connect options, so no
store can drift to its own busy wait, journal or admission policy.
"""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace


POLICY_CRATE = "awaken-sqlite-pool"
POOL_CONSTRUCTION = re.compile(r"\b(SqlitePoolOptions|SqliteConnectOptions|SqlitePool::connect)\b")

ASYNC_SQLITE_ADAPTERS = (
    "crates/stores/awaken-resource-store/src/lib.rs",
    "crates/server/awaken-environment-image-build/src/sqlite.rs",
)


def async_sqlite_scheduler_violations(source: str) -> list[str]:
    """Reject async rusqlite adapters that recreate a blocking policy."""

    production = _crate_boundary_workspace.production_rust(source)
    errors: list[str] = []
    bridged = "SharedSqliteConnection" in production and "with_connection" in production
    pooled = "EmbeddedSqlite" in production
    if not (bridged or pooled):
        errors.append("async SQLite adapter bypasses the embedded SQLite policy")
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
        "async SQLite adapter bypasses the embedded SQLite policy",
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
        "async SQLite adapter bypasses the embedded SQLite policy",
        "async SQLite adapter blocks Tokio workers on Mutex<Connection>",
    ]  # R4
    # R5 an adapter opened through the policy crate's EmbeddedSqlite is accepted.
    assert async_sqlite_scheduler_violations(
        "struct Store { pool: SqlitePool } EmbeddedSqlite::file(path).open()"
    ) == []  # R5
    # R6/R7 pool construction outside the policy crate is rejected in production
    # code and ignored in test-only code.
    assert pool_construction_violations("SqlitePoolOptions::new().connect(url)")
    assert pool_construction_violations(
        "#[cfg(test)] mod tests { fn f() { SqlitePoolOptions::new(); } }"
    ) == []


def pool_construction_violations(source: str) -> list[str]:
    """Reject sqlx SQLite pool or connect-option construction in production code."""

    production = _crate_boundary_workspace.production_rust(source)
    if POOL_CONSTRUCTION.search(production):
        return ["builds its own sqlx SQLite pool; open it through awaken_sqlite_pool::EmbeddedSqlite"]
    return []


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    for source_path in ASYNC_SQLITE_ADAPTERS:
        for error in async_sqlite_scheduler_violations(
            (repo_root / source_path).read_text(encoding="utf-8")
        ):
            errors.append(f"{source_path}: {error}")
    crates_root = repo_root / "crates"
    for path in _crate_boundary_workspace.crate_source_files(crates_root):
        manifest = next((parent / "Cargo.toml" for parent in path.parents if (parent / "Cargo.toml").exists()), None)
        if manifest is not None and manifest.parent.name == POLICY_CRATE:
            continue
        for error in pool_construction_violations(path.read_text(encoding="utf-8", errors="ignore")):
            errors.append(f"{path.relative_to(repo_root)}: {error}")
    return errors
