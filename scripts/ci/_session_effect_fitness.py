"""Enforce domain-owned intent/receipt paths for Session external effects."""

from __future__ import annotations

import re
from pathlib import Path


SESSION_RUNTIME = "crates/contract/awaken-session-contract/src/session.rs"
SESSION_CLEANUP = "crates/contract/awaken-session-contract/src/terminal_cleanup.rs"
SESSION_REPOSITORY_PUBLICATION = (
    "crates/contract/awaken-session-contract/src/"
    "terminal_cleanup/repository_publication.rs"
)
SESSION_REPOSITORY = "crates/contract/awaken-session-contract/src/session_repo.rs"
SESSION_REPOSITORY_PUBLICATION_AGGREGATE = (
    "crates/contract/awaken-session-contract/src/session_repo/repository_publication.rs"
)
SESSION_ROW_CODEC = "crates/stores/awaken-session-store/src/row_codec.rs"
SESSION_REALIZATION = "crates/contract/awaken-session-contract/src/session_realization.rs"
PROVISIONING_REPOSITORY_PUBLICATION = (
    "crates/contract/awaken-provisioning-contract/src/"
    "sandbox/repository_publication.rs"
)
RESOURCE_EXECUTION = "crates/contract/awaken-resource-contract/src/execution.rs"
APPLICATION_CLEANUP = (
    "crates/server/awaken-session-application/src/"
    "resource_reconciliation/terminal_cleanup.rs"
)
APPLICATION_PUBLICATION_CONTROL = (
    "crates/server/awaken-session-application/src/resource_reconciliation.rs"
)
APPLICATION_EVENT_BATCHES = (
    "crates/server/awaken-session-application/src/event_batches.rs"
)
APPLICATION_TERMINAL = "crates/server/awaken-session-application/src/terminal.rs"
APPLICATION_WORK_DISPATCH = "crates/server/awaken-session-application/src/application.rs"
PROTOCOL_SESSIONS = "crates/server/awaken-protocol-managed/src/state/sessions.rs"
SESSION_STORE = "crates/stores/awaken-session-store/src/lib.rs"
SQLITE_SESSION_STORE = "crates/stores/awaken-session-store/src/sqlite.rs"
POSTGRES_SESSION_STORE = "crates/stores/awaken-session-store/src/postgres.rs"
RUNTIME_HOST = "crates/server/awaken-runtime-host/src/lib.rs"
RUNTIME_HOST_APPLICATION = "crates/server/awaken-runtime-host/src/application.rs"
RUNTIME_REPOSITORY_PUBLICATION = (
    "crates/server/awaken-runtime-host/src/terminal_repository_publication.rs"
)
RUNTIME_HOST_TERMINAL = (
    "crates/server/awaken-runtime-host/src/environment_continuation.rs"
)
GIT_REPOSITORY_PUBLICATION = (
    "crates/worker/awaken-sandbox-local/src/git_transport.rs"
)
WORKER_PUBLICATION_ROUTE = (
    "crates/server/awaken-run-ingress-http/src/"
    "worker_dispatch/terminal_repository_publication.rs"
)
WORKER_CONTROL_CLIENT = (
    "crates/server/awaken-worker-runtime/src/worker_control_client.rs"
)
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
        "execute_terminal_repository_publication",
        "SessionRepositoryPublicationEffect",
    ),
    SESSION_CLEANUP: (
        "SessionCleanupCommand",
        "SessionCleanupCompletion",
        "SessionCleanupOperation",
        "Fenced",
        "freeze_targets",
        "delegation_watermark",
    ),
    SESSION_REPOSITORY_PUBLICATION: (
        "verified_repository_publication_outcome",
        "pub fn verify_for(&self, session_id: &str)",
        "pub fn repository_publication_receipt(",
        "receipt.verify(&command)?",
        "pub fn repository_publication_rejection(",
        "rejection.verify(&command)?",
        "pub fn record_repository_publication_rejection(",
        "RepositoryPublicationOutcomeMismatch",
    ),
    SESSION_REPOSITORY: (
        "SessionRepositoryRecoveryAction",
        "Unavailable(String)",
        "Corrupt(String)",
        "async fn get(&self, session_id: &str) -> Result<PersistedSession",
        "async fn owner(&self, session_id: &str) -> Result<String",
    ),
    SESSION_REPOSITORY_PUBLICATION_AGGREGATE: (
        "pub fn verified_terminal_cleanup(",
        "self.terminal_cleanup.verify_for(&self.session_id)?",
    ),
    SESSION_ROW_CODEC: ("verify_aggregate(session)?;", "verify_aggregate(&aggregate)?;"),
    SESSION_REALIZATION: (
        "publish_mcp_generation_receipt",
        "drain_mcp_generation_receipt",
        "McpProjectionEffectKind::Publish",
        "McpProjectionEffectKind::Drain",
        "record_terminal_repository_publication_rejection",
    ),
    PROVISIONING_REPOSITORY_PUBLICATION: (
        "pub expected_prior_commit: Option<String>",
        "pub enum RepositoryPublicationRejection",
        "pub enum RepositoryPublicationError",
        "canonical lowercase 40-hex object id",
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
        ".publication_command(session_id)",
        ".record_repository_publication_rejection(",
        ".verified_terminal_cleanup()",
        '"terminal-repository-publication-local-outcome"',
        "let root_commands = session",
    ),
    APPLICATION_PUBLICATION_CONTROL: (
        "record_external_terminal_repository_publication_effect",
        ".record_repository_publication_rejection(session_id, rejection)",
        '"terminal-repository-publication-worker-outcome"',
        ".verified_terminal_cleanup()",
    ),
    APPLICATION_EVENT_BATCHES: (".verified_terminal_cleanup()",),
    APPLICATION_TERMINAL: (
        "SessionDeleteCommand",
        "commit_delete_intent",
        "pub async fn delete_session(",
        ".wake_lifecycle_supervisor()",
        'event_type: "session.deleted"',
        ".repository_publication_rejection(&command.session_id)",
        ".repository_publication_receipt(&command.session_id)",
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
        ".execute_terminal_cleanup_continuation(command).await",
    ),
    RUNTIME_HOST_APPLICATION: (
        "SessionRepositoryPublicationEffect::Rejected(",
        ".record_terminal_repository_publication_rejection(",
        "let root_commands = match control.terminal_cleanup_commands(",
    ),
    RUNTIME_REPOSITORY_PUBLICATION: (
        "RepositoryPublicationActivationError::Rejected(",
        "SessionRepositoryPublicationEffect::Rejected(",
    ),
    RUNTIME_HOST_TERMINAL: (
        "async fn execute_terminal_cleanup_continuation",
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
    GIT_REPOSITORY_PUBLICATION: (
        "fn admit_repository_publication(",
        "fn classify_repository_publication_reobservation(",
        "fn push_repo_to_at(",
        "match admit_repository_publication(expectation, observed.as_deref())",
        'format!("--force-with-lease={remote_ref}:{expected_remote}")',
        "match classify_repository_publication_reobservation(",
        "RepositoryPublicationReobservation::Unchanged",
        "matches!(byte, b'0'..=b'9' | b'a'..=b'f')",
    ),
    WORKER_PUBLICATION_ROUTE: (
        "verify_terminal_cleanup_authority",
        ".record_terminal_repository_publication_receipt(",
        ".record_terminal_repository_publication_rejection(",
    ),
    WORKER_CONTROL_CLIENT: (
        "record_terminal_repository_publication_receipt",
        "record_terminal_repository_publication_rejection",
        '"/v1/worker/session/cleanup/repository-publication/reject"',
    ),
}


def _require_marker_order(
    sources: dict[str, str],
    relative: str,
    markers: tuple[str, ...],
    errors: list[str],
) -> None:
    text = sources.get(relative, "")
    positions = tuple(text.find(marker) for marker in markers)
    if all(position >= 0 for position in positions) and positions != tuple(
        sorted(positions)
    ):
        errors.append(
            f"{relative}: Session external-effect guards are out of order: "
            + " -> ".join(repr(marker) for marker in markers)
        )


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
    for relative in (
        APPLICATION_CLEANUP,
        APPLICATION_PUBLICATION_CONTROL,
        APPLICATION_EVENT_BATCHES,
    ):
        if "session.terminal_cleanup.is_completed()" in sources.get(relative, ""):
            errors.append(
                f"{relative}: completed cleanup suppression bypasses aggregate Session binding"
            )
    _require_marker_order(
        sources,
        APPLICATION_CLEANUP,
        (
            ".publication_command(session_id)",
            ".record_repository_publication_rejection(",
            '"terminal-repository-publication-local-outcome"',
            "let root_commands = session",
        ),
        errors,
    )
    _require_marker_order(
        sources,
        APPLICATION_PUBLICATION_CONTROL,
        (
            ".record_repository_publication_rejection(session_id, rejection)",
            '"terminal-repository-publication-worker-outcome"',
        ),
        errors,
    )
    _require_marker_order(
        sources,
        RUNTIME_HOST_APPLICATION,
        (
            ".record_terminal_repository_publication_rejection(",
            "let root_commands = match control.terminal_cleanup_commands(",
        ),
        errors,
    )
    _require_marker_order(
        sources,
        APPLICATION_TERMINAL,
        (
            ".repository_publication_rejection(&command.session_id)",
            ".repository_publication_receipt(&command.session_id)",
        ),
        errors,
    )
    _require_marker_order(
        sources,
        GIT_REPOSITORY_PUBLICATION,
        (
            "match admit_repository_publication(expectation, observed.as_deref())",
            'format!("--force-with-lease={remote_ref}:{expected_remote}")',
            "match classify_repository_publication_reobservation(",
        ),
        errors,
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


def repository_publication_writer_violations(sources: dict[str, str]) -> list[str]:
    """Keep the exact ref-lease writer in its existing Git transport owner."""
    errors: list[str] = []
    lease_marker = 'format!("--force-with-lease={remote_ref}:{expected_remote}")'
    lease_owners = sorted(
        relative for relative, text in sources.items() if lease_marker in text
    )
    if lease_owners != [GIT_REPOSITORY_PUBLICATION]:
        errors.append(
            "Repository publication ref-lease writer owner mismatch: "
            f"expected {[GIT_REPOSITORY_PUBLICATION]!r}, got {lease_owners!r}"
        )
    function_owners = sorted(
        relative
        for relative, text in sources.items()
        if re.search(r"\bfn\s+push_repo_to_at\s*\(", text)
    )
    if function_owners != [GIT_REPOSITORY_PUBLICATION]:
        errors.append(
            "Repository publication Git writer owner mismatch: "
            f"expected {[GIT_REPOSITORY_PUBLICATION]!r}, got {function_owners!r}"
        )
    return errors


def check_all(repo_root: Path) -> list[str]:
    sources = {
        relative: (repo_root / relative).read_text(encoding="utf-8")
        for relative in REQUIRED
    }
    errors = session_effect_violations(sources)
    rust_sources = {
        path.relative_to(repo_root).as_posix(): path.read_text(encoding="utf-8")
        for path in (repo_root / "crates").rglob("*.rs")
    }
    errors.extend(repository_publication_writer_violations(rust_sources))
    return errors


def selftest() -> None:
    # Cause/effect graph: C1 the Managed adapter projects committed Session
    # truth; C2 it constructs or directly mutates the aggregate. E1 C1 is
    # accepted, while E2 C2 is rejected before a second authority path lands.
    # The existing intent/receipt checks cover the independent effect bypass.
    canonical = {
        relative: "\n".join(markers) for relative, markers in REQUIRED.items()
    }
    assert session_effect_violations(canonical) == [], "canonical effect paths"
    assert repository_publication_writer_violations(canonical) == [], (
        "canonical Repository publication writer"
    )

    # G48 cause/effect rules: C1 one expected-prior declaration reaches the
    # existing local Git writer under an exact ref lease; C2 its typed outcome
    # is durably bound to the exact Session command before either local or
    # registered-Worker root cleanup. E1 accepts C1. Removing an edge, moving
    # root cleanup before the outcome CAS, or introducing a second writer must
    # produce E2, a fitness error, without relying on dynamic tests alone.
    for rule, owner, marker in (
        (
            "expected-prior contract",
            PROVISIONING_REPOSITORY_PUBLICATION,
            "pub expected_prior_commit: Option<String>",
        ),
        (
            "Session-bound receipt verification",
            SESSION_REPOSITORY_PUBLICATION,
            "receipt.verify(&command)?",
        ),
        (
            "Session-bound aggregate verification",
            SESSION_REPOSITORY_PUBLICATION_AGGREGATE,
            "self.terminal_cleanup.verify_for(&self.session_id)?",
        ),
        (
            "store encode aggregate binding",
            SESSION_ROW_CODEC,
            "verify_aggregate(session)?;",
        ),
        (
            "store decode aggregate binding",
            SESSION_ROW_CODEC,
            "verify_aggregate(&aggregate)?;",
        ),
        (
            "local cleanup completed binding",
            APPLICATION_CLEANUP,
            ".verified_terminal_cleanup()",
        ),
        (
            "Worker control completed binding",
            APPLICATION_PUBLICATION_CONTROL,
            ".verified_terminal_cleanup()",
        ),
        (
            "terminal Event completed binding",
            APPLICATION_EVENT_BATCHES,
            ".verified_terminal_cleanup()",
        ),
        (
            "canonical lowercase publication OID",
            PROVISIONING_REPOSITORY_PUBLICATION,
            "canonical lowercase 40-hex object id",
        ),
        (
            "durable rejection outcome",
            SESSION_REPOSITORY_PUBLICATION,
            "pub fn record_repository_publication_rejection(",
        ),
        (
            "local outcome CAS",
            APPLICATION_CLEANUP,
            '"terminal-repository-publication-local-outcome"',
        ),
        (
            "registered Worker outcome CAS",
            APPLICATION_PUBLICATION_CONTROL,
            '"terminal-repository-publication-worker-outcome"',
        ),
        (
            "exact Git ref lease",
            GIT_REPOSITORY_PUBLICATION,
            'format!("--force-with-lease={remote_ref}:{expected_remote}")',
        ),
        (
            "Worker rejection route",
            WORKER_PUBLICATION_ROUTE,
            ".record_terminal_repository_publication_rejection(",
        ),
        (
            "Worker rejection client",
            WORKER_CONTROL_CLIENT,
            '"/v1/worker/session/cleanup/repository-publication/reject"',
        ),
        (
            "hosted rejection effect",
            RUNTIME_HOST_APPLICATION,
            ".record_terminal_repository_publication_rejection(",
        ),
    ):
        mutant = dict(canonical)
        mutant[owner] = mutant[owner].replace(marker, "", 1)
        assert session_effect_violations(mutant), f"removed {rule} rejected"

    for rule, owner, earlier, later in (
        (
            "local outcome before root cleanup",
            APPLICATION_CLEANUP,
            '"terminal-repository-publication-local-outcome"',
            "let root_commands = session",
        ),
        (
            "Worker outcome before its root cleanup projection",
            RUNTIME_HOST_APPLICATION,
            ".record_terminal_repository_publication_rejection(",
            "let root_commands = match control.terminal_cleanup_commands(",
        ),
        (
            "registered Worker rejection before outcome CAS",
            APPLICATION_PUBLICATION_CONTROL,
            ".record_repository_publication_rejection(session_id, rejection)",
            '"terminal-repository-publication-worker-outcome"',
        ),
        (
            "permanent rejection before successful replay",
            APPLICATION_TERMINAL,
            ".repository_publication_rejection(&command.session_id)",
            ".repository_publication_receipt(&command.session_id)",
        ),
        (
            "Git admission before exact ref lease",
            GIT_REPOSITORY_PUBLICATION,
            "match admit_repository_publication(expectation, observed.as_deref())",
            'format!("--force-with-lease={remote_ref}:{expected_remote}")',
        ),
        (
            "Git ref lease before reobservation",
            GIT_REPOSITORY_PUBLICATION,
            'format!("--force-with-lease={remote_ref}:{expected_remote}")',
            "match classify_repository_publication_reobservation(",
        ),
    ):
        mutant = dict(canonical)
        mutant[owner] = (
            mutant[owner]
            .replace(earlier, "__AWAKEN_EFFECT_ORDER__", 1)
            .replace(later, earlier, 1)
            .replace("__AWAKEN_EFFECT_ORDER__", later, 1)
        )
        assert session_effect_violations(mutant), f"reordered {rule} rejected"

    duplicate_writer = dict(canonical)
    duplicate_writer["crates/example/src/duplicate_repository_writer.rs"] = (
        'fn push_repo_to_at() {}\n'
        'let lease = format!("--force-with-lease={remote_ref}:{expected_remote}");'
    )
    assert repository_publication_writer_violations(duplicate_writer), (
        "second Repository publication writer rejected"
    )

    bypass = dict(canonical)
    bypass[APPLICATION_CLEANUP] += "\nruntime().end_session(thread);"
    assert session_effect_violations(bypass), "direct terminal teardown rejected"

    missing_receipt = dict(canonical)
    missing_receipt[ARTIFACT_TRANSPORT] = missing_receipt[ARTIFACT_TRANSPORT].replace(
        "receipt.verify(&publication)", ""
    )
    assert session_effect_violations(missing_receipt), "unverified receipt rejected"

    # Cause/effect rules: C1 the runtime trait entry delegates to the sole
    # continuation owner; C2 that owner harvests artifacts and constructs the
    # verified completion. Removing any edge must produce E1, a fitness error,
    # rather than letting a forwarding shell masquerade as effect ownership.
    for rule, owner, marker in (
        (
            "terminal cleanup delegation",
            RUNTIME_HOST,
            ".execute_terminal_cleanup_continuation(command).await",
        ),
        (
            "terminal artifact harvest",
            RUNTIME_HOST_TERMINAL,
            ".harvest_thread_artifacts(&command.thread_id)",
        ),
        (
            "terminal completion construction",
            RUNTIME_HOST_TERMINAL,
            "SessionCleanupCompletion::new",
        ),
    ):
        mutant = dict(canonical)
        mutant[owner] = mutant[owner].replace(marker, "")
        assert session_effect_violations(mutant), f"removed {rule} rejected"

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
