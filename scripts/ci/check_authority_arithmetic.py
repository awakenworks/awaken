#!/usr/bin/env python3
"""Reject arithmetic patterns that can silently rewrite durable authority."""

from __future__ import annotations

import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RULES = {
    "crates/server/awaken-run-ingress/src/postgres.rs": (" as i64", " as u64", ".max(0)"),
    "crates/server/awaken-run-ingress/src/sqlite.rs": (" as i64", " as u64", ".max(0)"),
    "crates/server/awaken-worker-registry/src/postgres.rs": (" as i64", " as u64"),
    "crates/server/awaken-worker-registry/src/sqlite.rs": (" as i64", " as u64"),
    "crates/server/awaken-worker-registry/src/transition.rs": ("saturating_add(1)",),
    "crates/server/awaken-sandbox-policy-store/src/lib.rs": ("saturating_add(1)",),
    "crates/contract/awaken-environment-realization-contract/src/lib.rs": (
        "saturating_add(1)",
    ),
    "crates/stores/awaken-session-store/src/row_codec.rs": (
        'expect("managed Session revision',
        'expect("Session aggregate is an object',
    ),
}

PERSISTENCE_PORT_MARKERS = ("impl CommitCoordinator for", "impl RunRecoverySource for")
SQL_DEPENDENCY_MARKERS = ("sqlx", "rusqlite")
SQL_ADAPTER_NAMES = ("postgres.rs", "sqlite.rs", "row_codec.rs")
AUTHORITY_IDENTIFIERS = (
    "revision",
    "generation",
    "epoch",
    "sequence",
    "ordinal",
    "cursor",
    "version",
    "watermark",
)
PERSISTED_INTEGER_FORBIDDEN = (" as i64", " as u64", ".max(0)")


def _production_source(path: Path) -> str:
    return path.read_text().split("#[cfg(test)]", maxsplit=1)[0]


def persistence_authority_sources(root: Path = ROOT) -> list[Path]:
    """Discover SQL authority adapters from crate metadata and implemented ports.

    Every crate declaring a domain authority and a SQL driver contributes its
    concrete SQL adapter files. Runtime stores that use the shared codec are
    additionally discovered from the authoritative ports they implement. This
    keeps one mechanical discovery path without treating unrelated sizes or
    presentation timestamps as durable identity.
    """
    discovered: set[Path] = set()
    for manifest in (root / "crates").rglob("Cargo.toml"):
        crate = manifest.parent
        sources = list((crate / "src").rglob("*.rs"))
        manifest_text = manifest.read_text(encoding="utf-8")
        metadata_sql_authority = (
            'authority = "' in manifest_text
            and any(marker in manifest_text for marker in SQL_DEPENDENCY_MARKERS)
        )
        shared_runtime_authority = (
            "awaken-store-schema" in manifest_text
            and any(
                marker in _production_source(source)
                for source in sources
                for marker in PERSISTENCE_PORT_MARKERS
            )
        )
        if metadata_sql_authority:
            discovered.update(
                source for source in sources if source.name in SQL_ADAPTER_NAMES
            )
        if shared_runtime_authority:
            discovered.update(sources)
    return sorted(discovered)


def _authority_lines(source: str) -> list[str]:
    return [
        line
        for line in source.splitlines()
        if any(identifier in line.lower() for identifier in AUTHORITY_IDENTIFIERS)
    ]


def violations(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    for relative, forbidden in RULES.items():
        target = root / relative
        if not target.exists():
            continue
        text = target.read_text()
        for token in forbidden:
            if token in text:
                errors.append(f"{relative}: forbidden authority arithmetic {token!r}")
    for path in persistence_authority_sources(root):
        text = "\n".join(_authority_lines(_production_source(path)))
        relative = path.relative_to(root)
        for token in PERSISTED_INTEGER_FORBIDDEN:
            if token in text:
                errors.append(
                    f"{relative}: persisted authority must use the shared StoredU64 codec; "
                    f"forbidden {token!r}"
                )
    return errors


def self_test() -> int:
    # Cause/effect graph: C1 a crate declares an authority; C2 it depends on a
    # SQL driver; C3 an adapter performs a raw cast on an authority identifier.
    # E1 discovery includes the adapter; E2 the raw cast is rejected. A non-SQL
    # authority and a non-authority byte-size cast remain outside this rule.
    #
    # | Rule | authority | SQL | authority cast | Effect |
    # | D1 | yes | yes | yes | E1 + E2 |
    # | D2 | yes | no | yes | not a SQL adapter |
    # | D3 | yes | yes | no (size only) | E1, no violation |
    assert all(isinstance(value, tuple) and value for value in RULES.values())
    assert any(" as i64" in tokens for tokens in RULES.values())
    sources = {path.as_posix() for path in persistence_authority_sources()}
    assert any("awaken-store-postgres/src/lib.rs" in path for path in sources)
    assert any("awaken-store-sqlite/src/lib.rs" in path for path in sources)
    with tempfile.TemporaryDirectory() as directory:
        fixture = Path(directory)
        sql_crate = fixture / "crates" / "sql-authority"
        sql_crate.joinpath("src").mkdir(parents=True)
        sql_crate.joinpath("Cargo.toml").write_text(
            '[package.metadata.awaken]\nauthority = "fixture"\n[dependencies]\nsqlx = "0.8"\n',
            encoding="utf-8",
        )
        sql_crate.joinpath("src", "postgres.rs").write_text(
            "fn store(revision: u64, bytes: usize) { let _ = revision as i64; let _ = bytes as i64; }\n",
            encoding="utf-8",
        )
        non_sql = fixture / "crates" / "non-sql-authority"
        non_sql.joinpath("src").mkdir(parents=True)
        non_sql.joinpath("Cargo.toml").write_text(
            '[package.metadata.awaken]\nauthority = "fixture"\n', encoding="utf-8"
        )
        non_sql.joinpath("src", "postgres.rs").write_text(
            "fn store(revision: u64) { let _ = revision as i64; }\n",
            encoding="utf-8",
        )
        fixture_sources = persistence_authority_sources(fixture)
        assert fixture_sources == [sql_crate / "src" / "postgres.rs"], "D1/D2"
        fixture_errors = violations(fixture)
        assert len(fixture_errors) == 1 and "revision as i64" not in fixture_errors[0]
        assert "forbidden ' as i64'" in fixture_errors[0], "D1/E2; size cast is ignored"
    print("authority arithmetic guard self-test passed")
    return 0


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    errors = violations()
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("authority arithmetic guard passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
