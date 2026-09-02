"""Enforce the Session aggregate's bounded-context state ownership."""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace


SESSION_CONTRACT = "crates/contract/awaken-session-contract/src/session_repo.rs"
SESSION_PERSISTED_CONTRACT = (
    "crates/contract/awaken-session-contract/src/session_repo/persisted_session.rs"
)
SESSION_DISPOSITION_CONTRACT = (
    "crates/contract/awaken-session-contract/src/session_repo/disposition.rs"
)
SESSION_EXECUTION_STATE_CONTRACT = (
    "crates/contract/awaken-session-contract/src/session_repo/execution_state.rs"
)
SESSION_MUTATION_ROOTS = (
    "crates/server/awaken-session-application/src",
    "crates/server/awaken-protocol-managed/src",
    "crates/stores/awaken-session-store/src",
)

DIRECT_STATE_WRITE = re.compile(r"\.(?:execution|disposition)\s*=(?!=)")
RETIRED_SESSION_STATE = re.compile(
    r"\bSessionLifecycleState\b|\bSessionExecutionState::Deleted\b"
)


def _is_test_module(relative: str) -> bool:
    """Recognize source modules whose parent declaration is test-only."""
    parts = Path(relative).parts
    try:
        source_index = parts.index("src")
    except ValueError:
        return False
    return parts[-1] == "tests.rs" or "tests" in parts[source_index + 1 : -1]


def session_state_ownership_violations(sources: dict[str, str]) -> list[str]:
    errors: list[str] = []
    contract = sources.get(SESSION_CONTRACT, "")
    persisted_contract = sources.get(SESSION_PERSISTED_CONTRACT, "")
    disposition_contract = sources.get(SESSION_DISPOSITION_CONTRACT, "")
    execution_contract = sources.get(SESSION_EXECUTION_STATE_CONTRACT, "")
    if not re.search(
        r"\bmod\s+disposition\s*;.*?pub\s+use\s+disposition::\{",
        contract,
        re.S,
    ):
        errors.append(
            f"{SESSION_CONTRACT}: missing private disposition module and public re-export"
        )
    if not re.search(
        r"\bmod\s+execution_state\s*;\s*pub\s+use\s+execution_state::\{",
        contract,
        re.S,
    ):
        errors.append(
            f"{SESSION_CONTRACT}: missing private execution_state module and public re-export"
        )
    if "pub enum SessionDisposition" not in disposition_contract:
        errors.append(
            f"{SESSION_DISPOSITION_CONTRACT}: missing authoritative Session state declaration "
            "'pub enum SessionDisposition'"
        )
    for declaration in (
        "pub execution: SessionExecutionState",
        "pub disposition: SessionDisposition",
        "pub fn transition_execution",
        "pub fn archive",
        "pub fn request_delete",
    ):
        if declaration not in persisted_contract:
            errors.append(
                f"{SESSION_PERSISTED_CONTRACT}: missing authoritative Session state declaration "
                f"{declaration!r}"
            )
    if "pub enum SessionExecutionState" not in execution_contract:
        errors.append(
            f"{SESSION_EXECUTION_STATE_CONTRACT}: missing authoritative Session state "
            "declaration 'pub enum SessionExecutionState'"
        )
    if "pub enum SessionExecutionState" in contract:
        errors.append(
            f"{SESSION_CONTRACT}: SessionExecutionState duplicates the private child owner"
        )
    if "pub enum SessionDisposition" in contract:
        errors.append(
            f"{SESSION_CONTRACT}: SessionDisposition duplicates the private child owner"
        )
    if "pub archived_at:" in contract:
        errors.append(
            f"{SESSION_CONTRACT}: archived_at is a projection of SessionDisposition, "
            "not a parallel aggregate field"
        )

    for relative, text in sources.items():
        production = (
            ""
            if _is_test_module(relative)
            else _crate_boundary_workspace.production_rust(text)
        )
        if RETIRED_SESSION_STATE.search(production):
            errors.append(
                f"{relative}: retired one-dimensional Session state; use "
                "SessionExecutionState plus SessionDisposition"
            )
        if relative in (
            SESSION_CONTRACT,
            SESSION_PERSISTED_CONTRACT,
            SESSION_DISPOSITION_CONTRACT,
            SESSION_EXECUTION_STATE_CONTRACT,
        ):
            continue
        if relative.startswith(SESSION_MUTATION_ROOTS) and DIRECT_STATE_WRITE.search(production):
            errors.append(
                f"{relative}: direct Session state write outside the aggregate; use "
                "PersistedSession transition methods through SessionApplication"
            )
    return errors


def check_all(repo_root: Path) -> list[str]:
    sources = {
        str(path.relative_to(repo_root)): path.read_text(encoding="utf-8")
        for path in sorted((repo_root / "crates").glob("**/src/**/*.rs"))
    }
    return session_state_ownership_violations(sources)


def selftest() -> None:
    owner = """
mod disposition;
pub use disposition::{SessionDisposition, SessionDispositionTransitionError};
mod execution_state;
pub use execution_state::{SessionExecutionState, SessionExecutionStateError};
"""
    persisted = """
pub struct PersistedSession {
    pub execution: SessionExecutionState,
    pub disposition: SessionDisposition,
}
impl PersistedSession {
    pub fn transition_execution(&mut self) {}
    pub fn archive(&mut self) {}
    pub fn request_delete(&mut self) {}
}
"""
    sources = {
        SESSION_CONTRACT: owner,
        SESSION_PERSISTED_CONTRACT: persisted,
        SESSION_DISPOSITION_CONTRACT: (
            "pub enum SessionDisposition { Active, Archived }"
        ),
        SESSION_EXECUTION_STATE_CONTRACT: (
            "pub enum SessionExecutionState { Idle, Terminated }"
        ),
        f"{SESSION_MUTATION_ROOTS[0]}/activity.rs": "session.transition_execution();",
        f"{SESSION_MUTATION_ROOTS[1]}/sessions.rs": "project(session.execution);",
        f"{SESSION_MUTATION_ROOTS[2]}/row_codec.rs": "decode(SessionDisposition::Active);",
    }
    assert session_state_ownership_violations(sources) == [], "canonical ownership"

    # Cause/effect decision rules: R1 the root declares and re-exports the two
    # private child owners => accepted; R2 the root also declares either enum
    # => duplicate authority rejected. Constraint: responsibility splitting
    # may move code, but it cannot create a second Session state declaration.
    duplicate = dict(sources)
    duplicate[SESSION_CONTRACT] += "\npub enum SessionExecutionState { Idle }"
    assert session_state_ownership_violations(duplicate), "duplicate owner rejected"

    duplicate_disposition = dict(sources)
    duplicate_disposition[SESSION_CONTRACT] += "\npub enum SessionDisposition { Active }"
    assert session_state_ownership_violations(duplicate_disposition), (
        "duplicate disposition owner rejected"
    )

    stale = dict(sources)
    stale[f"{SESSION_MUTATION_ROOTS[0]}/activity.rs"] = (
        "session.execution = SessionExecutionState::Running;"
    )
    assert session_state_ownership_violations(stale), "direct write rejected"

    retired = dict(sources)
    retired[f"{SESSION_MUTATION_ROOTS[1]}/sessions.rs"] = "SessionLifecycleState::Deleted"
    assert session_state_ownership_violations(retired), "retired state rejected"

    tests_only = dict(sources)
    tests_only[f"{SESSION_MUTATION_ROOTS[0]}/activity.rs"] = (
        "session.transition_execution();\n"
        "#[cfg(test)] mod tests { session.execution = SessionExecutionState::Running; }"
    )
    assert session_state_ownership_violations(tests_only) == [], "fixture writes allowed"

    split_tests = dict(sources)
    split_tests[f"{SESSION_MUTATION_ROOTS[0]}/tests/authority.rs"] = (
        "session.execution = SessionExecutionState::Running;"
    )
    assert session_state_ownership_violations(split_tests) == [], (
        "cfg(test) source modules remain fixture-only after responsibility splits"
    )
