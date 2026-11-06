#!/usr/bin/env python3
"""Reject arithmetic patterns that can silently rewrite durable authority.

Raw integer casts in the SQL adapter modules are the compiler's: each module
denies `clippy::cast_possible_wrap`, `cast_sign_loss` and
`cast_possible_truncation` outside cfg(test) (docs/design/guard-consolidation.md
R14). Lease epochs are the compiler's too: `awaken_lease_contract::fence::Epoch` has a
private `NonZeroU64` and only `Epoch::next`; the work-store lease authority and
the Environment image-build claim hold one, so neither can add to its epoch, and
the dispatch kernel mints its successor through it. `SandboxExecutionPolicyVersion`
likewise has a private field and only `checked_next`.

What stays here is what no type or lint names: an epoch increment written in
SQL text, the Worker registry generation (still a public `u64` on
`WorkerIdentity`), a clamp (`.max(0)`) that hides a negative persisted
authority, and a codec `expect` on a durable revision. Retires when
`WorkerIdentity::generation` is a private-field newtype and every persisted
authority decodes through `StoredU64`.
"""

from __future__ import annotations

import argparse
import sys
import tempfile
from pathlib import Path

import _crate_boundary_workspace


ROOT = Path(__file__).resolve().parents[2]
RULES = {
    "crates/platform/environment/awaken-work-store/src/session_access.rs": ("lease_epoch + 1",),
    "crates/platform/dispatch/awaken-dispatch/src/postgres.rs": (".max(0)",),
    "crates/platform/dispatch/awaken-dispatch/src/sqlite.rs": (".max(0)",),
    "crates/platform/dispatch/awaken-worker-registry/src/transition.rs": ("saturating_add(1)",),
    "crates/platform/session/awaken-session-store/src/row_codec.rs": (
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
PERSISTED_INTEGER_FORBIDDEN = (".max(0)",)


def persistence_authority_sources(root: Path = ROOT) -> list[Path]:
    """Discover SQL authority adapters from crate metadata and implemented ports.

    Every crate declaring a domain authority and a SQL driver contributes its
    concrete SQL adapter files. Runtime stores built on the commit schema's codec are
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
                marker
                in _crate_boundary_workspace.production_rust(
                    source.read_text(encoding="utf-8")
                )
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
        text = "\n".join(
            _authority_lines(
                _crate_boundary_workspace.production_rust(
                    path.read_text(encoding="utf-8")
                )
            )
        )
        relative = path.relative_to(root)
        for token in PERSISTED_INTEGER_FORBIDDEN:
            if token in text:
                errors.append(
                    f"{relative}: persisted authority must use this context's StoredU64 codec; "
                    f"forbidden {token!r}"
                )
    return errors


def self_test() -> int:
    # Cause/effect graph: C1 a crate declares an authority; C2 it depends on a
    # SQL driver; C3 an adapter clamps an authority identifier (`.max(0)`);
    # C4 an adapter casts one. E1 discovery includes the adapter; E2 the clamp
    # is rejected; E3 the cast is left to the module's clippy cast lints. A
    # non-SQL authority and a non-authority clamp remain outside this rule.
    #
    # | Rule | authority | SQL | authority clamp | authority cast | Effect |
    # | D1 | yes | yes | yes | no  | E1 + E2 |
    # | D2 | yes | no  | yes | no  | not a SQL adapter |
    # | D3 | yes | yes | no (size only) | no | E1, no violation |
    # | D4 | yes | yes | no  | yes | E1 + E3, no violation here |
    assert all(isinstance(value, tuple) and value for value in RULES.values())
    assert not any(" as i64" in tokens or " as u64" in tokens for tokens in RULES.values()), "D4"
    sources = {path.as_posix() for path in persistence_authority_sources()}
    assert any("awaken-store-postgres/src/lib.rs" in path for path in sources)
    assert any("awaken-store-sqlite/src/lib.rs" in path for path in sources)
    with tempfile.TemporaryDirectory() as directory:
        fixture = Path(directory)
        # X5: a second SQL increment bypasses the proved numeric transition.
        # Green fixture -> introduce the exact old bypass -> one semantic
        # rejection -> restore -> green; no unrelated error counts as a kill.
        claim_path = fixture / "crates/platform/environment/awaken-work-store/src/session_access.rs"
        claim_path.parent.mkdir(parents=True)
        claim_path.write_text("lease_epoch = ?7", encoding="utf-8")
        assert violations(fixture) == []
        claim_path.write_text("lease_epoch = lease_epoch + 1", encoding="utf-8")
        errors = violations(fixture)
        assert len(errors) == 1 and "forbidden authority arithmetic" in errors[0], "X5"
        claim_path.write_text("lease_epoch = ?7", encoding="utf-8")
        assert violations(fixture) == []
        sql_crate = fixture / "crates" / "sql-authority"
        sql_crate.joinpath("src").mkdir(parents=True)
        sql_crate.joinpath("Cargo.toml").write_text(
            '[package.metadata.awaken]\nauthority = "fixture"\n[dependencies]\nsqlx = "0.8"\n',
            encoding="utf-8",
        )
        sql_crate.joinpath("src", "postgres.rs").write_text(
            "fn store(revision: i64) { let _ = revision.max(0); }\n"
            "fn size(bytes: i64) { let _ = bytes.max(0); }\n"
            "fn cast(revision: i64) { let _ = revision as u64; }\n",
            encoding="utf-8",
        )
        non_sql = fixture / "crates" / "non-sql-authority"
        non_sql.joinpath("src").mkdir(parents=True)
        non_sql.joinpath("Cargo.toml").write_text(
            '[package.metadata.awaken]\nauthority = "fixture"\n', encoding="utf-8"
        )
        non_sql.joinpath("src", "postgres.rs").write_text(
            "fn store(revision: i64) { let _ = revision.max(0); }\n",
            encoding="utf-8",
        )
        fixture_sources = persistence_authority_sources(fixture)
        assert fixture_sources == [sql_crate / "src" / "postgres.rs"], "D1/D2"
        fixture_errors = violations(fixture)
        assert len(fixture_errors) == 1, "D1/E2 once; D3 size clamp and D4 cast are not this rule's"
        assert "forbidden '.max(0)'" in fixture_errors[0], "D1/E2"
        assert "this context's StoredU64 codec" in fixture_errors[0], "D1/E2 names the owner"
    print("authority arithmetic guard self-test passed")
    return 0


def main(arguments: argparse.Namespace) -> int:
    if arguments.self_test:
        return self_test()
    errors = violations()
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("authority arithmetic guard passed")
    return 0


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_authority_arithmetic.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the decision table instead of the scan"
    )
    return parser.parse_args(argv)


if __name__ == "__main__":
    raise SystemExit(main(parse(sys.argv[1:])))
