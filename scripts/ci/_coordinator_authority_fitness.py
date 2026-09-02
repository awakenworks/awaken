"""Fitness rules for the three Coordinator authorities documented by ADR-0065/0066.

The rules deliberately inspect implementations and durable writes separately: a crate
dependency proves compile-time direction, while these checks prove that a second
implementation or database writer has not quietly become another source of truth.
"""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace


AUTHORITY_IMPLS: dict[str, tuple[str, ...]] = {
    # Durable implementations remain in run-ingress. The one Worker-side HTTP
    # adapter implements the same port without storing or deciding authority;
    # its server-owned verbs fail closed and all accepted mutations cross the
    # authenticated Coordinator boundary.
    "DispatchQueue": (
        "crates/server/awaken-run-ingress/src/",
        "crates/server/awaken-worker-runtime/src/dispatch_client.rs",
    ),
    "OperationCoordinator": (
        "crates/runtime/awaken-store-inmem/src/",
        "crates/stores/awaken-store-fs/src/",
        "crates/stores/awaken-store-sqlite/src/",
        "crates/stores/awaken-store-postgres/src/",
        "crates/server/awaken-runtime-host/src/store.rs",
    ),
    "ManagedSessionRepository": ("crates/stores/awaken-session-store/src/",),
}

TABLE_AUTHORITIES: tuple[tuple[str, re.Pattern[str], tuple[str, ...]], ...] = (
    (
        "dispatch delivery truth",
        re.compile(
            r"\b(?:CREATE\s+TABLE|INSERT\s+INTO|UPDATE|DELETE\s+FROM)\s+"
            r"(?:\{(?:p|prefix|NS)\}|runtime)_(?:dispatch|pending|outbox|delegation_group|"
            r"dispatch_completion|stream_checkpoint|dispatch_operation)\b",
            re.IGNORECASE,
        ),
        ("crates/server/awaken-run-ingress/",),
    ),
    (
        "committed run/thread truth",
        re.compile(
            r"\b(?:CREATE\s+TABLE|INSERT\s+INTO|UPDATE|DELETE\s+FROM)\s+"
            r"(?:\{(?:p|prefix|NS)\}|runtime)_(?:commit|message|state_command|event|run_record|"
            r"waiting|thread_version|commit_receipt)\b",
            re.IGNORECASE,
        ),
        (
            "crates/stores/awaken-store-schema/",
            "crates/stores/awaken-store-fs/",
            "crates/stores/awaken-store-sqlite/",
            "crates/stores/awaken-store-postgres/",
        ),
    ),
    (
        "managed Session truth",
        re.compile(
            r"\b(?:CREATE\s+TABLE|INSERT\s+INTO|UPDATE|DELETE\s+FROM)\s+"
            r"(?:\{(?:p|prefix|NS)\}|managed)_(?:session|session_idempotency|"
            r"session_tombstone|lifecycle_outbox)\b",
            re.IGNORECASE,
        ),
        ("crates/stores/awaken-session-store/",),
    ),
)


def _owned_by(relative_path: str, allowed: tuple[str, ...]) -> bool:
    return any(relative_path == prefix or relative_path.startswith(prefix) for prefix in allowed)


def implementation_violation(relative_path: str, trait_name: str) -> list[str]:
    """Pure ownership predicate used by both the scan and its decision table."""
    allowed = AUTHORITY_IMPLS[trait_name]
    if _owned_by(relative_path, allowed):
        return []
    return [
        f"{relative_path}: implements Coordinator authority `{trait_name}` outside its "
        f"canonical owner {list(allowed)}"
    ]


def table_violation(
    relative_path: str, authority: str, allowed: tuple[str, ...]
) -> list[str]:
    if _owned_by(relative_path, allowed):
        return []
    return [
        f"{relative_path}: writes {authority} outside its canonical persistence owner "
        f"{list(allowed)}"
    ]


def selftest() -> None:
    """Cause/effect decision table:

    R1 canonical durable owner + implementation/write -> accepted; R2 the one
    non-authoritative Worker HTTP adapter -> accepted; R3 another non-owner + same
    action -> rejected; R4 test-only implementation -> removed; R5 ordinary code
    -> preserved. These are the smallest rules that distinguish port adapters from
    database owners without treating test doubles as production authorities.
    """
    assert implementation_violation(
        "crates/server/awaken-run-ingress/src/sqlite.rs", "DispatchQueue"
    ) == []  # R1
    assert implementation_violation(
        "crates/server/awaken-worker-runtime/src/dispatch_client.rs", "DispatchQueue"
    ) == []  # R2
    assert implementation_violation(
        "crates/server/awaken-runtime-host/src/dispatch.rs", "DispatchQueue"
    )  # R3
    allowed = TABLE_AUTHORITIES[2][2]
    assert table_violation("crates/stores/awaken-session-store/src/lib.rs", "session", allowed) == []
    assert table_violation("crates/server/example/src/lib.rs", "session", allowed)  # R3
    sample = "pub fn live() {}\n#[cfg(test)] mod tests { impl DispatchQueue for Fake {} }"
    stripped = _crate_boundary_workspace.production_rust(sample)
    assert "Fake" not in stripped  # R4
    assert "live" in stripped  # R5


def check_all(repo_root: Path, crates_root: Path) -> list[str]:
    errors: list[str] = []
    implementation = re.compile(
        r"\bimpl(?:\s*<[^>{}]*>)?\s+(DispatchQueue|OperationCoordinator|"
        r"ManagedSessionRepository)\s+for\b"
    )
    for path in sorted(crates_root.glob("**/*")):
        if not path.is_file() or path.suffix not in {".rs", ".sql"}:
            continue
        relative = path.relative_to(repo_root).as_posix()
        if "tests" in path.relative_to(crates_root).parts:
            continue
        source = path.read_text(encoding="utf-8")
        production = (
            _crate_boundary_workspace.production_rust(source)
            if path.suffix == ".rs"
            else source
        )
        for match in implementation.finditer(production):
            errors.extend(implementation_violation(relative, match.group(1)))
        for authority, pattern, allowed in TABLE_AUTHORITIES:
            if pattern.search(production):
                errors.extend(table_violation(relative, authority, allowed))

    for facade in (
        crates_root / "server/awaken-protocol-managed/src/lib.rs",
        crates_root / "server/awaken-runtime-host/src/lib.rs",
    ):
        source = facade.read_text(encoding="utf-8")
        if re.search(
            r"pub\s+use[^;]*(?:ManagedSessionRepository|"
            r"(?:Sqlite|Postgres)ManagedSessionRepository)",
            source,
            re.DOTALL,
        ):
            errors.append(
                f"{facade.relative_to(repo_root)}: Session port/store must be imported "
                "from awaken-session-contract/awaken-session-store, not re-exported by a facade"
            )
    return errors
