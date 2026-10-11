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
    # adapter implements only Worker roles without storing or deciding authority;
    # Coordinator roles are absent from its type. Accepted mutations cross the
    # authenticated Coordinator boundary. The shared erased role delegators
    # implement their selected roles over one Dispatch owner; they own no
    # SQL, claim transition, or second queue.
    "OperationCoordinator": (
        "crates/runtime/awaken-store-inmem/src/",
        "crates/stores/awaken-store-fs/src/",
        "crates/stores/awaken-store-sqlite/src/",
        "crates/stores/awaken-store-postgres/src/",
        "crates/server/awaken-session-executor/src/store.rs",
    ),
    "SessionRepository": ("crates/stores/awaken-session-store/src/",),
    "LifecycleOutbox": ("crates/stores/awaken-session-store/src/",),
}

WORKER_DISPATCH_ROLES = (
    "DispatchEnqueue", "DispatchClaims", "DispatchExecution",
    "DispatchClaimOwnership", "DispatchSettlement", "InboxRead",
)
COORDINATOR_DISPATCH_ROLES = (
    "DispatchAdministration", "DispatchCommitAuthority", "DispatchReads",
    "DispatchMaintenance", "SessionRunReservations", "SessionReservationAuthority",
    "DispatchAttemptControl", "DispatchControlClaims", "DispatchControlReads",
    "Inbox", "Outbox", "SessionContinuationOutbox", "DispatchOperationalFeed",
)
DISPATCH_OWNERS = (
    "crates/server/awaken-dispatch/src/",
    "crates/shared/awaken-dispatch-erasure/src/lib.rs",
)
for role in WORKER_DISPATCH_ROLES:
    AUTHORITY_IMPLS[role] = (*DISPATCH_OWNERS, "crates/server/awaken-coordinator-client/src/dispatch_client.rs")
for role in COORDINATOR_DISPATCH_ROLES:
    AUTHORITY_IMPLS[role] = DISPATCH_OWNERS


def dispatch_projection_violations(source: str) -> list[str]:
    """A core Worker view cannot fabricate local authority or probe for ports."""
    errors = []
    if re.search(r"\b(?:trait|impl)\s+(?:[\w:]+::)?(?:DispatchQueue|ControlDispatchQueue)\b", source):
        errors.append("use required caller roles instead of the removed broad dispatch traits")
    if re.search(r"\.(?:control|claimed_checkpoint_transport|claimed_credential_delivery_transport)\s*\(", source):
        errors.append("dispatch roles must be bound at construction, never discovered by optional getters")
    local = "|".join(COORDINATOR_DISPATCH_ROLES)
    if re.search(rf"\bimpl\s+(?:[\w:]+::)?({local})\s+for\s+AnyDispatchStore\b", source):
        errors.append("AnyDispatchStore is a Worker projection and cannot implement Coordinator roles")
    return errors

TABLE_AUTHORITIES: tuple[tuple[str, re.Pattern[str], tuple[str, ...]], ...] = (
    (
        "dispatch delivery truth",
        re.compile(
            r"\b(?:CREATE\s+TABLE|INSERT\s+INTO|UPDATE|DELETE\s+FROM)\s+"
            r"(?:\{(?:p|prefix|NS)\}|runtime)_(?:dispatch|pending|outbox|delegation_group|"
            r"dispatch_completion|stream_checkpoint|dispatch_operation)\b",
            re.IGNORECASE,
        ),
        ("crates/server/awaken-dispatch/",),
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

ATTEMPT_RESULT_OWNER = "crates/server/awaken-dispatch/src/dispatch.rs"
ATTEMPT_BACKENDS = {
    "crates/server/awaken-dispatch/src/memory/physical_attempt.rs": ("admission", "finish", "close"),
    "crates/server/awaken-dispatch/src/sqlite/physical_attempt.rs": ("admission", "finish", "close"),
    "crates/server/awaken-dispatch/src/postgres/dispatch_queue.rs": ("admission", "finish", "close"),
}


def attempt_rule_violations(relative: str, source: str) -> list[str]:
    """Adapters may apply storage effects, but cannot fork physical-slot policy."""
    errors: list[str] = []
    if relative.startswith("crates/server/awaken-dispatch/src/"):
        if relative != ATTEMPT_RESULT_OWNER and re.search(
            r"\b(?:begin|handoff|finish|can_close)_physical_attempt\s*\(", source
        ):
            errors.append(f"{relative}: physical-slot rules must use {ATTEMPT_RESULT_OWNER}")
    if relative in ATTEMPT_BACKENDS:
        for operation in ATTEMPT_BACKENDS[relative]:
            decision = f"decide_physical_attempt_{operation}"
            if not re.search(rf"\b{decision}\s*\(", source):
                errors.append(f"{relative}: missing canonical {decision} call")
    return errors


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
    non-authoritative Worker HTTP adapter or sole role delegator -> accepted;
    R3 another non-owner + same
    action -> rejected; R4 test-only implementation -> removed; R5 ordinary code
    -> preserved. These are the smallest rules that distinguish port adapters from
    database owners without treating test doubles as production authorities.
    """
    assert implementation_violation(
        "crates/server/awaken-dispatch/src/sqlite.rs", "DispatchCommitAuthority"
    ) == []  # R1
    assert implementation_violation(
        "crates/server/awaken-coordinator-client/src/dispatch_client.rs", "DispatchClaims"
    ) == []  # R2
    assert implementation_violation(
        "crates/shared/awaken-dispatch-erasure/src/lib.rs", "DispatchCommitAuthority"
    ) == []  # R2 role delegation, not a second durable authority
    assert implementation_violation(
        "crates/server/awaken-session-executor/src/dispatch.rs", "DispatchCommitAuthority"
    )  # R3
    allowed = TABLE_AUTHORITIES[2][2]
    assert table_violation("crates/stores/awaken-session-store/src/lib.rs", "session", allowed) == []
    assert table_violation("crates/server/example/src/lib.rs", "session", allowed)  # R3
    sample = "pub fn live() {}\n#[cfg(test)] mod tests { impl DispatchQueue for Fake {} }"
    stripped = _crate_boundary_workspace.production_rust(sample)
    assert "Fake" not in stripped  # R4
    assert "live" in stripped  # R5
    for role in COORDINATOR_DISPATCH_ROLES:
        assert implementation_violation("crates/server/awaken-coordinator-client/src/dispatch_client.rs", role)
        assert dispatch_projection_violations(f"impl {role} for AnyDispatchStore {{}}")
        assert dispatch_projection_violations(f"impl awaken_dispatch_contract::{role} for AnyDispatchStore {{}}")
        assert not dispatch_projection_violations(f"impl {role} for AnyCoordinatorDispatch {{}}")
    for getter in ("control", "claimed_checkpoint_transport", "claimed_credential_delivery_transport"):
        assert dispatch_projection_violations(f"store.{getter}()")
    assert dispatch_projection_violations("pub trait DispatchQueue {}")
    assert not dispatch_projection_violations("DispatchBinding::remote(owner, commit)")
    # Causes: C1 canonical result adapter or storage backend; C2 direct kernel
    # call; C3 every adapter call assigned to this file retained. Effects/rules:
    # A1 owner+C2 -> accept; A2 backend+C2 -> reject the policy bypass;
    # A3 backend+!C2+C3 -> accept; A4 backend missing any C3 call -> reject.
    # Memory and SQLite consolidate storage under their existing physical
    # attempt modules. A5 each contains admission, return and closure calls;
    # removing any moved call must still fail A4. Thin forwarding is not a
    # second decision owner.
    # Constraint: this is structural ownership evidence, not a proof of SQL
    # transaction semantics; backend conformance owns behavioral refinement.
    shared = "decide_physical_attempt_admission(x); decide_physical_attempt_finish(x); decide_physical_attempt_close(x);"
    assert attempt_rule_violations(ATTEMPT_RESULT_OWNER, "begin_physical_attempt(x)") == []
    for backend, operations in ATTEMPT_BACKENDS.items():
        assigned = " ".join(f"decide_physical_attempt_{op}(x);" for op in operations)
        assert attempt_rule_violations(backend, assigned) == []  # A5
        assert attempt_rule_violations(backend, shared) == []  # A3
        for verb in ("begin", "handoff", "finish", "can_close"):
            assert attempt_rule_violations(backend, shared + f"{verb}_physical_attempt(x)")  # A2
        for operation in operations:
            missing = shared.replace(f"decide_physical_attempt_{operation}(x);", "")
            assert attempt_rule_violations(backend, missing)  # A4


def check_all(repo_root: Path, crates_root: Path) -> list[str]:
    errors: list[str] = []
    implementation = re.compile(
        r"\bimpl(?:\s*<[^>{}]*>)?\s+(?:[\w:]+::)?(" + "|".join(AUTHORITY_IMPLS) + r")\s+for\b"
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
        errors.extend(attempt_rule_violations(relative, production))
        errors.extend(f"{relative}: {error}" for error in dispatch_projection_violations(production))
        for match in implementation.finditer(production):
            errors.extend(implementation_violation(relative, match.group(1)))
        for authority, pattern, allowed in TABLE_AUTHORITIES:
            if pattern.search(production):
                errors.extend(table_violation(relative, authority, allowed))

    for facade in (
        crates_root / "server/awaken-protocol-managed/src/lib.rs",
        crates_root / "server/awaken-session-executor/src/lib.rs",
    ):
        source = facade.read_text(encoding="utf-8")
        if re.search(
            r"pub\s+use[^;]*(?:\bSessionRepository\b|\bLifecycleOutbox\b|\bSessionStore\b|"
            r"(?:Sqlite|Postgres)ManagedSessionRepository)",
            source,
            re.DOTALL,
        ):
            errors.append(
                f"{facade.relative_to(repo_root)}: Session port/store must be imported "
                "from the owning Session contract/awaken-session-store, not re-exported by a facade"
            )
    return errors
