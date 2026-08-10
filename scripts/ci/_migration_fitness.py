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
SESSION_SCHEMA = "crates/stores/awaken-session-store/src/schema.rs"

CONDITIONAL_MIGRATION_SQL = (
    ("IF NOT EXISTS", re.compile(r"\bIF\s+NOT\s+EXISTS\b", re.IGNORECASE)),
    ("IF EXISTS", re.compile(r"\bIF\s+EXISTS\b", re.IGNORECASE)),
    ("CREATE OR REPLACE", re.compile(r"\bCREATE\s+OR\s+REPLACE\b", re.IGNORECASE)),
    ("INSERT OR IGNORE", re.compile(r"\bINSERT\s+OR\s+IGNORE\b", re.IGNORECASE)),
    (
        "ON CONFLICT DO NOTHING",
        re.compile(r"\bON\s+CONFLICT(?:\s*\([^)]*\))?\s+DO\s+NOTHING\b", re.IGNORECASE),
    ),
)

LEGACY_CONSTRUCTORS = (
    "Migration::published_legacy(",
    "Migration::published_legacy_with_aliases(",
    "Migration::published_legacy_per_dialect(",
    "Migration::published_legacy_per_dialect_with_aliases(",
)


def _session_registry_violations(source: str) -> list[str]:
    errors: list[str] = []
    if "fn published(" in source or "Migration::published_legacy(" in source:
        errors.append(
            f"{SESSION_SCHEMA}: unreleased Session history must have one deterministic migration stream"
        )
    return errors


def _conditional_errors(path: Path, source: str, root: Path) -> list[str]:
    errors: list[str] = []
    for label, pattern in CONDITIONAL_MIGRATION_SQL:
        if pattern.search(source):
            errors.append(
                f"{path.relative_to(root)}: migration SQL uses conditional `{label}`; "
                "the scoped ledger must be the only apply/skip decision"
            )
    return errors


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
    """Check one version identity, one registration, and deterministic SQL."""
    errors: list[str] = []
    crates = repo_root / "crates"
    session_schema = repo_root / SESSION_SCHEMA
    if session_schema.exists():
        errors.extend(
            _session_registry_violations(session_schema.read_text(encoding="utf-8"))
        )

    rust_sources = {
        path: _production_rust(path.read_text(encoding="utf-8"))
        for path in sorted(crates.rglob("*.rs"))
        if "tests" not in path.parts
    }
    dialect_groups: dict[tuple[Path, str], dict[str, Path]] = {}
    for path in sorted(crates.rglob("*.sql")):
        dialect = DIALECT_SQL.fullmatch(path.name)
        if path.parent.name == "migrations" and dialect:
            key = (path.parent, dialect.group("identity"))
            dialect_groups.setdefault(key, {})[dialect.group("dialect")] = path
            continue
        errors.extend(_conditional_errors(path, path.read_text(encoding="utf-8"), repo_root))

        include = f'include_str!("migrations/{path.name}")'
        owners = [source_path for source_path, source in rust_sources.items() if include in source]
        if len(owners) != 1:
            errors.append(
                f"{path.relative_to(repo_root)}: versioned SQL must be registered by exactly "
                f"one Migration declaration (found {len(owners)})"
            )
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
            source = rust_sources.get(source_path, "")
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

    registration = re.compile(
        r'"(?P<registered>V[0-9]{4}__[a-z0-9_]+\.sql)"\s*,\s*'
        r'include_str!\("migrations/(?P<included>V[0-9]{4}__[a-z0-9_]+\.sql)"\)'
    )
    for path, source in rust_sources.items():
        migration_source = _migration_declarations(source)
        owns_migration = bool(migration_source)
        if DDL.search(source) and not owns_migration:
            errors.append(
                f"{path.relative_to(repo_root)}: production DDL is not owned by a versioned Migration"
            )
        if owns_migration:
            errors.extend(_conditional_errors(path, migration_source, repo_root))
            for constructor in LEGACY_CONSTRUCTORS:
                if constructor in migration_source:
                    errors.append(
                        f"{path.relative_to(repo_root)}: `{constructor[:-1]}` is a parallel "
                        "unreleased-history path; use Migration::new/per_dialect"
                    )
        for match in registration.finditer(source):
            if match.group("registered") != match.group("included"):
                errors.append(
                    f"{path.relative_to(repo_root)}: registered migration "
                    f"{match.group('registered')} includes {match.group('included')}"
                )
    return errors


def selftest() -> None:
    """Cause/effect decision table.

    Causes: C1 versioned filename, C2 exactly one registration, C3 registered
    identity equals the included file, C4 unconditional body, C5 ordinary
    Migration constructor, C6 production DDL is migration-owned. Effects: E1
    accept one executable history; E2 reject before a database connection.

    Decision table: M1 all true -> E1; M2 !C1 -> E2; M3 !C2 -> E2; M4 !C3 ->
    E2; M5 !C4 -> E2; M6 !C5 -> E2; M7 !C6 -> E2. M8 test-only DDL and M9
    runtime DML are outside the migration source; M10 paired dialect files share
    one version identity; M11 Session cannot retain a legacy registry.
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
    for source in (
        "CREATE TABLE IF NOT EXISTS x(id TEXT)",
        "DROP TABLE IF EXISTS x",
        "CREATE OR REPLACE VIEW x AS SELECT 1",
        "INSERT OR IGNORE INTO x VALUES (1)",
        "INSERT INTO x VALUES (1) ON CONFLICT(id) DO NOTHING",
    ):
        assert any(pattern.search(source) for _, pattern in CONDITIONAL_MIGRATION_SQL)  # M5
    legacy = 'Migration::published_legacy(9, "x", "CREATE TABLE x(id INT)", "sum")'
    assert any(constructor in legacy for constructor in LEGACY_CONSTRUCTORS)  # M6
    assert _migration_declarations(legacy) == _migration_declarations(
        "#[cfg(feature = \"test-support\")]\nuse fixture::Store;\n" + legacy
    )  # M8
    postgres = DIALECT_SQL.fullmatch("V0025__nonnegative_authority.postgres.sql")
    sqlite = DIALECT_SQL.fullmatch("V0025__nonnegative_authority.sqlite.sql")
    assert postgres and sqlite
    assert postgres.group("identity") == sqlite.group("identity")  # M9
    assert not DIALECT_SQL.fullmatch("V0025__nonnegative_authority.mysql.sql")
    registry = "fn published() { Migration::published_legacy(); }\nMigration::new(22);"
    assert _session_registry_violations(registry)  # M11 duplicate history path
    assert _session_registry_violations("Migration::new(1);") == []  # M1
