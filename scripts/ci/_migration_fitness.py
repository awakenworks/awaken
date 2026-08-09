"""Deterministic, versioned SQL migration fitness rules."""
from __future__ import annotations

import re
from pathlib import Path


VERSIONED_SQL = re.compile(r"^V[0-9]{4}__[a-z0-9_]+\.sql$")
DDL = re.compile(
    r"\b(?:CREATE\s+(?:TABLE|INDEX|SEQUENCE|FUNCTION|TRIGGER|VIEW)|"
    r"ALTER\s+TABLE|DROP\s+(?:TABLE|INDEX|SEQUENCE|FUNCTION|TRIGGER|VIEW))\b",
    re.IGNORECASE,
)
CONDITIONAL_MIGRATION_SQL: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("IF [NOT] EXISTS", re.compile(r"\bIF\s+(?:NOT\s+)?EXISTS\b", re.IGNORECASE)),
    ("CREATE OR REPLACE", re.compile(r"\bCREATE\s+OR\s+REPLACE\b", re.IGNORECASE)),
    ("INSERT OR IGNORE", re.compile(r"\bINSERT\s+OR\s+IGNORE\b", re.IGNORECASE)),
    (
        "ON CONFLICT ... DO NOTHING",
        re.compile(r"\bON\s+CONFLICT\b[^;]*\bDO\s+NOTHING\b", re.IGNORECASE | re.DOTALL),
    ),
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


def _conditional_errors(path: Path, source: str, root: Path) -> list[str]:
    errors: list[str] = []
    for label, pattern in CONDITIONAL_MIGRATION_SQL:
        if pattern.search(source):
            errors.append(
                f"{path.relative_to(root)}: migration SQL uses conditional `{label}`; "
                "make the versioned command unconditional and let ledger state decide apply/verify"
            )
    return errors


def _migration_declarations(source: str) -> str:
    """Return the declaration region that owns inline Migration SQL.

    Some older crates keep their bundle function at the top of `lib.rs` beside
    runtime DML. The first column-zero closing brace after the final Migration
    constructor terminates that declaration region, preventing ordinary
    idempotent application writes from being mistaken for migration commands.
    """
    last = max(source.rfind("Migration::new"), source.rfind("Migration::per_dialect"))
    if last < 0:
        return ""
    end = source.find("\n}", last)
    return source if end < 0 else source[: end + 2]


def check_all(repo_root: Path) -> list[str]:
    """Check version identity, DDL ownership, and unconditional migration bodies."""
    errors: list[str] = []
    crates = repo_root / "crates"

    for path in sorted(crates.rglob("*.sql")):
        if path.parent.name != "migrations" or not VERSIONED_SQL.fullmatch(path.name):
            errors.append(
                f"{path.relative_to(repo_root)}: SQL schema file is not a versioned "
                "migrations/Vdddd__slug.sql authority"
            )
            continue
        errors.extend(_conditional_errors(path, path.read_text(encoding="utf-8"), repo_root))

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
        if owns_migration:
            errors.extend(_conditional_errors(path, migration_source, repo_root))
    return errors


def selftest() -> None:
    """Cause/effect decision table.

    M1 versioned unconditional SQL -> accepted; M2 unversioned SQL filename ->
    rejected; M3 conditional DDL or conflict-ignore migration -> rejected; M4
    production raw DDL without Migration ownership -> rejected; M5 the same DDL
    inside a Migration -> accepted; M6 inline test fixture DDL -> ignored; M7
    runtime idempotent DML after an inline bundle declaration -> ignored.
    """
    assert VERSIONED_SQL.fullmatch("V0001__catalog.sql")  # M1
    assert not VERSIONED_SQL.fullmatch("catalog.sql")  # M2
    for source in (
        "CREATE TABLE IF NOT EXISTS x(id TEXT)",
        "DROP TABLE IF EXISTS x",
        "CREATE OR REPLACE VIEW x AS SELECT 1",
        "INSERT OR IGNORE INTO x VALUES (1)",
        "INSERT INTO x VALUES (1) ON CONFLICT(id) DO NOTHING",
    ):
        assert any(pattern.search(source) for _, pattern in CONDITIONAL_MIGRATION_SQL)  # M3
    assert DDL.search(_production_rust('const SQL: &str = "CREATE TABLE x(id TEXT)";'))  # M4
    assert "Migration::new" in _production_rust(
        'Migration::new(1, "x", "CREATE TABLE {prefix}_x(id TEXT)")'
    )  # M5
    assert not DDL.search(
        _production_rust(
            '#[cfg(test)]\nmod tests { const SQL: &str = "CREATE TABLE fixture(id TEXT)"; }'
        )
    )  # M6
    mixed = """pub fn bundle() {\nMigration::new(1, \"x\", \"CREATE TABLE x(id INT)\");\n}\n\
pub fn write() { sql(\"INSERT OR IGNORE INTO x VALUES (1)\"); }"""
    assert "INSERT OR IGNORE" not in _migration_declarations(mixed)  # M7
