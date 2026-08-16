"""Enforce domain-owned intent/receipt paths for Session external effects."""

from __future__ import annotations

import re
from pathlib import Path


SESSION_RUNTIME = "crates/contract/awaken-session-contract/src/session.rs"
SESSION_CLEANUP = "crates/contract/awaken-session-contract/src/terminal_cleanup.rs"
SESSION_REPOSITORY = "crates/contract/awaken-session-contract/src/session_repo.rs"
SESSION_REALIZATION = "crates/contract/awaken-session-contract/src/session_realization.rs"
RESOURCE_EXECUTION = "crates/contract/awaken-resource-contract/src/execution.rs"
APPLICATION_CLEANUP = (
    "crates/server/awaken-session-application/src/resource_reconciliation.rs"
)
APPLICATION_TERMINAL = "crates/server/awaken-session-application/src/terminal.rs"
APPLICATION_WORK_DISPATCH = "crates/server/awaken-session-application/src/application.rs"
PROTOCOL_SESSIONS = "crates/server/awaken-protocol-managed/src/state/sessions.rs"
SESSION_STORE = "crates/stores/awaken-session-store/src/lib.rs"
SQLITE_SESSION_STORE = "crates/stores/awaken-session-store/src/sqlite.rs"
POSTGRES_SESSION_STORE = "crates/stores/awaken-session-store/src/postgres.rs"
RUNTIME_HOST = "crates/server/awaken-runtime-host/src/lib.rs"
WORKER_RUNTIME = "crates/bin/awaken-worker/src/lib.rs"
ARTIFACT_HARVEST = "crates/server/awaken-runtime-host/src/provisioning.rs"
ARTIFACT_TRANSPORT = (
    "crates/resources/awaken-resource-worker-http/src/artifact_publication_http.rs"
)


REQUIRED = {
    SESSION_RUNTIME: (
        "SessionEnvironmentReceipt",
        "publish_mcp_generation_receipt",
        "drain_mcp_generation_receipt",
        "execute_terminal_cleanup",
        "quiesce_terminal_delegations",
        "DelegatedRunSnapshot",
    ),
    SESSION_CLEANUP: (
        "SessionCleanupCommand",
        "SessionCleanupCompletion",
        "SessionCleanupOperation",
        "Fenced",
        "freeze_targets",
        "delegation_watermark",
    ),
    SESSION_REPOSITORY: (
        "SessionRepositoryRecoveryAction",
        "Unavailable(String)",
        "Corrupt(String)",
        "async fn get(&self, session_id: &str) -> Result<PersistedSession",
        "async fn owner(&self, session_id: &str) -> Result<String",
    ),
    SESSION_REALIZATION: (
        "publish_mcp_generation_receipt",
        "drain_mcp_generation_receipt",
        "McpProjectionEffectKind::Publish",
        "McpProjectionEffectKind::Drain",
    ),
    RESOURCE_EXECUTION: (
        "pub effect_id: String",
        "pub content_id: String",
        "ArtifactPublicationReceipt",
        "receipt does not match its exact publication intent",
    ),
    APPLICATION_CLEANUP: (
        ".execute_terminal_cleanup(command.clone())",
        ".complete_terminal_cleanup(",
        ".commit_delete_tombstone(owner_scope, &session)",
        ".retire_terminal_work(&session)",
        ".ensure_terminal_cleanup_fence()",
        ".quiesce_terminal_delegations(session_id)",
        ".freeze_terminal_cleanup_targets(",
        '"terminal-cleanup-fence"',
    ),
    APPLICATION_TERMINAL: (
        "SessionDeleteCommand",
        "commit_delete_intent",
        "pub async fn delete_session(",
        ".wake_lifecycle_supervisor()",
        'event_type: "session.deleted"',
    ),
    APPLICATION_WORK_DISPATCH: ("if session.is_terminal()",),
    PROTOCOL_SESSIONS: (
        "SessionDeleteCommand::new(id)",
        ".delete_session(awaken_session_application::SessionDeleteCommand::new(id))",
    ),
    SESSION_STORE: ("SessionRepositoryError::Unavailable", "SessionRepositoryError::Corrupt"),
    SQLITE_SESSION_STORE: ("current_session.admits_tombstone(",),
    POSTGRES_SESSION_STORE: ("current_session.admits_tombstone(",),
    RUNTIME_HOST: (
        "async fn execute_terminal_cleanup",
        "SessionCleanupCompletion::new",
        ".harvest_thread_artifacts(&command.thread_id)",
    ),
    WORKER_RUNTIME: (),
    ARTIFACT_HARVEST: (
        "harvest_idempotency_key",
        ".verify()",
        "verify(&publication)",
    ),
    ARTIFACT_TRANSPORT: (
        "publication.verify()",
        ".json::<ArtifactPublicationReceipt>()",
        "receipt.verify(&publication)",
    ),
}


def session_effect_violations(sources: dict[str, str]) -> list[str]:
    errors: list[str] = []
    for relative, markers in REQUIRED.items():
        text = sources.get(relative, "")
        for marker in markers:
            if marker not in text:
                errors.append(
                    f"{relative}: missing Session external-effect guard {marker!r}"
                )

    application = sources.get(APPLICATION_CLEANUP, "")
    if "runtime().end_session(" in application:
        errors.append(
            f"{APPLICATION_CLEANUP}: terminal cleanup bypasses its durable intent/receipt port"
        )
    if application.count(".retire_terminal_work(") != 1:
        errors.append(
            f"{APPLICATION_CLEANUP}: Work retirement must have one cleanup-driver call site"
        )
    if application.count(".commit_delete_tombstone(") != 1:
        errors.append(
            f"{APPLICATION_CLEANUP}: Delete tombstone must have one finalization path"
        )
    terminal = sources.get(APPLICATION_TERMINAL, "")
    if ".retire_terminal_work(" in terminal:
        errors.append(
            f"{APPLICATION_TERMINAL}: terminal edge bypasses the shared cleanup driver"
        )
    work_dispatch = sources.get(APPLICATION_WORK_DISPATCH, "")
    terminal_dispatch = work_dispatch.split("if session.is_terminal()", 1)[-1].split("}", 1)[0]
    if ".retire_session_work(" in terminal_dispatch:
        errors.append(
            f"{APPLICATION_WORK_DISPATCH}: terminal Work retirement bypasses the shared cleanup driver"
        )
    protocol = sources.get(PROTOCOL_SESSIONS, "")
    for forbidden in (
        ".begin_delete(",
        "lifecycle_event::SESSION_DELETED",
        ".commit_delete_intent(",
        ".release_terminal_resources(",
        ".notify_lifecycle_fact(",
    ):
        if forbidden in protocol:
            errors.append(
                f"{PROTOCOL_SESSIONS}: protocol adapter owns Delete authority via {forbidden!r}"
            )
    if "child_thread_ids" in protocol:
        errors.append(
            f"{PROTOCOL_SESSIONS}: protocol projection still supplies terminal cleanup targets"
        )
    if "PersistedSession {" in protocol or re.search(
        r"\bpersisted\.(?:baseline|tools|activity_epoch|environment|mcp|resources|"
        r"realization|execution|disposition|terminal_cleanup|revision)\s*=",
        protocol,
    ):
        errors.append(
            f"{PROTOCOL_SESSIONS}: protocol adapter constructs or mutates Session authority; "
            "use the aggregate constructor and SessionApplication command"
        )
    worker = sources.get(WORKER_RUNTIME, "")
    for forbidden in ("SessionCleanupCommand", "execute_terminal_cleanup"):
        if forbidden in worker:
            errors.append(
                f"{WORKER_RUNTIME}: stale Worker can settle Coordinator-owned cleanup via "
                f"{forbidden!r}"
            )
    combined = "\n".join(sources.values())
    for obsolete in (
        "try_pending_lifecycle",
        "try_reconcilable_sessions",
        "SessionRepositoryError::Storage",
        'expect("Session aggregate serializes")',
    ):
        if obsolete in combined:
            errors.append(f"Session repository compatibility escape remains: {obsolete!r}")
    return errors


def check_all(repo_root: Path) -> list[str]:
    sources = {
        relative: (repo_root / relative).read_text(encoding="utf-8")
        for relative in REQUIRED
    }
    return session_effect_violations(sources)


def selftest() -> None:
    # Cause/effect graph: C1 the Managed adapter projects committed Session
    # truth; C2 it constructs or directly mutates the aggregate. E1 C1 is
    # accepted, while E2 C2 is rejected before a second authority path lands.
    # The existing intent/receipt checks cover the independent effect bypass.
    canonical = {
        relative: "\n".join(markers) for relative, markers in REQUIRED.items()
    }
    assert session_effect_violations(canonical) == [], "canonical effect paths"

    bypass = dict(canonical)
    bypass[APPLICATION_CLEANUP] += "\nruntime().end_session(thread);"
    assert session_effect_violations(bypass), "direct terminal teardown rejected"

    missing_receipt = dict(canonical)
    missing_receipt[ARTIFACT_TRANSPORT] = missing_receipt[ARTIFACT_TRANSPORT].replace(
        "receipt.verify(&publication)", ""
    )
    assert session_effect_violations(missing_receipt), "unverified receipt rejected"

    # Mutation-test rules: removing any one critical edge must make the fitness
    # gate fail independently. This is the static complement to F0-F6 crash
    # injection and prevents one broad marker from masking a missing fence,
    # target freeze, exact settlement, or quiescence edge.
    for rule, marker in (
        ("durable fence", '"terminal-cleanup-fence"'),
        ("quiescence", ".quiesce_terminal_delegations(session_id)"),
        ("watermarked freeze", ".freeze_terminal_cleanup_targets("),
        ("exact settlement", ".complete_terminal_cleanup("),
    ):
        mutant = dict(canonical)
        mutant[APPLICATION_CLEANUP] = mutant[APPLICATION_CLEANUP].replace(marker, "")
        assert session_effect_violations(mutant), f"removed {rule} rejected"

    protocol_authority = dict(canonical)
    protocol_authority[PROTOCOL_SESSIONS] += "\nlet value = PersistedSession { ... };"
    assert session_effect_violations(protocol_authority), "protocol constructor rejected"
    protocol_authority[PROTOCOL_SESSIONS] = (
        canonical[PROTOCOL_SESSIONS] + "\npersisted.resources = Default::default();"
    )
    assert session_effect_violations(protocol_authority), "protocol mutation rejected"
    protocol_authority[PROTOCOL_SESSIONS] = (
        canonical[PROTOCOL_SESSIONS] + "\napplication.release_terminal_resources(scope, id);"
    )
    assert session_effect_violations(protocol_authority), "protocol cleanup driver rejected"

    stale_worker = dict(canonical)
    stale_worker[WORKER_RUNTIME] = "execute_terminal_cleanup(SessionCleanupCommand)"
    assert session_effect_violations(stale_worker), "Worker cleanup settlement rejected"
