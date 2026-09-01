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
SESSION_CLEANUP_DRIVER = (
    "crates/contract/awaken-session-contract/src/terminal_cleanup/driver.rs"
)
SESSION_CLEANUP_EFFECTS = (
    "crates/contract/awaken-session-contract/src/terminal_cleanup/effects.rs"
)
SESSION_CLEANUP_PROGRESS = (
    "crates/contract/awaken-session-contract/src/terminal_cleanup/progress.rs"
)
SESSION_REPOSITORY_RECOVERY = (
    "crates/contract/awaken-session-contract/src/session_repo/recovery.rs"
)
SESSION_REPOSITORY_PORT = (
    "crates/contract/awaken-session-contract/src/session_repo/repository_port.rs"
)
SESSION_REPOSITORY_PUBLICATION_AGGREGATE = (
    "crates/contract/awaken-session-contract/src/session_repo/repository_publication.rs"
)
SESSION_ROW_CODEC = "crates/stores/awaken-session-store/src/row_codec.rs"
SESSION_REALIZATION = "crates/contract/awaken-session-contract/src/session_realization.rs"
SESSION_TERMINAL_AUTHORIZATION = (
    "crates/contract/awaken-session-contract/src/"
    "session_realization/terminal_cleanup_authorization.rs"
)
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
    "crates/server/awaken-session-application/src/realization/control.rs"
)
APPLICATION_EVENT_BATCHES = (
    "crates/server/awaken-session-application/src/event_batches.rs"
)
APPLICATION_TERMINAL = "crates/server/awaken-session-application/src/terminal.rs"
APPLICATION_WORK_DISPATCH = "crates/server/awaken-session-application/src/application.rs"
APPLICATION_RESOURCE_RECONCILIATION = (
    "crates/server/awaken-session-application/src/resource_reconciliation.rs"
)
PROTOCOL_SESSIONS = "crates/server/awaken-protocol-managed/src/state/sessions.rs"
SESSION_STORE = "crates/stores/awaken-session-store/src/lib.rs"
SQLITE_SESSION_STORE = "crates/stores/awaken-session-store/src/sqlite.rs"
POSTGRES_SESSION_STORE = "crates/stores/awaken-session-store/src/postgres.rs"
RUNTIME_HOST = "crates/server/awaken-runtime-host/src/lib.rs"
RUNTIME_HOST_APPLICATION = "crates/server/awaken-runtime-host/src/application.rs"
RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE = (
    "crates/server/awaken-runtime-host/src/application/"
    "realization_control_deadline.rs"
)
RUNTIME_REPOSITORY_PUBLICATION = (
    "crates/server/awaken-runtime-host/src/terminal_repository_publication.rs"
)
RUNTIME_HOST_TERMINAL = (
    "crates/server/awaken-runtime-host/src/host/session/environment_lifecycle.rs"
)
RUNTIME_HOST_TERMINAL_PREPARATION = (
    "crates/server/awaken-runtime-host/src/host/session/"
    "environment_lifecycle/terminal_preparation.rs"
)
RUNTIME_HOST_TERMINAL_CLEANUP = (
    "crates/server/awaken-runtime-host/src/host/session/"
    "environment_lifecycle/terminal_cleanup.rs"
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
        "prepare_terminal_cleanup_for_effect",
        "acknowledge_terminal_cleanup_preparation",
        "dispose_terminal_cleanup_for_effect",
        "acknowledge_terminal_cleanup_disposal",
        "acknowledge_completed_terminal_cleanup",
        "install_terminal_cleanup_assignment",
        "quiesce_terminal_delegations",
        "DelegatedRunSnapshot",
        "execute_terminal_repository_publication_for_lease",
        "SessionRepositoryPublicationEffect",
    ),
    SESSION_CLEANUP: (
        "SessionCleanupCommand",
        "SessionCleanupOperation",
        "SessionCleanupPreparation",
        "SessionCleanupDisposalReceipt",
        "SessionTerminalCleanupAction",
        "drive_session_terminal_cleanup",
    ),
    SESSION_CLEANUP_DRIVER: (
        "SessionTerminalCleanupAction::Prepare",
        "SessionTerminalCleanupAction::Dispose",
        "authorize_terminal_cleanup_effect",
        "prepare_terminal_cleanup_for_effect",
        "record_terminal_cleanup_preparation",
        "authorize_terminal_cleanup_disposal",
        "dispose_terminal_cleanup_for_effect",
        "record_terminal_cleanup_disposal",
    ),
    SESSION_CLEANUP_EFFECTS: (
        "SessionTerminalCleanupDisposalEffect",
        "sandbox_disposal_authorization_for_current_generation",
        "realization_lease_generation_authorizes(current_lease, &self.lease)",
        "SandboxDisposalAuthorization",
        "provider_disposal.authorize(current)",
    ),
    SESSION_CLEANUP_PROGRESS: (
        "SessionCleanupPreparing",
        "SessionCleanupDisposing",
        "SessionCleanupDisposalCommand",
        "SandboxDisposalPreparation",
        "pub artifact_receipts: Vec<ArtifactPublicationReceipt>",
        "canonical_thread_artifact_receipt_fingerprint",
        "self.artifact_receipts != canonical_receipts",
    ),
    SESSION_REPOSITORY_PUBLICATION: (
        "verified_repository_publication_outcome",
        "pub(crate) fn verify_for(&self, session_id: &str)",
        "pub fn repository_publication_receipt(",
        "receipt.verify(&command)?",
        "pub fn repository_publication_rejection(",
        "rejection.verify(&command)?",
        "pub fn record_repository_publication_rejection(",
        "RepositoryPublicationOutcomeMismatch",
    ),
    SESSION_REPOSITORY_RECOVERY: (
        "SessionRepositoryRecoveryAction",
        "Self::Unavailable(_)",
        "Self::Corrupt(_)",
    ),
    SESSION_REPOSITORY_PORT: (
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
        "SessionTerminalCleanupPreparationAuthorization",
        "authorize_terminal_cleanup_effect",
        "authorize_terminal_cleanup_disposal",
        "record_terminal_cleanup_preparation",
        "record_terminal_cleanup_disposal",
        "record_terminal_repository_publication_rejection",
    ),
    SESSION_TERMINAL_AUTHORIZATION: (
        "SessionTerminalCleanupPreparationAuthorization",
        "#[serde(deny_unknown_fields)]",
        "inherited_provider_disposal",
        "verify_for",
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
        "drive_session_terminal_cleanup(",
        ".commit_delete_tombstone(owner_scope, &session)",
        ".retire_terminal_work(&session)",
        ".ensure_terminal_cleanup_fence()",
        ".quiesce_terminal_delegations(session_id)",
        ".freeze_terminal_cleanup_targets(",
        '"terminal-cleanup-fence"',
        ".verified_terminal_cleanup()",
    ),
    APPLICATION_PUBLICATION_CONTROL: (
        "record_terminal_repository_publication_rejection_after_refresh",
        ".record_terminal_repository_publication_rejection_from_root(",
        "async fn record_terminal_repository_publication_rejection(",
    ),
    APPLICATION_EVENT_BATCHES: (".verified_terminal_cleanup()",),
    APPLICATION_RESOURCE_RECONCILIATION: (
        ".terminal_cleanup_work_action()",
        ".authorize_terminal_cleanup_effect(effect)",
        "SessionTerminalCleanupPreparationAuthorization::try_new(",
        ".authorize_terminal_cleanup_disposal_effect(&owner_scope, effect)",
        ".record_terminal_cleanup_preparation(",
        '"terminal-cleanup-preparation"',
        ".record_terminal_cleanup_disposal(",
        '"terminal-cleanup-disposal"',
        ".verified_terminal_cleanup()",
        "record_terminal_repository_publication_effect_from_root",
        ".record_repository_publication_rejection(session_id, rejection)",
        '"terminal-repository-publication-worker-outcome"',
    ),
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
        "async fn prepare_terminal_cleanup_for_effect",
        "async fn acknowledge_terminal_cleanup_preparation",
        "async fn dispose_terminal_cleanup_for_effect",
        "async fn acknowledge_terminal_cleanup_disposal",
        "async fn acknowledge_completed_terminal_cleanup",
    ),
    RUNTIME_HOST_APPLICATION: ("drive_session_terminal_cleanup(",),
    RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE: (
        "async fn record_terminal_repository_publication_receipt(",
        ".record_terminal_repository_publication_receipt(",
        "async fn record_terminal_repository_publication_rejection(",
        ".record_terminal_repository_publication_rejection(",
    ),
    RUNTIME_REPOSITORY_PUBLICATION: (
        "RepositoryPublicationActivationError::Rejected(",
        "SessionRepositoryPublicationEffect::Rejected(",
    ),
    RUNTIME_HOST_TERMINAL: (),
    RUNTIME_HOST_TERMINAL_PREPARATION: (
        ".sandbox_disposal_authorization_for_current_generation(&current)",
    ),
    RUNTIME_HOST_TERMINAL_CLEANUP: (
        "authorization.verify_for(&effect)",
        ".harvest_with_fence_mode(",
        "ArtifactPublicationFence::Terminal",
        "SessionCleanupPreparation::try_new",
        ".dispose_for_effect(&authorization)",
        "SessionCleanupDisposalReceipt::new",
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
    WORKER_RUNTIME: (),
    ARTIFACT_HARVEST: (
        "harvest_idempotency_key",
        ".verify()",
        "verify(&publication)",
        "ArtifactCaptureMode::ReceiptOnly",
        "recover_terminal_receipts",
        ".publisher",
        ".recover(recovery)",
    ),
    ARTIFACT_TRANSPORT: (
        "publication.verify()",
        ".json::<ArtifactPublicationReceipt>()",
        "receipt.verify(&publication)",
    ),
}


def _production_source(source: str) -> str:
    """Exclude inline `#[cfg(test)] mod ...` items, not test-only helpers/fields."""
    test_module = re.compile(
        r"#\[cfg\(test\)\]\s*(?:#\[[^\]]+\]\s*)*mod\s+[A-Za-z0-9_]+\s*(?P<end>[;{])"
    )
    ranges: list[tuple[int, int]] = []
    for match in test_module.finditer(source):
        if match.group("end") == ";":
            ranges.append((match.start(), match.end()))
            continue
        body_start = match.end() - 1
        body_end = _matching_rust_brace(source, body_start)
        ranges.append((match.start(), len(source) if body_end is None else body_end))
    for start, end in reversed(ranges):
        source = source[:start] + source[end:]
    return source


def _matching_rust_brace(source: str, body_start: int) -> int | None:
    depth = 0
    for index in range(body_start, len(source)):
        character = source[index]
        if character == "{":
            depth += 1
        elif character == "}":
            depth -= 1
            if depth == 0:
                return index + 1
    return None


def _without_rust_comments(source: str) -> str:
    """Remove comments so a prose copy cannot satisfy an executable guard."""
    without_blocks = re.sub(r"/\*.*?\*/", "", source, flags=re.DOTALL)
    return re.sub(r"//[^\n]*", "", without_blocks)


def _rust_function(source: str, name: str) -> str | None:
    """Return the first Rust function item with a body, including that body."""
    source = _without_rust_comments(_production_source(source))
    signature = re.compile(
        rf"(?m)^[ \t]*(?:pub(?:\([^\n)]*\))?[ \t]+)?"
        rf"(?:async[ \t]+)?fn[ \t]+{re.escape(name)}\b"
    )
    for match in signature.finditer(source):
        body_start = source.find("{", match.end())
        declaration_end = source.find(";", match.end())
        if body_start < 0 or 0 <= declaration_end < body_start:
            continue
        body_end = _matching_rust_brace(source, body_start)
        if body_end is not None:
            return source[match.start() : body_end]
    return None


def _require_ordered_function(
    sources: dict[str, str],
    relative: str,
    function: str,
    markers: tuple[str, ...],
    ownership: str,
    *,
    require_tail_result: bool = False,
) -> list[str]:
    body = _rust_function(sources.get(relative, ""), function)
    if body is None:
        return [f"{relative}: missing {ownership} owner function {function!r}"]
    cursor = 0
    for marker in markers:
        position = body.find(marker, cursor)
        if position < 0:
            return [
                f"{relative}: {ownership} must contain {marker!r} in canonical order"
            ]
        cursor = position + len(marker)
    if require_tail_result and body[cursor:].strip() != "}":
        return [
            f"{relative}: {ownership} must return its awaited Control result as "
            "the function tail expression"
        ]
    return []


def session_effect_violations(sources: dict[str, str]) -> list[str]:
    errors: list[str] = []
    for relative, markers in REQUIRED.items():
        text = sources.get(relative, "")
        for marker in markers:
            if marker not in text:
                errors.append(
                    f"{relative}: missing Session external-effect guard {marker!r}"
                )

    # The shared driver, not either topology, owns the two-stage protocol.
    # These ordered checks are scoped to executable function bodies so moving a
    # marker into a comment, helper, or parallel caller cannot satisfy the gate.
    errors.extend(
        _require_ordered_function(
            sources,
            SESSION_CLEANUP_DRIVER,
            "drive_session_terminal_cleanup",
            (
                "SessionTerminalCleanupAction::Waiting",
                ".execute_terminal_repository_publication_for_lease(",
                "SessionRepositoryPublicationEffect::Published(",
                ".record_terminal_repository_publication_receipt(",
                "SessionRepositoryPublicationEffect::Rejected(",
                ".record_terminal_repository_publication_rejection(",
                "SessionTerminalCleanupAction::Prepare",
                ".authorize_terminal_cleanup_effect(&effect)",
                "authorization.verify_for(&effect)?",
                ".prepare_terminal_cleanup_for_effect(effect.clone(), authorization)",
                ".record_terminal_cleanup_preparation(&lease, preparation)",
                ".acknowledge_terminal_cleanup_preparation(&effect)",
                "SessionTerminalCleanupAction::Dispose",
                "SessionTerminalCleanupDisposalEffect::new(command, lease.clone())",
                ".authorize_terminal_cleanup_disposal(&effect)",
                ".dispose_terminal_cleanup_for_effect(effect.clone())",
                ".record_terminal_cleanup_disposal(&lease, receipt)",
                ".acknowledge_terminal_cleanup_disposal(&effect)",
            ),
            "two-stage terminal cleanup",
        )
    )
    errors.extend(
        _require_ordered_function(
            sources,
            SESSION_CLEANUP_EFFECTS,
            "sandbox_disposal_authorization_for_current_generation",
            (
                "realization_lease_generation_authorizes(current_lease, &self.lease)",
                "current_lease.sandbox_effect_fence(self.command.effect_id.clone())?",
                "self.command.provider_disposal.authorize(current)",
            ),
            "aggregate-derived Sandbox disposal authorization",
        )
    )

    realization = _production_source(sources.get(SESSION_TERMINAL_AUTHORIZATION, ""))
    closed_authorization = re.search(
        r"#\[serde\(deny_unknown_fields\)\]\s*"
        r"pub struct SessionTerminalCleanupPreparationAuthorization\s*\{(?P<body>.*?)\n\}",
        realization,
        re.DOTALL,
    )
    if closed_authorization is None:
        errors.append(
            f"{SESSION_TERMINAL_AUTHORIZATION}: terminal preparation authorization must be a "
            "deny-unknown-fields closed value"
        )
    else:
        body = closed_authorization.group("body")
        for field in (
            "effect: crate::SessionTerminalCleanupEffect",
            "workspace_id: String",
            "inherited_provider_disposal: Option<",
        ):
            if field not in body:
                errors.append(
                    f"{SESSION_TERMINAL_AUTHORIZATION}: closed terminal preparation authorization "
                    f"is missing private field {field!r}"
                )
        if re.search(r"(?m)^\s*pub(?:\([^)]*\))?\s+", body):
            errors.append(
                f"{SESSION_TERMINAL_AUTHORIZATION}: terminal preparation authorization exposes "
                "caller-authorable fields"
            )
    errors.extend(
        _require_ordered_function(
            sources,
            SESSION_TERMINAL_AUTHORIZATION,
            "try_new",
            ("let authorization = Self {", "authorization.validate()?", "Ok(authorization)"),
            "closed terminal preparation authorization",
        )
    )
    errors.extend(
        _require_ordered_function(
            sources,
            SESSION_TERMINAL_AUTHORIZATION,
            "verify_for",
            ("self.validate()?", "if &self.effect != effect"),
            "exact terminal preparation authorization",
        )
    )

    # Application Control re-derives authority from the root and commits both
    # receipt classes through that same root CAS; it owns no side queue.
    for function, markers, ownership in (
        (
            "authorize_terminal_cleanup_effect_from_root",
            (
                ".get(&effect.command.session_id)",
                "Self::require_current_live_terminal_generation(&session)?",
                ".owner(&effect.command.session_id)",
                "terminal_restore_target_for_thread(",
                ".authorize_terminal_cleanup_effect(effect)",
                "SessionTerminalCleanupPreparationAuthorization::try_new(",
            ),
            "root-derived terminal preparation authorization",
        ),
        (
            "authorize_terminal_cleanup_disposal_from_root",
            (
                ".get(&effect.command.session_id)",
                ".owner(&effect.command.session_id)",
                ".authorize_terminal_cleanup_disposal_effect(&owner_scope, effect)",
            ),
            "root-derived terminal disposal authorization",
        ),
        (
            "record_terminal_cleanup_preparation_from_root",
            (
                ".get(&session_id)",
                ".record_terminal_cleanup_preparation(",
                '"terminal-cleanup-preparation"',
            ),
            "root-CAS terminal preparation receipt",
        ),
        (
            "record_terminal_cleanup_disposal_from_root",
            (
                ".get(&session_id)",
                ".record_terminal_cleanup_disposal(",
                '"terminal-cleanup-disposal"',
            ),
            "root-CAS terminal disposal receipt",
        ),
    ):
        errors.extend(
            _require_ordered_function(
                sources,
                APPLICATION_RESOURCE_RECONCILIATION,
                function,
                markers,
                ownership,
            )
        )

    errors.extend(
        _require_ordered_function(
            sources,
            APPLICATION_RESOURCE_RECONCILIATION,
            "record_terminal_repository_publication_effect_from_root",
            (
                ".record_repository_publication_rejection(session_id, rejection)",
                '"terminal-repository-publication-worker-outcome"',
            ),
            "one root-CAS Repository publication outcome",
        )
    )
    errors.extend(
        _require_ordered_function(
            sources,
            APPLICATION_PUBLICATION_CONTROL,
            "record_terminal_repository_publication_rejection_after_refresh",
            (".record_terminal_repository_publication_rejection_from_root(",),
            "extracted Session realization rejection forwarding",
        )
    )
    errors.extend(
        _require_ordered_function(
            sources,
            APPLICATION_TERMINAL,
            "archive_with_repository_publication",
            (
                ".repository_publication_rejection(&command.session_id)",
                ".repository_publication_receipt(&command.session_id)",
            ),
            "permanent Repository rejection before successful replay",
        )
    )
    for function, effect, ownership in (
        (
            "session_repository_publication_poll",
            ".terminal_repository_publication_command(",
            "authenticated Worker Repository publication polling",
        ),
        (
            "session_repository_publication_complete",
            ".record_terminal_repository_publication_receipt(",
            "authenticated Worker Repository publication receipt",
        ),
        (
            "session_repository_publication_reject",
            ".record_terminal_repository_publication_rejection(",
            "authenticated Worker Repository publication rejection",
        ),
    ):
        errors.extend(
            _require_ordered_function(
                sources,
                WORKER_PUBLICATION_ROUTE,
                function,
                (
                    "verify_terminal_cleanup_authority(",
                    "session_control(&service)",
                    effect,
                ),
                ownership,
            )
        )
    for function, operation, forwarding, ownership in (
        (
            "record_terminal_repository_publication_receipt",
            '"record_terminal_repository_publication"',
            ".record_terminal_repository_publication_receipt(",
            "deadline-bounded hosted Repository publication receipt",
        ),
        (
            "record_terminal_repository_publication_rejection",
            '"record_terminal_repository_publication_rejection"',
            ".record_terminal_repository_publication_rejection(",
            "deadline-bounded hosted Repository publication rejection",
        ),
    ):
        errors.extend(
            _require_ordered_function(
                sources,
                RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE,
                function,
                ("self.call(", operation, forwarding, ".await"),
                ownership,
                require_tail_result=True,
            )
        )
    errors.extend(
        _require_ordered_function(
            sources,
            GIT_REPOSITORY_PUBLICATION,
            "push_repo_to_at",
            (
                "match admit_repository_publication(expectation, observed.as_deref())",
                'format!("--force-with-lease={remote_ref}:{expected_remote}")',
                "match classify_repository_publication_reobservation(",
            ),
            "single exact Repository ref-lease writer",
        )
    )

    # Host preparation may capture live output or recover an already-published
    # batch, but only the preparation half does either. The destructive half
    # consumes the exact aggregate-derived SandboxDisposalAuthorization.
    errors.extend(
        _require_ordered_function(
            sources,
            RUNTIME_HOST_TERMINAL_CLEANUP,
            "prepare_terminal_cleanup_effect",
            (
                "authorization.verify_for(&effect)",
                "self.terminal_cleanup_effect_fence(&effect)?",
                "TerminalEnvironmentPreparation::artifact_capture_mode",
                ".harvest_with_fence_mode(",
                "SessionCleanupPreparation::try_new(",
            ),
            "terminal preparation effects",
        )
    )
    errors.extend(
        _require_ordered_function(
            sources,
            RUNTIME_HOST_TERMINAL_PREPARATION,
            "terminal_cleanup_disposal_authorization",
            (
                "self.authorize_terminal_cleanup_lease(&effect.command.session_id, &effect.lease)?",
                ".sandbox_disposal_authorization_for_current_generation(&current)",
                ".effect_fence()",
                ".validate_live_at(",
            ),
            "typed Sandbox disposal authorization",
        )
    )
    errors.extend(
        _require_ordered_function(
            sources,
            RUNTIME_HOST_TERMINAL_CLEANUP,
            "dispose_terminal_cleanup_effect",
            (
                "let authorization = self.terminal_cleanup_disposal_authorization(&effect)?",
                "authorization.effect_fence()",
                ".dispose_for_effect(&authorization)",
                "self.terminal_cleanup_disposal_authorization(&effect)?",
                "SessionCleanupDisposalReceipt::new(",
            ),
            "physical terminal disposal",
        )
    )
    preparation_body = _rust_function(
        sources.get(RUNTIME_HOST_TERMINAL_CLEANUP, ""),
        "prepare_terminal_cleanup_effect",
    )
    if preparation_body is not None and ".harvest_with_fence(" in preparation_body:
        errors.append(
            f"{RUNTIME_HOST_TERMINAL_CLEANUP}: terminal preparation bypasses the "
            "ReceiptOnly-aware Artifact batch owner"
        )
    errors.extend(
        _require_ordered_function(
            sources,
            ARTIFACT_HARVEST,
            "harvest_with_fence_mode_from_environment",
            (
                "if capture_mode == ArtifactCaptureMode::ReceiptOnly && terminal_effect.is_none()",
                "if capture_mode == ArtifactCaptureMode::ReceiptOnly",
                "return self",
                ".recover_terminal_receipts(",
                "let projected_environment = exact_environment.cloned().or_else",
                ".capture_artifacts()",
            ),
            "ReceiptOnly-before-live Artifact batch",
        )
    )

    errors.extend(
        _require_ordered_function(
            sources,
            APPLICATION_CLEANUP,
            "release_terminal_resources_once",
            (".drive_local_terminal_cleanup(&assignment)",),
            "local terminal generation supervisor",
        )
    )
    errors.extend(
        _require_ordered_function(
            sources,
            APPLICATION_CLEANUP,
            "drive_local_terminal_cleanup",
            ("drive_session_terminal_cleanup(",),
            "shared topology-neutral terminal driver",
        )
    )
    errors.extend(
        _require_ordered_function(
            sources,
            RUNTIME_HOST_APPLICATION,
            "reconcile_terminal_cleanup_for_lease",
            ("drive_session_terminal_cleanup(",),
            "shared topology-neutral terminal driver",
        )
    )

    for relative in (
        SESSION_RUNTIME,
        SESSION_CLEANUP_DRIVER,
        APPLICATION_CLEANUP,
        RUNTIME_HOST,
        RUNTIME_HOST_APPLICATION,
        RUNTIME_HOST_TERMINAL,
        RUNTIME_HOST_TERMINAL_PREPARATION,
        RUNTIME_HOST_TERMINAL_CLEANUP,
    ):
        production = _production_source(sources.get(relative, ""))
        for obsolete in (
            "execute_terminal_cleanup_for_effect",
            "SessionCleanupCompletion::new",
        ):
            if obsolete in production:
                errors.append(
                    f"{relative}: removed one-stage terminal cleanup path remains via "
                    f"{obsolete!r}"
                )

    for relative in (
        APPLICATION_CLEANUP,
        APPLICATION_RESOURCE_RECONCILIATION,
        APPLICATION_EVENT_BATCHES,
    ):
        if "session.terminal_cleanup.is_completed()" in sources.get(relative, ""):
            errors.append(
                f"{relative}: completed cleanup suppression bypasses aggregate Session binding"
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
    canonical[SESSION_CLEANUP_DRIVER] = """
pub async fn drive_session_terminal_cleanup() {
    match action {
        SessionTerminalCleanupAction::Waiting => {
            let effect = runtime.execute_terminal_repository_publication_for_lease(command, &lease);
            match effect {
                SessionRepositoryPublicationEffect::Published(receipt) => {
                    control.record_terminal_repository_publication_receipt(session_id, &lease, receipt);
                }
                SessionRepositoryPublicationEffect::Rejected(rejection) => {
                    control.record_terminal_repository_publication_rejection(session_id, &lease, rejection);
                }
            }
        }
        SessionTerminalCleanupAction::Prepare => {
            control.authorize_terminal_cleanup_effect(&effect);
            authorization.verify_for(&effect)?;
            runtime.prepare_terminal_cleanup_for_effect(effect.clone(), authorization);
            control.record_terminal_cleanup_preparation(&lease, preparation);
            runtime.acknowledge_terminal_cleanup_preparation(&effect);
        }
        SessionTerminalCleanupAction::Dispose => {
            SessionTerminalCleanupDisposalEffect::new(command, lease.clone());
            control.authorize_terminal_cleanup_disposal(&effect);
            runtime.dispose_terminal_cleanup_for_effect(effect.clone());
            control.record_terminal_cleanup_disposal(&lease, receipt);
            runtime.acknowledge_terminal_cleanup_disposal(&effect);
        }
    }
}
"""
    canonical[SESSION_CLEANUP_EFFECTS] = """
SessionTerminalCleanupDisposalEffect
SandboxDisposalAuthorization
pub fn sandbox_disposal_authorization_for_current_generation() {
    if !realization_lease_generation_authorizes(current_lease, &self.lease) {}
    let current = current_lease.sandbox_effect_fence(self.command.effect_id.clone())?;
    self.command.provider_disposal.authorize(current)
}
"""
    canonical[SESSION_TERMINAL_AUTHORIZATION] = """
#[serde(deny_unknown_fields)]
pub struct SessionTerminalCleanupPreparationAuthorization {
    effect: crate::SessionTerminalCleanupEffect,
    workspace_id: String,
    inherited_provider_disposal: Option<SandboxDisposalPreparation>,
}
fn try_new() {
    let authorization = Self { effect, workspace_id, inherited_provider_disposal };
    authorization.validate()?;
    Ok(authorization)
}
fn verify_for() {
    self.validate()?;
    if &self.effect != effect {}
}
"""
    canonical[APPLICATION_CLEANUP] = """
fn release_terminal_resources_once() {
    session.verified_terminal_cleanup();
    session.ensure_terminal_cleanup_fence();
    "terminal-cleanup-fence";
    self.retire_terminal_work(&session);
    self.runtime().quiesce_terminal_delegations(session_id);
    session.freeze_terminal_cleanup_targets(thread_ids, watermark, cursor);
    self.drive_local_terminal_cleanup(&assignment);
    self.commit_delete_tombstone(owner_scope, &session);
}
fn drive_local_terminal_cleanup() {
    drive_session_terminal_cleanup(session_id, lease, self, self.runtime());
}
"""
    canonical[APPLICATION_RESOURCE_RECONCILIATION] = """
fn terminal_cleanup_projection() { session.terminal_cleanup_work_action(); }
fn authorize_terminal_cleanup_effect_from_root() {
    repository.get(&effect.command.session_id);
    Self::require_current_live_terminal_generation(&session)?;
    repository.owner(&effect.command.session_id);
    terminal_restore_target_for_thread(&workspace_id, &session, &effect.command.thread_id);
    session.authorize_terminal_cleanup_effect(effect);
    SessionTerminalCleanupPreparationAuthorization::try_new(
        effect, workspace_id, inherited_provider_disposal,
    );
}
fn authorize_terminal_cleanup_disposal_from_root() {
    repository.get(&effect.command.session_id);
    repository.owner(&effect.command.session_id);
    session.authorize_terminal_cleanup_disposal_effect(&owner_scope, effect);
}
fn record_terminal_cleanup_preparation_from_root() {
    repository.get(&session_id);
    session.record_terminal_cleanup_preparation(lease, preparation);
    "terminal-cleanup-preparation";
}
fn record_terminal_cleanup_disposal_from_root() {
    repository.get(&session_id);
    session.record_terminal_cleanup_disposal(lease, receipt);
    "terminal-cleanup-disposal";
}
fn record_terminal_repository_publication_effect_from_root() {
    repository.get(session_id);
    session.verified_terminal_cleanup();
    match effect {
        Published(receipt) => session.record_repository_publication_receipt(session_id, receipt),
        Rejected(rejection) => session.record_repository_publication_rejection(session_id, rejection),
    }
    "terminal-repository-publication-worker-outcome";
}
"""
    canonical[APPLICATION_PUBLICATION_CONTROL] = """
fn record_terminal_repository_publication_rejection_after_refresh() {
    self.record_terminal_repository_publication_rejection_from_root(session_id, lease, rejection);
}
async fn record_terminal_repository_publication_rejection() {
    self.record_terminal_repository_publication_rejection_after_refresh(session_id, lease, rejection);
}
"""
    canonical[APPLICATION_EVENT_BATCHES] = "session.verified_terminal_cleanup();"
    canonical[APPLICATION_TERMINAL] = """
fn archive_with_repository_publication() {
    durable.terminal_cleanup.repository_publication_rejection(&command.session_id);
    durable.terminal_cleanup.repository_publication_receipt(&command.session_id);
}
SessionDeleteCommand
commit_delete_intent
pub async fn delete_session(
.wake_lifecycle_supervisor()
event_type: "session.deleted"
"""
    canonical[RUNTIME_HOST_APPLICATION] = """
fn reconcile_terminal_cleanup_for_lease() {
    drive_session_terminal_cleanup(session_id, lease, control, runtime);
}
"""
    canonical[WORKER_PUBLICATION_ROUTE] = """
async fn session_repository_publication_poll() {
    verify_terminal_cleanup_authority(...).await;
    session_control(&service).terminal_repository_publication_command(...);
}
async fn session_repository_publication_complete() {
    verify_terminal_cleanup_authority(...).await;
    session_control(&service).record_terminal_repository_publication_receipt(...);
}
async fn session_repository_publication_reject() {
    verify_terminal_cleanup_authority(...).await;
    session_control(&service).record_terminal_repository_publication_rejection(...);
}
"""
    canonical[RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE] = """
async fn record_terminal_repository_publication_receipt() {
    self.call(
        "record_terminal_repository_publication",
        self.inner.record_terminal_repository_publication_receipt(...),
    )
    .await
}
async fn record_terminal_repository_publication_rejection() {
    self.call(
        "record_terminal_repository_publication_rejection",
        self.inner.record_terminal_repository_publication_rejection(...),
    )
    .await
}
"""
    canonical[RUNTIME_HOST_TERMINAL_PREPARATION] = """
fn terminal_cleanup_disposal_authorization() {
    let current = self.authorize_terminal_cleanup_lease(&effect.command.session_id, &effect.lease)?;
    let authorization = effect.sandbox_disposal_authorization_for_current_generation(&current);
    authorization.effect_fence().validate_live_at(now);
}
"""
    canonical[RUNTIME_HOST_TERMINAL_CLEANUP] = """
fn prepare_terminal_cleanup_effect() {
    authorization.verify_for(&effect);
    self.terminal_cleanup_effect_fence(&effect)?;
    TerminalEnvironmentPreparation::artifact_capture_mode;
    self.artifact_harvester().harvest_with_fence_mode(
        thread,
        Some(ArtifactPublicationFence::Terminal(effect)),
        capture_mode,
    );
    SessionCleanupPreparation::try_new(&effect, provider_prepared_effect_fence, artifact_receipts);
}
fn dispose_terminal_cleanup_effect() {
    self.terminal_cleanup_disposal_authorization(&effect)?;
    let authorization = self.terminal_cleanup_disposal_authorization(&effect)?;
    prepare(authorization.effect_fence());
    environment.dispose_for_effect(&authorization);
    self.terminal_cleanup_disposal_authorization(&effect)?;
    SessionCleanupDisposalReceipt::new(&effect.command);
}
"""
    canonical[GIT_REPOSITORY_PUBLICATION] = """
fn admit_repository_publication() {}
fn classify_repository_publication_reobservation() {}
fn push_repo_to_at() {
    match admit_repository_publication(expectation, observed.as_deref());
    let exact_ref_lease = format!("--force-with-lease={remote_ref}:{expected_remote}");
    match classify_repository_publication_reobservation(expectation, before, after);
    RepositoryPublicationReobservation::Unchanged;
    matches!(byte, b'0'..=b'9' | b'a'..=b'f');
}
"""
    canonical[ARTIFACT_HARVEST] = """
fn harvest_with_fence_mode_from_environment() {
    if capture_mode == ArtifactCaptureMode::ReceiptOnly && terminal_effect.is_none() {
        validate_terminal_fence();
    }
    if capture_mode == ArtifactCaptureMode::ReceiptOnly {
        return self.recover_terminal_receipts(thread, workspace, effect);
    }
    let projected_environment = exact_environment.cloned().or_else(|| self.session_slots.read(thread));
    env.capture_artifacts();
    harvest_idempotency_key(thread, path, content);
    publication.verify();
    receipt.verify(&publication);
}
fn recover_terminal_receipts() {
    self.publisher.recover(recovery);
}
"""
    # Extracted-owner cause/effect decision table. C1=the repository failure
    # policy remains in recovery.rs; C2=the durable root-CAS port remains in
    # repository_port.rs; C3=the closed cleanup authorization remains in its
    # private realization child; C4=the Host's typed disposal authority remains
    # in terminal_preparation.rs; C5=the Host's two-stage orchestration remains
    # in terminal_cleanup.rs. R1 C1+C2+C3+C4+C5 -> accept the one canonical
    # owner graph; R2 any missing source -> reject instead of accepting stale
    # parent markers or a duplicate compatibility definition.
    assert session_effect_violations(canonical) == [], "canonical effect paths"
    assert repository_publication_writer_violations(canonical) == [], (
        "canonical Repository publication writer"
    )

    # G48 cause/effect rules: C1 one expected-prior declaration reaches the
    # sole local Git writer under an exact ref lease; C2 the shared cleanup
    # driver records its typed outcome through the one Session-root CAS before
    # projecting root preparation/disposal; C3 the Host deadline decorator
    # forwards both outcomes without becoming another outcome owner. E1 accepts
    # C1+C2+C3. Removing an edge, reordering a durable outcome, or adding a
    # second writer produces E2.
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
        ("store encode aggregate binding", SESSION_ROW_CODEC, "verify_aggregate(session)?;"),
        ("store decode aggregate binding", SESSION_ROW_CODEC, "verify_aggregate(&aggregate)?;"),
        ("local completed binding", APPLICATION_CLEANUP, ".verified_terminal_cleanup()"),
        (
            "root outcome completed binding",
            APPLICATION_RESOURCE_RECONCILIATION,
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
            "shared root outcome CAS",
            APPLICATION_RESOURCE_RECONCILIATION,
            '"terminal-repository-publication-worker-outcome"',
        ),
        (
            "extracted rejection forwarding",
            APPLICATION_PUBLICATION_CONTROL,
            ".record_terminal_repository_publication_rejection_from_root(",
        ),
        (
            "shared driver rejection settlement",
            SESSION_CLEANUP_DRIVER,
            ".record_terminal_repository_publication_rejection(",
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
            "hosted receipt forwarding",
            RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE,
            ".record_terminal_repository_publication_receipt(",
        ),
        (
            "hosted rejection forwarding",
            RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE,
            ".record_terminal_repository_publication_rejection(",
        ),
        (
            "typed hosted rejection effect",
            RUNTIME_REPOSITORY_PUBLICATION,
            "SessionRepositoryPublicationEffect::Rejected(",
        ),
    ):
        mutant = dict(canonical)
        mutant[owner] = mutant[owner].replace(marker, "", 1)
        assert session_effect_violations(mutant), f"removed {rule} rejected"

    # G48 authority/deadline removal rules: C1 each destructive Worker route
    # verifies the authenticated cleanup generation in its own function before
    # reaching Control; C2 each Host forwarding method enters the one deadline
    # decorator and returns the awaited Control result unchanged as its tail
    # expression. E1 accepts C1+C2. E2 rejects a route that relies on a sibling's
    # marker, bypasses its deadline, or swallows the Control outcome, even though
    # the same file still contains every broad marker.
    for rule, function in (
        ("Worker publication poll authority", "session_repository_publication_poll"),
        ("Worker publication receipt authority", "session_repository_publication_complete"),
        ("Worker publication rejection authority", "session_repository_publication_reject"),
    ):
        mutant = dict(canonical)
        guarded = (
            f"async fn {function}() {{\n"
            "    verify_terminal_cleanup_authority(...).await;\n"
        )
        assert guarded in mutant[WORKER_PUBLICATION_ROUTE], f"canonical {rule} fixture"
        mutant[WORKER_PUBLICATION_ROUTE] = mutant[WORKER_PUBLICATION_ROUTE].replace(
            guarded,
            f"async fn {function}() {{\n",
            1,
        )
        assert session_effect_violations(mutant), f"removed {rule} rejected"

    for rule, function, operation, forwarding in (
        (
            "hosted receipt deadline",
            "record_terminal_repository_publication_receipt",
            "record_terminal_repository_publication",
            "record_terminal_repository_publication_receipt",
        ),
        (
            "hosted rejection deadline",
            "record_terminal_repository_publication_rejection",
            "record_terminal_repository_publication_rejection",
            "record_terminal_repository_publication_rejection",
        ),
    ):
        mutant = dict(canonical)
        bounded = (
            f"    self.call(\n"
            f'        "{operation}",\n'
            f"        self.inner.{forwarding}(...),\n"
            "    )\n"
            "    .await"
        )
        direct = f"    self.inner.{forwarding}(...).await"
        assert bounded in mutant[RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE], (
            f"canonical {function} deadline fixture"
        )
        mutant[RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE] = mutant[
            RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE
        ].replace(bounded, direct, 1)
        assert session_effect_violations(mutant), f"removed {rule} rejected"

        swallowed = dict(canonical)
        swallowed_call = (
            bounded.replace("    self.call(", "    let _ = self.call(", 1)
            + ";\n    Ok(())"
        )
        swallowed[RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE] = swallowed[
            RUNTIME_HOST_REALIZATION_CONTROL_DEADLINE
        ].replace(bounded, swallowed_call, 1)
        assert session_effect_violations(swallowed), (
            f"swallowed {rule} Control outcome rejected"
        )

    for rule, owner, earlier, later in (
        (
            "root rejection before outcome CAS",
            APPLICATION_RESOURCE_RECONCILIATION,
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

    for rule, relative in (
        ("repository recovery policy", SESSION_REPOSITORY_RECOVERY),
        ("repository root-CAS port", SESSION_REPOSITORY_PORT),
        ("terminal preparation authorization", SESSION_TERMINAL_AUTHORIZATION),
        ("Host terminal provider authorization", RUNTIME_HOST_TERMINAL_PREPARATION),
        ("Host terminal two-stage effects", RUNTIME_HOST_TERMINAL_CLEANUP),
    ):
        missing_owner = dict(canonical)
        missing_owner[relative] = ""
        assert session_effect_violations(missing_owner), f"missing {rule} rejected"

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
    for rule, relative, marker in (
        ("durable fence", APPLICATION_CLEANUP, '"terminal-cleanup-fence"'),
        (
            "quiescence",
            APPLICATION_CLEANUP,
            ".quiesce_terminal_delegations(session_id)",
        ),
        (
            "watermarked freeze",
            APPLICATION_CLEANUP,
            ".freeze_terminal_cleanup_targets(",
        ),
        (
            "preparation settlement",
            SESSION_CLEANUP_DRIVER,
            ".record_terminal_cleanup_preparation(&lease, preparation)",
        ),
        (
            "disposal settlement",
            SESSION_CLEANUP_DRIVER,
            ".record_terminal_cleanup_disposal(&lease, receipt)",
        ),
    ):
        mutant = dict(canonical)
        mutant[relative] = mutant[relative].replace(marker, "")
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

    # Terminal two-stage cause/effect decision table:
    # C1=the shared driver uses the closed preparation authorization and records
    # preparation before any physical disposal; C2=the aggregate-derived
    # SandboxDisposalAuthorization proves the asserted/current leases are the
    # same generation and projects its successor fence from the current lease;
    # C3=Host reconstructs/observes the physical owner under current live
    # successor B, while the complete typed A/fingerprint/B authorization
    # reaches the provider before the disposal receipt is recorded; C4=terminal
    # Artifact recovery selects ReceiptOnly through the one batch owner.
    # Effects: E1 accept only C1+C2+C3+C4; E2
    # reject the removed one-stage execute/completion path; E3 reject a foreign
    # generation or predecessor-expiry fence; C5=the closed authorization stays
    # in its one extracted owner; C6=local cleanup crosses the renewal supervisor
    # before the shared driver. E4 reject terminal output recovery routed through
    # the live-only harvest wrapper; E5 reject a stale parent-file inventory or a
    # local caller that bypasses its generation supervisor.
    #
    # | Rule | C1 | C2 | C3 | C4 | Effect |
    # | T1   | T  | T  | T  | T  | E1 accept canonical two-stage path |
    # | T2   | F  | *  | *  | *  | E2 reject one-stage cleanup |
    # | T3   | T  | F  | *  | *  | E3 reject foreign/predecessor fence |
    # | T4   | T  | T  | F  | *  | E2 reject disposal without typed authority |
    # | T5   | T  | T  | T  | F  | E4 reject live-harvest fallback |
    # | T6   | T  | T  | T  | T, !C5 | E5 reject missing extracted owner |
    # | T7   | T  | T  | T  | T, !C6 | E5 reject local renewal bypass |
    legacy_one_stage = dict(canonical)
    legacy_one_stage[APPLICATION_CLEANUP] += (
        "\n.execute_terminal_cleanup_for_effect(effect)"
        "\n.complete_terminal_cleanup(completion)"
    )
    assert session_effect_violations(legacy_one_stage), "T2 one-stage cleanup rejected"

    wrong_preparation_order = dict(canonical)
    wrong_preparation_order[SESSION_CLEANUP_DRIVER] = wrong_preparation_order[
        SESSION_CLEANUP_DRIVER
    ].replace(
        "runtime.prepare_terminal_cleanup_for_effect(effect.clone(), authorization);\n"
        "            control.record_terminal_cleanup_preparation(&lease, preparation);",
        "control.record_terminal_cleanup_preparation(&lease, preparation);\n"
        "            runtime.prepare_terminal_cleanup_for_effect(effect.clone(), authorization);",
    )
    assert session_effect_violations(wrong_preparation_order), (
        "T2 marker presence cannot hide prepare/record inversion"
    )

    untyped_disposal = dict(canonical)
    untyped_disposal[RUNTIME_HOST_TERMINAL_PREPARATION] = untyped_disposal[
        RUNTIME_HOST_TERMINAL_PREPARATION
    ].replace(
        "effect.sandbox_disposal_authorization_for_current_generation(&current)",
        "provider.authorize_delete()",
    )
    assert session_effect_violations(untyped_disposal), "T4 typed disposal authority required"

    foreign_generation = dict(canonical)
    foreign_generation[SESSION_CLEANUP_EFFECTS] = foreign_generation[
        SESSION_CLEANUP_EFFECTS
    ].replace(
        "realization_lease_generation_authorizes(current_lease, &self.lease)",
        "true",
    )
    assert session_effect_violations(foreign_generation), "T3 exact generation required"

    predecessor_fence = dict(canonical)
    predecessor_fence[SESSION_CLEANUP_EFFECTS] = predecessor_fence[
        SESSION_CLEANUP_EFFECTS
    ].replace(
        "current_lease.sandbox_effect_fence(self.command.effect_id.clone())?",
        "self.lease.sandbox_effect_fence(self.command.effect_id.clone())?",
    )
    assert session_effect_violations(predecessor_fence), "T3 current renewal fence required"

    stale_host_fence = dict(canonical)
    stale_host_fence[RUNTIME_HOST_TERMINAL_CLEANUP] = stale_host_fence[
        RUNTIME_HOST_TERMINAL_CLEANUP
    ].replace(
        "prepare(authorization.effect_fence());",
        "prepare(authorization.prepared_effect_fence());",
    )
    assert session_effect_violations(stale_host_fence), (
        "T3 Host physical reconstruction requires current successor B, not predecessor A"
    )

    live_harvest = dict(canonical)
    live_harvest[RUNTIME_HOST_TERMINAL_CLEANUP] = live_harvest[
        RUNTIME_HOST_TERMINAL_CLEANUP
    ].replace(".harvest_with_fence_mode(", ".harvest_with_fence(")
    assert session_effect_violations(live_harvest), "T5 live harvest fallback rejected"

    receipt_after_live = dict(canonical)
    receipt_after_live[ARTIFACT_HARVEST] = receipt_after_live[ARTIFACT_HARVEST].replace(
        "return self.recover_terminal_receipts(thread, workspace, effect);\n"
        "    }\n"
        "    let projected_environment = exact_environment.cloned().or_else(|| self.session_slots.read(thread));",
        "let projected_environment = exact_environment.cloned().or_else(|| self.session_slots.read(thread));\n"
        "    }\n"
        "    return self.recover_terminal_receipts(thread, workspace, effect);",
    )
    assert session_effect_violations(receipt_after_live), (
        "T5 marker presence cannot put ReceiptOnly recovery after live capture"
    )

    local_renewal_bypass = dict(canonical)
    local_renewal_bypass[APPLICATION_CLEANUP] = local_renewal_bypass[
        APPLICATION_CLEANUP
    ].replace(
        "self.drive_local_terminal_cleanup(&assignment);",
        "drive_session_terminal_cleanup(session_id, lease, self, self.runtime());",
    )
    assert session_effect_violations(local_renewal_bypass), (
        "T7 local cleanup cannot bypass its generation supervisor"
    )

    open_preparation_authorization = dict(canonical)
    open_preparation_authorization[SESSION_TERMINAL_AUTHORIZATION] = (
        open_preparation_authorization[SESSION_TERMINAL_AUTHORIZATION].replace(
            "    effect: crate::", "    pub effect: crate::", 1
        )
    )
    assert session_effect_violations(open_preparation_authorization), (
        "T2 preparation authorization fields remain closed"
    )
