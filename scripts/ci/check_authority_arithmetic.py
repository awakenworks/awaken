#!/usr/bin/env python3
"""Reject arithmetic patterns that can silently rewrite durable authority."""

from __future__ import annotations

import sys
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
    "scripts/e2e/memoryd_container_e2e.sh": ("timeout --foreground",),
}

PERSISTENCE_PORT_MARKERS = ("impl CommitCoordinator for", "impl RunRecoverySource for")
PERSISTED_INTEGER_FORBIDDEN = (" as i64", " as u64", ".max(0)")


def _production_source(path: Path) -> str:
    return path.read_text().split("#[cfg(test)]", maxsplit=1)[0]


def persistence_authority_sources(root: Path = ROOT) -> list[Path]:
    """Discover persistence adapters from the ports they implement.

    The guard follows implementations instead of a hand-maintained crate list,
    so adding another Commit/Recovery backend automatically puts all of its
    production source under the same integer-boundary rule.
    """
    discovered: set[Path] = set()
    for manifest in (root / "crates").rglob("Cargo.toml"):
        crate = manifest.parent
        sources = list((crate / "src").rglob("*.rs"))
        if "awaken-store-schema" not in manifest.read_text():
            continue
        if any(
            marker in _production_source(source)
            for source in sources
            for marker in PERSISTENCE_PORT_MARKERS
        ):
            discovered.update(sources)
    return sorted(discovered)


def violations(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    for relative, forbidden in RULES.items():
        text = (root / relative).read_text()
        for token in forbidden:
            if token in text:
                errors.append(f"{relative}: forbidden authority arithmetic {token!r}")
    for path in persistence_authority_sources(root):
        text = _production_source(path)
        relative = path.relative_to(root)
        for token in PERSISTED_INTEGER_FORBIDDEN:
            if token in text:
                errors.append(
                    f"{relative}: persisted authority must use the shared StoredU64 codec; "
                    f"forbidden {token!r}"
                )
    return errors


def self_test() -> int:
    assert all(isinstance(value, tuple) and value for value in RULES.values())
    assert any(" as i64" in tokens for tokens in RULES.values())
    sources = {path.as_posix() for path in persistence_authority_sources()}
    assert any("awaken-store-postgres/src/lib.rs" in path for path in sources)
    assert any("awaken-store-sqlite/src/lib.rs" in path for path in sources)
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
