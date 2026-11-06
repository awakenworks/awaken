"""One Session realization exclusion for every phase-driver entry point."""

from __future__ import annotations

import re
from pathlib import Path

from _session_effect_fitness import _rust_function

# One Session realization exclusion, on both sides of the driver.
#
# The durable lease fences other processes; it cannot fence the request path and
# the recovery sweep inside one process. `drive_session_realization` therefore
# takes its Session from a `SessionRealizationPermit`, so a phase driver that
# does not hold the exclusion no longer type-checks. What the compiler still
# cannot see is a *new* entry point that mints its own permit from a throwaway
# registry, or a future edit that puts a bare Session id back beside the permit
# and lets the driver run for a Session the permit does not name.
REALIZATION_DRIVER_DEFINITION = (
    "crates/platform/session/awaken-session-realization-driver/src/session_realization_driver.rs"
)
REALIZATION_PERMIT_DEFINITION = (
    "crates/platform/session/awaken-session-execution-contract/src/session_realization/exclusion.rs"
)
# The exclusion identity sources: the Worker-side installed Session
# allocation, the co-located driver's per-Session registry, and the test-only
# in-memory registry of the realization crate.
REALIZATION_PERMIT_MINTERS = {
    "crates/platform/execution/awaken-session-executor/src/installed_session/realization_permit.rs": (
        "installed.realization"
    ),
    "crates/platform/session/awaken-session-application/src/realization/local_lock.rs": (
        "registry.entry(session_id.to_string())"
    ),
    # Test support only: the realization crate's in-memory installed-Session
    # registry, whose own allocation is the identity, exactly as the host's.
    "crates/platform/execution/awaken-session-realization/src/test_support.rs": (
        "installed.realization.execution().phase"
    ),
}
# The only two entry points that may reach the application-owned phase driver,
# and the canonical accessor each must obtain its permit from.
REALIZATION_DRIVER_ENTRIES = {
    "crates/platform/execution/awaken-session-executor/src/application/worker_projection.rs": (
        "sessions().acquire_realization("
    ),
    "crates/platform/session/awaken-session-application/src/realization.rs": (
        "self.local_realization_locks.acquire(session_id.as_str())"
    ),
}
# The driver lives in `awaken-session-realization-driver`, re-exported by
# `awaken-session-application`: the application reaches it through `crate::`,
# every other crate through either package path. Every spelling names the same
# single driver.
REALIZATION_DRIVER_CALL = re.compile(
    r"\b(?:awaken_session_application|awaken_session_realization_driver|crate)::drive_session_realization\s*\("
)
# The co-located registry type has exactly one owner and one holder, the
# realization component; a second instance would be a second exclusion.
LOCAL_REALIZATION_REGISTRY = "LocalRealizationLocks"
LOCAL_REALIZATION_REGISTRY_HOLDERS = (
    "crates/platform/session/awaken-session-application/src/realization.rs",
    "crates/platform/session/awaken-session-application/src/realization/local_lock.rs",
)


def _without_whitespace(text: str) -> str:
    return re.sub(r"\s+", "", text)


def realization_driver_permit_violations(sources: dict[str, str]) -> list[str]:
    """Every realization phase-driver entry acquires the one Session permit.

    Cause/effect graph. C1 the contract entry takes a permit and derives its
    Session from it; C2 the permit is minted only by the two declared exclusion
    identity sources; C3 the phase driver is called only from the two
    declared entry points; C4 each such entry obtains its permit from the
    canonical per-Session accessor; C5 the co-located registry has exactly one
    owning module. E1 accept; E2 reject and name the bypass.
    """
    errors: list[str] = []

    driver = sources.get(REALIZATION_DRIVER_DEFINITION, "")
    signature = _rust_function(driver, "drive_session_realization")
    if signature is None:
        errors.append(
            f"{REALIZATION_DRIVER_DEFINITION}: the canonical realization phase "
            "driver is missing"
        )
    else:
        header = signature[: signature.find("{") if "{" in signature else len(signature)]
        if "permit: &SessionRealizationPermit" not in header:
            errors.append(
                f"{REALIZATION_DRIVER_DEFINITION}: drive_session_realization must take "
                "`permit: &SessionRealizationPermit`; without it a caller can start a "
                "phase driver without holding the Session's exclusion"
            )
        if re.search(r"\bsession_id\s*:\s*&(?:str|SessionId)\b", header):
            errors.append(
                f"{REALIZATION_DRIVER_DEFINITION}: drive_session_realization must not take a "
                "Session id beside its permit; the two can disagree and the driver would "
                "then run for a Session the exclusion does not cover"
            )
        if "permit.session_id()" not in signature:
            errors.append(
                f"{REALIZATION_DRIVER_DEFINITION}: drive_session_realization must take its "
                "Session from `permit.session_id()`"
            )

    if REALIZATION_PERMIT_DEFINITION not in sources:
        errors.append(f"{REALIZATION_PERMIT_DEFINITION}: the permit definition is missing")

    minted = sorted(
        relative
        for relative, text in sources.items()
        if relative != REALIZATION_PERMIT_DEFINITION
        and re.search(r"\bSessionRealizationPermit::acquire\s*[(:]", text)
    )
    if minted != sorted(REALIZATION_PERMIT_MINTERS):
        errors.append(
            "Session realization permit minting owner mismatch: expected "
            f"{sorted(REALIZATION_PERMIT_MINTERS)!r}, got {minted!r}; a permit minted "
            "outside a declared per-Session exclusion identity source excludes nothing"
        )
    for relative, identity in REALIZATION_PERMIT_MINTERS.items():
        text = sources.get(relative, "")
        if text and identity not in text:
            errors.append(
                f"{relative}: the declared exclusion identity {identity!r} is gone; a permit "
                "minted from an allocation nothing else registers is not exclusion"
            )

    called = sorted(
        relative
        for relative, text in sources.items()
        if REALIZATION_DRIVER_CALL.search(text)
    )
    if called != sorted(REALIZATION_DRIVER_ENTRIES):
        errors.append(
            "Session realization phase-driver entry mismatch: expected "
            f"{sorted(REALIZATION_DRIVER_ENTRIES)!r}, got {called!r}; a new entry point must "
            "acquire the Session's exclusion through a declared accessor first"
        )
    for relative, accessor in REALIZATION_DRIVER_ENTRIES.items():
        # Compared without whitespace: rustfmt breaks a long accessor chain
        # across lines, and where the line wraps is not part of the rule.
        text = _without_whitespace(sources.get(relative, ""))
        if text and _without_whitespace(accessor) not in text:
            errors.append(
                f"{relative}: a realization phase-driver entry must obtain its permit from "
                f"{accessor!r}, not from a registry constructed at the call site"
            )

    registries = sorted(
        relative
        for relative, text in sources.items()
        if LOCAL_REALIZATION_REGISTRY in text
    )
    if registries != sorted(LOCAL_REALIZATION_REGISTRY_HOLDERS):
        errors.append(
            "Co-located realization registry owner mismatch: expected "
            f"{sorted(LOCAL_REALIZATION_REGISTRY_HOLDERS)!r}, got {registries!r}; a second "
            "registry is a second exclusion and excludes nothing"
        )
    return errors


def selftest_realization_driver_permit() -> None:
    """Decision table for the realization phase-driver permit rule.

    | Rule | Condition                                             | Effect |
    |---|---|---|
    | R1 | canonical shape                                        | accept |
    | R2 | the driver takes no permit                             | reject |
    | R3 | a Session id sits beside the permit                    | reject |
    | R4 | the driver stops deriving its Session from the permit  | reject |
    | R5 | a third module mints a permit                          | reject |
    | R6 | a declared minter loses its exclusion identity          | reject |
    | R7 | a new module calls the phase driver                     | reject |
    | R8 | a declared entry drops its canonical permit accessor    | reject |
    | R9 | a second co-located registry appears                    | reject |
    """
    canonical = {
        REALIZATION_DRIVER_DEFINITION: """
pub async fn drive_session_realization(
    permit: &SessionRealizationPermit,
    source_run_id: Option<Id>,
) -> Result<(), SessionRealizationDriveError> {
    let session_id = permit.session_id();
    loop {}
}
""",
        REALIZATION_PERMIT_DEFINITION: "pub struct SessionRealizationPermit {}\n",
        "crates/platform/execution/awaken-session-executor/src/installed_session/realization_permit.rs": """
SessionRealizationPermit::acquire(session, || installed.realization.clone(), admit)
""",
        "crates/platform/session/awaken-session-application/src/realization/local_lock.rs": """
pub(crate) struct LocalRealizationLocks();
SessionRealizationPermit::acquire(session_id, || registry.entry(session_id.to_string()), admit)
""",
        "crates/platform/execution/awaken-session-realization/src/test_support.rs": """
SessionRealizationPermit::acquire(session, || installed.realization.execution().phase.clone(), admit)
""",
        "crates/platform/execution/awaken-session-executor/src/application/worker_projection.rs": """
let realization = host.sessions().acquire_realization(session_id).await;
awaken_session_realization_driver::drive_session_realization(realization, None)
""",
        "crates/platform/session/awaken-session-application/src/realization.rs": """
pub(crate) use local_lock::LocalRealizationLocks;
local_realization_locks: Arc<LocalRealizationLocks>,
let permit = self.local_realization_locks.acquire(session_id.as_str()).await;
crate::drive_session_realization(&permit, None)
""",
    }
    assert realization_driver_permit_violations(canonical) == [], "R1 canonical shape holds"

    unpermitted = dict(canonical)
    unpermitted[REALIZATION_DRIVER_DEFINITION] = canonical[
        REALIZATION_DRIVER_DEFINITION
    ].replace("permit: &SessionRealizationPermit", "session: &str")
    assert any(
        "must take `permit: &SessionRealizationPermit`" in error
        for error in realization_driver_permit_violations(unpermitted)
    ), "R2 an unpermitted phase driver is rejected"

    parallel_id = dict(canonical)
    parallel_id[REALIZATION_DRIVER_DEFINITION] = canonical[
        REALIZATION_DRIVER_DEFINITION
    ].replace(
        "    permit: &SessionRealizationPermit,",
        "    permit: &SessionRealizationPermit,\n    session_id: &str,",
    )
    assert any(
        "must not take a Session id beside its permit" in error
        for error in realization_driver_permit_violations(parallel_id)
    ), "R3 a Session id beside the permit is rejected"

    rederived = dict(canonical)
    rederived[REALIZATION_DRIVER_DEFINITION] = canonical[
        REALIZATION_DRIVER_DEFINITION
    ].replace("permit.session_id()", '"session"')
    assert any(
        "must take its Session from `permit.session_id()`" in error
        for error in realization_driver_permit_violations(rederived)
    ), "R4 a driver that does not read its permit's Session is rejected"

    third_minter = dict(canonical)
    third_minter["crates/platform/execution/awaken-session-executor/src/application.rs"] = (
        "SessionRealizationPermit::acquire(session, || Arc::new(Mutex::new(())), admit)\n"
    )
    assert any(
        "permit minting owner mismatch" in error
        for error in realization_driver_permit_violations(third_minter)
    ), "R5 a permit minted outside a declared exclusion is rejected"

    lost_identity = dict(canonical)
    lost_identity[
        "crates/platform/execution/awaken-session-executor/src/installed_session/realization_permit.rs"
    ] = "SessionRealizationPermit::acquire(session, || Arc::new(Mutex::new(())), admit)\n"
    assert any(
        "is gone; a permit" in error
        for error in realization_driver_permit_violations(lost_identity)
    ), "R6 a minter that stops using its registered identity is rejected"

    new_entry = dict(canonical)
    new_entry["crates/platform/execution/awaken-session-executor/src/application.rs"] = (
        "awaken_session_application::drive_session_realization(permit, None)\n"
    )
    assert any(
        "phase-driver entry mismatch" in error
        for error in realization_driver_permit_violations(new_entry)
    ), "R7 a new phase-driver entry point is rejected"

    ad_hoc_permit = dict(canonical)
    ad_hoc_permit["crates/platform/session/awaken-session-application/src/realization.rs"] = (
        canonical["crates/platform/session/awaken-session-application/src/realization.rs"].replace(
            "self.local_realization_locks.acquire(session_id.as_str())",
            "LocalRealizationLocks::default().acquire(session_id.as_str())",
        )
    )
    assert any(
        "must obtain its permit from" in error
        for error in realization_driver_permit_violations(ad_hoc_permit)
    ), "R8 an entry that mints from a call-site registry is rejected"

    second_registry = dict(canonical)
    second_registry["crates/platform/execution/awaken-session-executor/src/application.rs"] = (
        "let locks = LocalRealizationLocks::default();\n"
    )
    assert any(
        "registry owner mismatch" in error
        for error in realization_driver_permit_violations(second_registry)
    ), "R9 a second co-located registry is rejected"



def check_all(repo_root: Path) -> list[str]:
    rust_sources = {
        path.relative_to(repo_root).as_posix(): path.read_text(encoding="utf-8")
        for path in (repo_root / "crates").rglob("*.rs")
    }
    return realization_driver_permit_violations(rust_sources)


def selftest() -> None:
    selftest_realization_driver_permit()


if __name__ == "__main__":
    import sys

    if sys.argv[1:] != ["--self-test"]:
        print(
            "usage: python3 scripts/ci/_realization_permit_fitness.py --self-test",
            file=sys.stderr,
        )
        raise SystemExit(2)
    selftest()
    print("_realization_permit_fitness selftest: the decision table holds")
