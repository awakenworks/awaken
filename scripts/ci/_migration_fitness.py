"""Deterministic, versioned SQL migration fitness rules."""
from __future__ import annotations

import re
from pathlib import Path


VERSIONED_SQL = re.compile(r"^V[0-9]{4}__[a-z0-9_]+\.sql$")
DIALECT_SQL = re.compile(
    r"^(?P<identity>V[0-9]{4}__[a-z0-9_]+)\.(?P<dialect>postgres|sqlite)\.sql$"
)
DDL = re.compile(
    r"\b(?:CREATE\s+(?:TABLE|INDEX|SEQUENCE|FUNCTION|TRIGGER|VIEW)|"
    r"ALTER\s+TABLE|DROP\s+(?:TABLE|INDEX|SEQUENCE|FUNCTION|TRIGGER|VIEW))\b",
    re.IGNORECASE,
)


def _production_rust(source: str) -> str:
    source = re.split(
        r"(?m)^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*]",
        source,
        maxsplit=1,
    )[0]
    return "\n".join(
        line for line in source.splitlines() if not line.lstrip().startswith("//")
    )


def _migration_declarations(source: str) -> str:
    """Return the declaration region that owns inline Migration SQL.

    Some older crates keep their bundle function at the top of `lib.rs` beside
    runtime DML. The region begins at the first Migration constructor and ends
    at the first column-zero closing brace after the final constructor, so
    unrelated module prelude and ordinary idempotent writes affect no identity.
    """
    constructors = (
        "Migration::new",
        "Migration::per_dialect",
        "Migration::published_legacy",
        "Migration::published_legacy_with_aliases",
        "Migration::published_legacy_per_dialect",
        "Migration::published_legacy_per_dialect_with_aliases",
    )
    starts = [source.find(constructor) for constructor in constructors]
    starts = [start for start in starts if start >= 0]
    if not starts:
        return ""
    first = min(starts)
    last = max(source.rfind(constructor) for constructor in constructors)
    end = source.find("\n}", last)
    return source[first:] if end < 0 else source[first : end + 2]


def check_all(repo_root: Path) -> list[str]:
    """Check version identity and DDL ownership.

    SQL-policy validation belongs to the authoritative Foundation `Migration`
    constructors. In particular, `published_legacy*` pins historical bytes and
    may intentionally preserve conditional SQL that `Migration::new` rejects.
    Reimplementing that distinction here would create a second checksum/policy
    source of truth.
    """
    errors: list[str] = []
    crates = repo_root / "crates"

    dialect_groups: dict[tuple[Path, str], dict[str, Path]] = {}
    for path in sorted(crates.rglob("*.sql")):
        dialect = DIALECT_SQL.fullmatch(path.name)
        if path.parent.name == "migrations" and dialect:
            key = (path.parent, dialect.group("identity"))
            dialect_groups.setdefault(key, {})[dialect.group("dialect")] = path
            continue
        if path.parent.name != "migrations" or not VERSIONED_SQL.fullmatch(path.name):
            errors.append(
                f"{path.relative_to(repo_root)}: SQL schema file is not a versioned "
                "migrations/Vdddd__slug.sql authority or a paired dialect migration"
            )
            continue

    for (directory, identity), pair in sorted(dialect_groups.items()):
        missing = {"postgres", "sqlite"}.difference(pair)
        if missing:
            present = next(iter(pair.values()))
            errors.append(
                f"{present.relative_to(repo_root)}: dialect migration {identity} is missing "
                f"{', '.join(sorted(missing))} sibling"
            )
            continue
        postgres = pair["postgres"]
        sqlite = pair["sqlite"]
        include_postgres = f'include_str!("migrations/{postgres.name}")'
        include_sqlite = f'include_str!("migrations/{sqlite.name}")'
        owners = []
        for source_path in sorted(directory.parent.rglob("*.rs")):
            source = _production_rust(source_path.read_text(encoding="utf-8"))
            if include_postgres in source or include_sqlite in source:
                owners.append((source_path, source))
        exact_owners = [
            source_path
            for source_path, source in owners
            if include_postgres in source
            and include_sqlite in source
            and "Migration::per_dialect" in _migration_declarations(source)
        ]
        if len(exact_owners) != 1:
            errors.append(
                f"{postgres.relative_to(repo_root)}: paired dialect migration {identity} must "
                "be included together by exactly one Migration::per_dialect declaration"
            )

    for path in sorted(crates.rglob("*.rs")):
        if "tests" in path.parts:
            continue
        source = _production_rust(path.read_text(encoding="utf-8"))
        migration_source = _migration_declarations(source)
        owns_migration = bool(migration_source)
        if DDL.search(source) and not owns_migration:
            errors.append(
                f"{path.relative_to(repo_root)}: production DDL is not owned by a versioned Migration"
            )
    return errors


def selftest() -> None:
    """Cause/effect decision table.

    M1 versioned SQL -> accepted; M2 unversioned SQL filename -> rejected; M3
    production raw DDL without Migration ownership -> rejected; M4 the same DDL
    inside a Migration -> accepted; M5 inline test fixture DDL -> ignored; M6
    runtime idempotent DML after an inline bundle declaration -> ignored; M7 a
    published-legacy constructor owns historical DDL. Constructor tests in
    awaken-scoped-migration own the separate SQL-policy decision table; M8
    unrelated code before an inline bundle -> does not change its identity; M9
    only an exact Postgres/SQLite dialect suffix carries one shared identity.
    """
    assert VERSIONED_SQL.fullmatch("V0001__catalog.sql")  # M1
    assert not VERSIONED_SQL.fullmatch("catalog.sql")  # M2
    assert DDL.search(_production_rust('const SQL: &str = "CREATE TABLE x(id TEXT)";'))  # M3
    assert "Migration::new" in _production_rust(
        'Migration::new(1, "x", "CREATE TABLE {prefix}_x(id TEXT)")'
    )  # M4
    assert not DDL.search(
        _production_rust(
            '#[cfg(test)]\nmod tests { const SQL: &str = "CREATE TABLE fixture(id TEXT)"; }'
        )
    )  # M5
    mixed = """pub fn bundle() {\nMigration::new(1, \"x\", \"CREATE TABLE x(id INT)\");\n}\n\
pub fn write() { sql(\"INSERT OR IGNORE INTO x VALUES (1)\"); }"""
    assert "INSERT OR IGNORE" not in _migration_declarations(mixed)  # M6
    published = (
        'Migration::published_legacy(9, "x", "DROP TABLE IF EXISTS {prefix}_x", '
        '"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")'
    )
    assert "published_legacy" in _migration_declarations(published)  # M7
    assert _migration_declarations(published) == _migration_declarations(
        "#[cfg(feature = \"test-support\")]\nuse fixture::Store;\n" + published
    )  # M8
    postgres = DIALECT_SQL.fullmatch("V0025__nonnegative_authority.postgres.sql")
    sqlite = DIALECT_SQL.fullmatch("V0025__nonnegative_authority.sqlite.sql")
    assert postgres and sqlite
    assert postgres.group("identity") == sqlite.group("identity")  # M9
    assert not DIALECT_SQL.fullmatch("V0025__nonnegative_authority.mysql.sql")
