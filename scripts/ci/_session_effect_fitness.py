"""Enforce domain-owned intent/receipt paths for Session external effects."""

from __future__ import annotations

from pathlib import Path


SESSION_RUNTIME = "crates/contract/awaken-session-contract/src/session.rs"
SESSION_CLEANUP = "crates/contract/awaken-session-contract/src/terminal_cleanup.rs"
SESSION_REPOSITORY = "crates/contract/awaken-session-contract/src/session_repo.rs"
SESSION_REALIZATION = "crates/contract/awaken-session-contract/src/session_realization.rs"
RESOURCE_EXECUTION = "crates/contract/awaken-resource-contract/src/execution.rs"
APPLICATION_CLEANUP = (
    "crates/server/awaken-session-application/src/resource_reconciliation.rs"
)
PROTOCOL_SESSIONS = "crates/server/awaken-protocol-managed/src/state/sessions.rs"
SESSION_STORE = "crates/stores/awaken-session-store/src/lib.rs"
RUNTIME_HOST = "crates/server/awaken-runtime-host/src/lib.rs"
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
        "SessionTerminalCleanupIntent",
        "SessionTerminalCleanupReceipt",
        "SessionTerminalCleanupState",
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
        ".execute_terminal_cleanup(intent.clone())",
        ".complete(session_id, &receipts)",
        "SessionEnvironmentState::Unmaterialized",
        ".quiesce_terminal_delegations(session_id)",
        ".freeze_targets(",
        '"terminal-cleanup-fence"',
    ),
    PROTOCOL_SESSIONS: ("release_terminal_resources(&transition.owner_scope, id)",),
    SESSION_STORE: ("SessionRepositoryError::Unavailable", "SessionRepositoryError::Corrupt"),
    RUNTIME_HOST: (
        "async fn execute_terminal_cleanup",
        "SessionTerminalCleanupReceipt::new",
        ".harvest_thread_artifacts(&intent.thread_id)",
    ),
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
    protocol = sources.get(PROTOCOL_SESSIONS, "")
    if "child_thread_ids" in protocol:
        errors.append(
            f"{PROTOCOL_SESSIONS}: protocol projection still supplies terminal cleanup targets"
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
