"""Fitness rules for each driver's canonical connection pool policy.

An async SQLite adapter reaches the database only through an sqlx pool opened by
`awaken_sqlite_pool::EmbeddedSqlite`. Only `awaken-sqlite-pool` may construct
sqlx SQLite pools or connect options, so no store can drift to its own busy
wait, journal or admission policy.
PostgreSQL checkout belongs to awaken-sql-storage, so a standalone adapter
cannot bypass its cancellation/health policy with a second pool constructor.
"""

from __future__ import annotations

import re
from pathlib import Path
from tempfile import TemporaryDirectory

import _crate_boundary_workspace


POLICIES = {
    "sqlite": ("awaken-sqlite-pool", re.compile(
        r"\b(SqlitePoolOptions|SqliteConnectOptions|SqlitePool::connect)\b"),
        "awaken_sqlite_pool::EmbeddedSqlite"),
    "postgres": ("awaken-sql-storage", re.compile(
        r"\b(?:PgPoolOptions\s*::\s*new|PgPool\s*::\s*connect(?:_with|_lazy|_lazy_with)?)\s*\("),
        "awaken_sql_storage::postgres_pool_options"),
}
POOL_LIFECYCLE = re.compile(r"\.\s*(?:after_connect|before_acquire|after_release|test_before_acquire)\s*\(")

SQLITE_VOCABULARY = re.compile(
    r"\b(?:rusqlite|SqliteConnection|SqlitePool|EmbeddedPool|EmbeddedSqlite|SharedSqliteConnection)\b")


def async_sqlite_scheduler_violations(source: str) -> list[str]:
    """Reject async SQLite adapters that bypass the embedded pool policy."""

    production = _crate_boundary_workspace.production_rust(source)
    errors: list[str] = []
    if not SQLITE_VOCABULARY.search(production):
        return errors
    # Construction is enforced independently over every shipped module below.
    # An implementation may receive an EmbeddedPool from its sibling factory;
    # requiring that factory's spelling in every file creates a second scope.
    if "SharedSqliteConnection" in production:
        errors.append("async SQLite adapter bypasses the embedded SQLite policy")
    if re.search(r"\bMutex\s*<\s*(?:Sqlite)?Connection\s*>", production):
        errors.append("async SQLite adapter blocks Tokio workers on Mutex<Connection>")
    return errors


def selftest() -> None:
    # Cause/effect decision table: C1 an async repository opens its pool through
    # EmbeddedSqlite; C2 it holds a raw Mutex around a connection; C3 that Mutex is
    # provably test-only; C4 the same syntax can compile through a non-test
    # feature; C5 it uses the removed blocking bridge instead of EmbeddedSqlite.
    # Effects: E1 accept the canonical policy; E2 reject a second scheduling
    # authority with a blocking connection; E3 ignore only C3; E4 retain and
    # reject C4; E5 reject the bridge as a bypass.
    #
    # | Rule | C1 | raw Mutex | cfg(test)-only | feature-capable | bridge | Effect |
    # | R1   | T  | F         | F              | F               | F      | E1     |
    # | R2   | F  | T         | F              | F               | F      | E2     |
    # | R3   | T  | T         | T              | F               | F      | E3     |
    # | R4   | F  | T         | F              | T               | F      | E4     |
    # | R5   | F  | F         | F              | F               | T      | E5     |
    assert async_sqlite_scheduler_violations(
        "struct Store { pool: EmbeddedPool } EmbeddedSqlite::file(path).open()"
    ) == []  # R1
    assert async_sqlite_scheduler_violations(
        "use std::sync::Mutex; struct Store { connection: Mutex<SqliteConnection> }"
    ) == [
        "async SQLite adapter blocks Tokio workers on Mutex<Connection>",
    ]  # R2
    assert async_sqlite_scheduler_violations(
        "struct Store { pool: EmbeddedPool } EmbeddedSqlite::memory().open()\n"
        "#[cfg(test)] struct Fixture { connection: Mutex<Connection> }"
    ) == []  # R3
    assert async_sqlite_scheduler_violations(
        '#[cfg(any(test, feature = "support"))] '
        "struct Store { connection: Mutex<SqliteConnection> }"
    ) == [
        "async SQLite adapter blocks Tokio workers on Mutex<Connection>",
    ]  # R4
    assert async_sqlite_scheduler_violations(
        "struct Store { connection: SharedSqliteConnection } with_connection("
    ) == ["async SQLite adapter bypasses the embedded SQLite policy"]  # R5
    # R6 receiving the canonical pool needs no second local constructor; a
    # non-SQLite connection mutex belongs to its own transport, not this rule.
    assert async_sqlite_scheduler_violations("struct Store { pool: EmbeddedPool }") == []
    assert async_sqlite_scheduler_violations("struct Store { client: Mutex<Connection> }") == []
    # R6/R7 pool construction outside the policy crate is rejected in production
    # code and ignored in test-only code.
    assert pool_construction_violations("SqlitePoolOptions::new().connect(url)", "sqlite")
    assert pool_construction_violations(
        "#[cfg(test)] mod tests { fn f() { SqlitePoolOptions::new(); } }", "sqlite"
    ) == []
    # PostgreSQL decision table: P1 canonical builder -> accept; P2 each direct
    # driver constructor -> reject; P3 test-only negative controls -> ignore;
    # P4 feature-capable constructor -> retain/reject; P5 type-only capacity
    # projection -> accept. These partition source ownership from type naming.
    assert pool_construction_violations("postgres_pool_options().connect(url)", "postgres") == []
    for constructor in ("PgPoolOptions::new()", "PgPool::connect(url)",
                        "PgPool::connect_with(options)", "PgPool::connect_lazy(url)",
                        "PgPool::connect_lazy_with(options)"):
        assert pool_construction_violations(constructor, "postgres")
    assert pool_construction_violations(
        "#[cfg(test)] mod tests { fn f() { PgPoolOptions::new(); } }", "postgres") == []
    assert pool_construction_violations(
        '#[cfg(any(test, feature = "support"))] fn f() { PgPoolOptions::new(); }', "postgres")
    assert pool_construction_violations(
        "fn options() -> PgPoolOptions { postgres_pool_options() }", "postgres") == []
    # P6 lifecycle overrides outside both policy owners -> reject; test-only
    # callbacks remain legitimate controlled fault/performance fixtures.
    assert lifecycle_violations("options.before_acquire(check)")
    assert lifecycle_violations("options.after_connect(check)")
    assert lifecycle_violations("options.after_release(check)")
    assert lifecycle_violations("options.test_before_acquire(false)")
    assert lifecycle_violations("#[cfg(test)] fn f() { options.before_acquire(check); }") == []
    # P9 renamed imports/type aliases cannot introduce a second constructor;
    # P10 merely carrying the public builder type does not construct a pool.
    assert pool_construction_violations(
        "use sqlx::postgres::PgPoolOptions as Options; Options::new();", "postgres")
    assert pool_construction_violations(
        "type ConnectionPool = sqlx::PgPool; ConnectionPool::connect(url);", "postgres")
    assert pool_construction_violations(
        "use sqlx::postgres::PgPoolOptions as Options; fn f() -> Options { postgres_pool_options() }", "postgres") == []
    # P7 an external module inherits cfg(test) from its parent -> excluded;
    # P8 cfg(any(test, feature=...)) can ship -> included and rejected. Reuse
    # actual module reachability; directory names are not ownership evidence.
    with TemporaryDirectory() as directory:
        source = Path(directory) / "crates/example/src/lib.rs"
        _crate_boundary_workspace.ensure_fixture_manifest(source)
        source.parent.mkdir(parents=True, exist_ok=True)
        source.write_text('#[cfg(test)] mod fixture;\n'
                          '#[cfg(any(test, feature = "support"))] mod candidate;\n')
        fixture = source.with_name("fixture.rs")
        candidate = source.with_name("candidate.rs")
        fixture.write_text("fn open() { PgPoolOptions::new(); }")
        candidate.write_text("fn open() { PgPoolOptions::new(); }")
        reachable = _crate_boundary_workspace.production_module_files(
            [source.parent.parent / "Cargo.toml"])
        assert fixture.resolve() not in reachable, "P7 inherited test-only module"
        assert candidate.resolve() in reachable, "P8 feature-capable module"
        assert pool_construction_violations(candidate.read_text(), "postgres"), "P8"


def pool_construction_violations(source: str, backend: str) -> list[str]:
    """Reject production code that constructs a pool outside its driver owner."""

    production = _crate_boundary_workspace.production_rust(source)
    _, construction, owner = POLICIES[backend]
    found = bool(construction.search(production))
    if backend == "postgres":
        aliases = [(base, alias) for base, alias in re.findall(
            r"\b(PgPoolOptions|PgPool)\s+as\s+(\w+)", production)]
        aliases += [(base, alias) for alias, base in re.findall(
            r"\btype\s+(\w+)\s*=\s*(?:sqlx\s*::\s*(?:postgres\s*::\s*)?)?(PgPoolOptions|PgPool)\s*;", production)]
        for base, alias in aliases:
            method = "new" if base == "PgPoolOptions" else "connect(?:_with|_lazy|_lazy_with)?"
            found |= bool(re.search(rf"\b{re.escape(alias)}\s*::\s*{method}\s*\(", production))
    if found:
        return [f"builds its own sqlx {backend} pool; open it through {owner}"]
    return []


def lifecycle_violations(source: str) -> list[str]:
    if POOL_LIFECYCLE.search(_crate_boundary_workspace.production_rust(source)):
        return ["overrides connection lifecycle outside the canonical driver pool policy"]
    return []


def check_all(repo_root: Path) -> list[str]:
    errors: list[str] = []
    crates_root = repo_root / "crates"
    # Reuse the canonical Cargo/module reachability graph. A sibling tests.rs
    # inherits its parent's cfg(test); a filename heuristic cannot prove that,
    # and would also hide feature-capable production modules.
    manifests = _crate_boundary_workspace.crate_manifests(crates_root)
    for path in sorted(_crate_boundary_workspace.production_module_files(manifests)):
        manifest = next((parent / "Cargo.toml" for parent in path.parents if (parent / "Cargo.toml").exists()), None)
        crate = manifest.parent.name if manifest else None
        source = path.read_text(encoding="utf-8", errors="ignore")
        if crate != POLICIES["sqlite"][0]:
            for error in async_sqlite_scheduler_violations(source):
                errors.append(f"{path.relative_to(repo_root)}: {error}")
        for backend, (policy, _, _) in POLICIES.items():
            if crate != policy:
                for error in pool_construction_violations(source, backend):
                    errors.append(f"{path.relative_to(repo_root)}: {error}")
        if crate not in {policy for policy, _, _ in POLICIES.values()}:
            for error in lifecycle_violations(source):
                errors.append(f"{path.relative_to(repo_root)}: {error}")
    return errors
