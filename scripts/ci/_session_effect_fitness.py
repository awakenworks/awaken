"""Enforce domain-owned intent/receipt paths for Session external effects."""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace


SESSION_RUNTIME = "crates/contract/awaken-session-execution-contract/src/session.rs"
SESSION_RUNTIME_ROLES = "crates/contract/awaken-session-execution-contract/src/session/roles.rs"
SESSION_CLEANUP = "crates/contract/awaken-session-state-contract/src/terminal_cleanup.rs"
SESSION_REPOSITORY_PUBLICATION = (
    "crates/contract/awaken-session-state-contract/src/"
    "terminal_cleanup/repository_publication.rs"
)
SESSION_CLEANUP_DRIVER = (
    "crates/server/awaken-session-realization-driver/src/terminal_cleanup_driver.rs"
)
SESSION_CLEANUP_RECOVERY = (
    "crates/server/awaken-session-realization-driver/src/terminal_cleanup_recovery.rs"
)
SESSION_CLEANUP_EFFECTS = (
    "crates/contract/awaken-session-state-contract/src/terminal_cleanup/effects.rs"
)
SESSION_CLEANUP_PROGRESS = (
    "crates/contract/awaken-session-state-contract/src/terminal_cleanup/progress.rs"
)
SESSION_REPOSITORY_RECOVERY = (
    "crates/contract/awaken-session-state-contract/src/session_repo/recovery.rs"
)
SESSION_REPOSITORY_PORT = (
    "crates/contract/awaken-session-repository-contract/src/repository_port.rs"
)
SESSION_REPOSITORY_MUTATION = (
    "crates/contract/awaken-session-state-contract/src/session_repo/mutation.rs"
)
SESSION_REPOSITORY_PUBLICATION_AGGREGATE = (
    "crates/contract/awaken-session-state-contract/src/session_repo/repository_publication.rs"
)
SESSION_DELETE_DISPOSITION = (
    "crates/contract/awaken-session-state-contract/src/session_repo/disposition.rs"
)
SESSION_ROW_CODEC = "crates/stores/awaken-session-store/src/row_codec.rs"
SESSION_REALIZATION = "crates/contract/awaken-session-realization-contract/src/session_realization.rs"
SESSION_REALIZATION_DRIVER = (
    "crates/server/awaken-session-realization-driver/src/session_realization_driver.rs"
)
SESSION_TERMINAL_AUTHORIZATION = (
    "crates/contract/awaken-session-realization-contract/src/"
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
RUNTIME_HOST = "crates/server/awaken-session-executor/src/run_assembly/roles.rs"
REALIZATION_EFFECTS_ROLE = (
    "crates/server/awaken-session-realization/src/realization_executor/effects_role.rs"
)
RUNTIME_HOST_APPLICATION = "crates/server/awaken-session-executor/src/application.rs"
SESSION_REALIZATION_CONTROL_DEADLINE = (
    "crates/server/awaken-session-realization-driver/src/"
    "control_deadline.rs"
)
RUNTIME_REPOSITORY_PUBLICATION = (
    "crates/server/awaken-session-realization/src/terminal_effects/repository_publication.rs"
)
RUNTIME_HOST_TERMINAL = (
    "crates/server/awaken-session-realization/src/environment_holding/lifecycle.rs"
)
RUNTIME_BOUND_ENVIRONMENT = "crates/server/awaken-session-realization/src/environment_holding/bound_environment.rs"
RUNTIME_HOST_TERMINAL_PREPARATION = (
    "crates/server/awaken-session-realization/src/"
    "terminal_effects/preparation.rs"
)
RUNTIME_HOST_TERMINAL_CLEANUP = (
    "crates/server/awaken-session-realization/src/"
    "terminal_effects/cleanup.rs"
)
GIT_REPOSITORY_PUBLICATION = (
    "crates/worker/awaken-sandbox-local/src/git_transport.rs"
)
WORKER_PUBLICATION_ROUTE = (
    "crates/server/awaken-dispatch-http/src/"
    "worker_dispatch/terminal_repository_publication.rs"
)
WORKER_CONTROL_CLIENT = (
    "crates/server/awaken-coordinator-client/src/worker_control_client.rs"
)
WORKER_RUNTIME = "crates/bin/awaken-worker/src/lib.rs"
ARTIFACT_HARVEST = "crates/server/awaken-session-realization/src/artifact_harvest.rs"
ARTIFACT_TRANSPORT = (
    "crates/resources/awaken-resource-worker-http/src/artifact_publication_http.rs"
)


REQUIRED = {
    SESSION_RUNTIME: (
        "SessionEnvironmentReceipt",
        "publish_mcp_generation_receipt",
        "drain_mcp_generation_receipt",
        "DelegatedRunSnapshot",
    ),
    SESSION_RUNTIME_ROLES: (
        "prepare_terminal_cleanup_for_effect",
        "acknowledge_terminal_cleanup_preparation",
        "dispose_terminal_cleanup_for_effect",
        "acknowledge_terminal_cleanup_disposal",
        "acknowledge_completed_terminal_cleanup",
        "install_terminal_cleanup_assignment",
        "quiesce_terminal_delegations",
        "execute_terminal_repository_publication_for_lease",
        "SessionRepositoryPublicationEffect",
    ),
    SESSION_CLEANUP: (
        "SessionCleanupCommand",
        "SessionCleanupOperation",
        "SessionCleanupPreparation",
        "SessionCleanupDisposalReceipt",
        "SessionTerminalCleanupAction",
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
        "pub(crate) fn verify_for(&self, session_id: &SessionId)",
        "pub fn repository_publication_receipt(",
        "receipt.verify(&command)?",
        "pub fn repository_publication_rejection(",
        "rejection.verify(&command)?",
        "pub(crate) fn record_repository_publication_rejection(",
        "RepositoryPublicationOutcomeMismatch",
    ),
    SESSION_REPOSITORY_RECOVERY: (
        "SessionRepositoryRecoveryAction",
        "Self::AuthorityUnavailable(_)",
        "Self::Corrupt(_)",
    ),
    SESSION_REPOSITORY_MUTATION: (
        "Unavailable(String)",
        "Corrupt(String)",
        "validate_observation_replacement",
    ),
    SESSION_REPOSITORY_PORT: (
        "async fn get(&self, session_id: &SessionId)\n    -> Result<PersistedSession",
        "async fn owner(&self, session_id: &SessionId) -> Result<String",
    ),
    SESSION_REPOSITORY_PUBLICATION_AGGREGATE: (
        "pub fn verified_terminal_cleanup(",
        "self.terminal_cleanup.verify_for(&self.session_id)?",
        "pub fn record_repository_publication_rejection(",
        "matches_source_binding(self.environment.terminal_repository_publication_binding())",
    ),
    SESSION_DELETE_DISPOSITION: (
        'event_type: "session.deleted"',
        "SessionDeleteLifecycleFactPlan",
    ),
    SESSION_ROW_CODEC: ("verify_aggregate(session)?;", "verify_aggregate(&aggregate)?;"),
    SESSION_REALIZATION_DRIVER: (
        "publish_mcp_generation_receipt",
        "drain_mcp_generation_receipt",
        "McpProjectionEffectKind::Publish",
        "McpProjectionEffectKind::Drain",
    ),
    SESSION_REALIZATION: (
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
        ".authorize_terminal_cleanup_effect(effect)",
        "SessionTerminalCleanupPreparationAuthorization::try_new(",
    ),
    APPLICATION_PUBLICATION_CONTROL: (
        "record_terminal_repository_publication_rejection_after_refresh",
        ".record_terminal_repository_publication_rejection_from_root(",
        "async fn record_terminal_repository_publication_rejection(",
    ),
    APPLICATION_EVENT_BATCHES: (".verified_terminal_cleanup()",),
    APPLICATION_RESOURCE_RECONCILIATION: (
        ".terminal_cleanup_work_action()",
        ".authorize_terminal_cleanup_disposal_effect(&owner_scope, effect)",
        ".record_terminal_cleanup_preparation(",
        '"terminal-cleanup-preparation"',
        ".record_terminal_cleanup_disposal(",
        '"terminal-cleanup-disposal"',
        ".verified_terminal_cleanup()",
        "record_terminal_repository_publication_effect_from_root",
        ".record_repository_publication_rejection(rejection)",
        '"terminal-repository-publication-worker-outcome"',
    ),
    APPLICATION_TERMINAL: (
        "SessionDeleteCommand",
        "commit_delete_intent",
        "pub async fn delete_session(",
        ".wake_lifecycle_supervisor()",
        ".repository_publication_rejection(&command.session_id)",
        ".repository_publication_receipt(&command.session_id)",
    ),
    APPLICATION_WORK_DISPATCH: ("if session.is_terminal()",),
    PROTOCOL_SESSIONS: (
        "SessionDeleteCommand::new(id)",
        ".delete_session(awaken_session_application::SessionDeleteCommand::new(id))",
    ),
    SESSION_STORE: (
        "SessionRepositoryError::AuthorityUnavailable",
        "SessionRepositoryError::Corrupt",
        "fn validate_current_mutation(",
        "current.admits_tombstone(",
        ".validate_observation_replacement(replacement)",
    ),
    SQLITE_SESSION_STORE: ("validate_current_mutation(",),
    POSTGRES_SESSION_STORE: ("validate_current_mutation(",),
    RUNTIME_HOST: (
        "async fn prepare_terminal_cleanup_for_effect",
        "async fn acknowledge_terminal_cleanup_preparation",
        "async fn acknowledge_terminal_cleanup_disposal",
        "async fn acknowledge_completed_terminal_cleanup",
    ),
    REALIZATION_EFFECTS_ROLE: ("async fn dispose_terminal_cleanup_for_effect",),
    SESSION_CLEANUP_RECOVERY: ("drive_session_terminal_cleanup(",),
    SESSION_REALIZATION_CONTROL_DEADLINE: (
        "async fn renew_session_realization(",
        ".renew_session_realization(",
    ),
    RUNTIME_REPOSITORY_PUBLICATION: (
        "RepositoryPublicationActivationError::Rejected(",
        "SessionRepositoryPublicationEffect::Rejected(",
    ),
    RUNTIME_HOST_TERMINAL: (),
    RUNTIME_BOUND_ENVIRONMENT: (
        "fn install_bound_environment_under_lifecycle(",
        "fn prepare_bound_environment_for_effect_under_lifecycle(",
    ),
    RUNTIME_HOST_TERMINAL_PREPARATION: (
        ".sandbox_disposal_authorization_for_current_generation(&current)",
    ),
    RUNTIME_HOST_TERMINAL_CLEANUP: (
        "authorization.verify_for(&effect)",
        ".harvest_with_fence_mode(",
        "ResourceOperationFence::Terminal",
        "SessionCleanupPreparation::try_new",
        ".dispose_for_effect(&authorization, &authority)",
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


def _rust_function(source: str, name: str) -> str | None:
    """Return the first Rust function item with a body, including that body."""
    source = _crate_boundary_workspace.production_rust(source)
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
    # rustfmt breaks a long method chain before each `.`; markers name it whole.
    body = re.sub(r"\s*\n\s*\.", ".", body)
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
            SESSION_REPOSITORY_PUBLICATION_AGGREGATE,
            "record_repository_publication_rejection",
            (
                "matches_source_binding(self.environment.terminal_repository_publication_binding())",
                ".record_repository_publication_rejection(&self.session_id, rejection)",
            ),
            "source-bound aggregate rejection before sidecar mutation",
        )
    )
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

    realization = _crate_boundary_workspace.production_rust(
        sources.get(SESSION_TERMINAL_AUTHORIZATION, "")
    )
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

    errors.extend(_require_ordered_function(
        sources, APPLICATION_RESOURCE_RECONCILIATION,
        "require_current_live_terminal_generation",
        ("current.expires_at_unix_ms", "self.clock.now_ms()"),
        "terminal generation consumes the installed application clock",
    ))

    if _rust_function(sources.get(APPLICATION_RESOURCE_RECONCILIATION, ""),
                      "authorize_terminal_cleanup_effect_from_root") is not None:
        errors.append(f"{APPLICATION_CLEANUP}: terminal preparation authorization has a duplicate or retired owner")

    # Application Control re-derives authority from the root and commits both
    # receipt classes through that same root CAS; it owns no side queue.
    for owner, function, markers, ownership in (
        (
            APPLICATION_CLEANUP,
            "authorize_terminal_cleanup_effect_from_root",
            (
                ".get(&effect.command.session_id)",
                "self.require_current_live_terminal_generation(&session)?",
                ".owner(&effect.command.session_id)",
                "terminal_restore_target_for_thread(",
                ".authorize_terminal_cleanup_effect(effect)",
                ".source_dispatch_observation(&session)",
                ".terminal_source_dispatch()?",
                "SessionTerminalCleanupPreparationAuthorization::try_new(",
                ".with_source_dispatch(source_dispatch)",
            ),
            "root-derived terminal preparation authorization",
        ),
        (
            APPLICATION_RESOURCE_RECONCILIATION,
            "authorize_terminal_cleanup_disposal_from_root",
            (
                ".get(&effect.command.session_id)",
                ".owner(&effect.command.session_id)",
                ".authorize_terminal_cleanup_disposal_effect(&owner_scope, effect)",
            ),
            "root-derived terminal disposal authorization",
        ),
        (
            APPLICATION_RESOURCE_RECONCILIATION,
            "record_terminal_cleanup_preparation_from_root",
            (
                ".session_aggregate(&session_id)",
                ".record_terminal_cleanup_preparation(",
                '"terminal-cleanup-preparation"',
            ),
            "root-CAS terminal preparation receipt",
        ),
        (
            APPLICATION_RESOURCE_RECONCILIATION,
            "record_terminal_cleanup_disposal_from_root",
            (
                ".session_aggregate(&session_id)",
                ".record_terminal_cleanup_disposal(",
                '"terminal-cleanup-disposal"',
            ),
            "root-CAS terminal disposal receipt",
        ),
    ):
        errors.extend(
            _require_ordered_function(
                sources,
                owner,
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
                "session.lifecycle().record_repository_publication_rejection(rejection)",
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
                    "terminal_cleanup_control(&service)",
                    effect,
                ),
                ownership,
            )
        )
    errors.extend(
        _require_ordered_function(
            sources,
            SESSION_REALIZATION_CONTROL_DEADLINE,
            "renew_session_realization",
            (
                "self.call(",
                '"renew"',
                ".renew_session_realization(",
                ".await",
            ),
            "deadline-bounded hosted Session lease renewal",
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
                "self.effect_fence(&effect)?",
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
            "disposal_authorization",
            (
                "self.authorize_lease(&effect.command.session_id, &effect.lease)?",
                ".sandbox_disposal_authorization_for_current_generation(&current)",
            ),
            "typed Sandbox disposal authorization",
        )
    )
    errors.extend(
        _require_ordered_function(
            sources, RUNTIME_BOUND_ENVIRONMENT,
            "install_bound_environment_under_lifecycle",
            ("peers.authority.provider_effect_authority(",
             ".prepare_terminal_effective_for_effect(", "&authority,"),
            "cold terminal reconstruction authority",
        )
    )
    # A later sibling adoption/rebind also borrows &authority. It must not
    # satisfy the terminal port's own argument obligation after a cold await.
    reconstruction = _rust_function(
        sources.get(RUNTIME_BOUND_ENVIRONMENT, ""),
        "install_bound_environment_under_lifecycle",
    )
    if reconstruction is not None and not re.search(
        r"\.prepare_terminal_effective_for_effect\(\s*spec,\s*handle,"
        r"\s*expected_effect_fence,\s*effect_fence,\s*&authority,?\s*\)",
        re.sub(r"\s*\n\s*\.", ".", reconstruction),
    ):
        errors.append(
            f"{RUNTIME_BOUND_ENVIRONMENT}: cold terminal reconstruction authority "
            "must reach the terminal provider call with both exact fences"
        )
    errors.extend(_require_ordered_function(
        sources, RUNTIME_BOUND_ENVIRONMENT,
        "prepare_bound_environment_for_effect_under_lifecycle",
        (".observe_effective(", ".await", ".install_bound_environment_under_lifecycle(",
         ".await?", "Ok(PreparedBoundEnvironment::new("),
        "bound Environment reconstruction handoff",
    ))
    errors.extend(
        _require_ordered_function(
            sources, RUNTIME_HOST_TERMINAL_PREPARATION,
            "cleanup_terminal_checkpoint",
            ("self.effect_fence(effect)?",
             "peers.authority.provider_effect_authority(",
             "with_current_sandbox_effect(", "match (checkpoint, pending_checkpoint)",
             ".cleanup_checkpoint_for_terminal(", "&authority,",
             "with_current_sandbox_effect("),
            "terminal checkpoint current authority",
        )
    )
    disposal_projection = _rust_function(
        sources.get(RUNTIME_HOST_TERMINAL_PREPARATION, ""),
        "disposal_authorization",
    )
    if disposal_projection is not None and any(
        marker in disposal_projection
        for marker in ("validate_live_at", "wall_clock_now_ms", "SystemTime::")
    ):
        errors.append(
            f"{RUNTIME_HOST_TERMINAL_PREPARATION}: terminal disposal projection "
            "must not compare Control expiry with Worker wall time"
        )
    errors.extend(
        _require_ordered_function(
            sources,
            RUNTIME_HOST_TERMINAL_CLEANUP,
            "dispose_terminal_cleanup_effect",
            (
                "let environments = peers.environments;",
                "root_realization.lock().await",
                "self.disposal_authorization(&effect)?",
                "sessions.lock_lifecycle(session_id.as_str()).await",
                "let authorization = self.disposal_authorization(&effect)?",
                "self.prepare_terminal_environment_under_lifecycle(",
                "authorization.effect_fence()",
                "BoundEnvironmentPreparationMode::TerminalDisposal",
                "(SessionEnvironmentOwner::Retiring(retirement), None)",
                "let exact_authorization = self.disposal_authorization(&effect)?",
                "if exact_authorization != authorization",
                "peers.authority.provider_effect_authority(",
                ".dispose_for_effect(&authorization, &authority)",
                "sessions.end_terminal_disposal(",
                "self.disposal_authorization(&effect)?",
                "SessionCleanupDisposalReceipt::new(",
            ),
            "physical terminal disposal",
        )
    )
    errors.extend(_require_ordered_function(
        sources, RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
        ("peers.authority.provider_effect_authority(",
         ".dispose_restored(&spec, expected, &authorization, &authority)",
         "with_current_sandbox_effect("),
        "restored target current authority",
    ))
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
                "let projected_environment = match exact_environment",
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
            SESSION_CLEANUP_RECOVERY,
            "reconcile_terminal_cleanup",
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
        production = _crate_boundary_workspace.production_rust(
            sources.get(relative, "")
        )
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



SANDBOX_PORTS = "crates/contract/awaken-provisioning-contract/src/sandbox/runtime.rs"


def sandbox_port_obligation_violations(sources: dict[str, str]) -> list[str]:
    """The Sandbox ports must keep the fenced edges mandatory and the unfenced
    ones absent.

    The compiler already enforces this for anyone implementing or calling the
    traits; this guard exists for the one thing the compiler cannot see — a
    future edit that re-adds an unfenced port, or gives a fenced obligation a
    default body so a provider can silently inherit it again. That inversion is
    exactly how an unfenced `dispose` survived five separate 'revalidate current
    authority' fixes in the Session environment lifecycle.
    """
    errors: list[str] = []
    ports = sources.get(SANDBOX_PORTS, "")
    if not ports:
        return [f"{SANDBOX_PORTS}: sandbox port definitions are missing"]

    for obligation in (
        # Creation is no longer an `async fn`: it hands back a `ClaimedEffect`
        # minted synchronously, so a provider cannot hand out a future whose
        # cancellation would strand the resource it creates. Pinning the
        # claimed spelling here keeps a future edit from quietly restoring the
        # bare async port along with that hazard.
        "fn claim_for_effect",
        "async fn adopt_for_effect(",
        "async fn dispose_for_effect(",
        "async fn dispose_unattached(",
    ):
        start = ports.find(obligation)
        if start < 0:
            errors.append(f"{SANDBOX_PORTS}: missing required obligation {obligation!r}")
            continue
        terminator = ports.find(";", start)
        body = ports.find("{", start)
        if body != -1 and (terminator == -1 or body < terminator):
            errors.append(
                f"{SANDBOX_PORTS}: {obligation!r} has a default body; a provider that cannot "
                "honour it must fail to compile, not inherit one"
            )

    for unfenced in (
        "async fn create(&self",
        "async fn adopt(&self",
        "async fn dispose(&self",
    ):
        if unfenced in ports:
            errors.append(
                f"{SANDBOX_PORTS}: unfenced port {unfenced!r} is back on the trait; a physical "
                "effect reachable through the port has no current-authority admission"
            )
    return errors


def check_all(repo_root: Path) -> list[str]:
    sources = {
        relative: (repo_root / relative).read_text(encoding="utf-8")
        for relative in REQUIRED
    }
    errors = session_effect_violations(sources)
    errors.extend(
        sandbox_port_obligation_violations(
            {SANDBOX_PORTS: (repo_root / SANDBOX_PORTS).read_text(encoding="utf-8")}
        )
    )
    rust_sources = {
        path.relative_to(repo_root).as_posix(): path.read_text(encoding="utf-8")
        for path in (repo_root / "crates").rglob("*.rs")
    }
    errors.extend(repository_publication_writer_violations(rust_sources))
    return errors




def _migrated_effect_owner_mutation_selftest(sources: dict[str, str]) -> None:
    """Removing any new owner's authority, recheck, or receipt must fail its own rule.

    Caller fixtures and current production sources use this same mutation table;
    sibling calls, comments and legacy Host facades cannot mask a missing edge.
    """
    for relative, function, marker, ownership, last, after in (
        (APPLICATION_CLEANUP, "authorize_terminal_cleanup_effect_from_root",
         ".get(&effect.command.session_id)", "root-derived terminal preparation authorization", False, ""),
        (APPLICATION_CLEANUP, "authorize_terminal_cleanup_effect_from_root",
         "self.require_current_live_terminal_generation(&session)?", "root-derived terminal preparation authorization", False, ""),
        (APPLICATION_CLEANUP, "authorize_terminal_cleanup_effect_from_root",
         ".authorize_terminal_cleanup_effect(effect)", "root-derived terminal preparation authorization", False, ""),
        (APPLICATION_CLEANUP, "authorize_terminal_cleanup_effect_from_root",
         ".source_dispatch_observation(&session)", "root-derived terminal preparation authorization", False, ""),
        (APPLICATION_CLEANUP, "authorize_terminal_cleanup_effect_from_root",
         ".terminal_source_dispatch()?", "root-derived terminal preparation authorization", False, ""),
        (APPLICATION_CLEANUP, "authorize_terminal_cleanup_effect_from_root",
         "SessionTerminalCleanupPreparationAuthorization::try_new(", "root-derived terminal preparation authorization", False, ""),
        (APPLICATION_CLEANUP, "authorize_terminal_cleanup_effect_from_root",
         ".with_source_dispatch(source_dispatch)", "root-derived terminal preparation authorization", False, ""),
        (RUNTIME_BOUND_ENVIRONMENT, "install_bound_environment_under_lifecycle",
         "peers.authority.provider_effect_authority(", "cold terminal reconstruction authority", False, ""),
        (RUNTIME_BOUND_ENVIRONMENT, "install_bound_environment_under_lifecycle",
         "&authority,", "cold terminal reconstruction authority", False, ".prepare_terminal_effective_for_effect("),
        (RUNTIME_BOUND_ENVIRONMENT, "install_bound_environment_under_lifecycle",
         "expected_effect_fence,", "cold terminal reconstruction authority", False, ".prepare_terminal_effective_for_effect("),
        (RUNTIME_BOUND_ENVIRONMENT, "prepare_bound_environment_for_effect_under_lifecycle",
         ".install_bound_environment_under_lifecycle(", "bound Environment reconstruction handoff", False, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         "let environments = peers.environments;", "physical terminal disposal", False, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         "let authorization = self.disposal_authorization(&effect)?", "physical terminal disposal", False, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         "let exact_authorization = self.disposal_authorization(&effect)?", "physical terminal disposal", False, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         "peers.authority.provider_effect_authority(", "physical terminal disposal", True, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         ".dispose_for_effect(&authorization, &authority)", "physical terminal disposal", False, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         "self.disposal_authorization(&effect)?", "physical terminal disposal", True, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         "SessionCleanupDisposalReceipt::new(", "physical terminal disposal", False, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         "peers.authority.provider_effect_authority(", "restored target current authority", False, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         ".dispose_restored(&spec, expected, &authorization, &authority)", "restored target current authority", False, ""),
        (RUNTIME_HOST_TERMINAL_CLEANUP, "dispose_terminal_cleanup_effect",
         "with_current_sandbox_effect(", "restored target current authority", False, ""),
    ):
        mutant = dict(sources)
        text = _crate_boundary_workspace.production_rust(mutant[relative])
        body = _rust_function(text, function)
        assert body is not None, (relative, function)
        normalized = re.sub(r"\s*\n\s*\.", ".", body)
        cursor = normalized.index(after) + len(after) if after else 0
        prefix, suffix = normalized[:cursor], normalized[cursor:]
        assert marker in suffix, (relative, function, marker)
        changed = "".join(suffix.rsplit(marker, 1)) if last else suffix.replace(marker, "", 1)
        mutant[relative] = text.replace(body, prefix + changed, 1)
        assert mutant[relative] != text, (relative, function, marker)
        assert any(ownership in error for error in session_effect_violations(mutant)), (ownership, marker)


def selftest() -> None:
    # A method chain rustfmt broke across lines still matches its whole marker.
    wrapped = {"w.rs": "fn f() {\n    let a = self\n        .x\n        .y(1)?;\n    g(&a);\n}\n"}
    assert _require_ordered_function(wrapped, "w.rs", "f", ("self.x.y(", "g(&a)"), "chain") == []
    assert _require_ordered_function(wrapped, "w.rs", "f", ("g(&a)", "self.x.y("), "chain") != []
    # Cause/effect graph: C1 the Managed adapter projects committed Session
    # truth; C2 it constructs or directly mutates the aggregate. E1 C1 is
    # accepted, while E2 C2 is rejected before a second authority path lands.
    # The existing intent/receipt checks cover the independent effect bypass.
    canonical = {
        relative: "\n".join(markers) for relative, markers in REQUIRED.items()
    }
    canonical[SESSION_REPOSITORY_PUBLICATION_AGGREGATE] = """
pub fn verified_terminal_cleanup() {
    self.terminal_cleanup.verify_for(&self.session_id)?;
}
pub fn record_repository_publication_rejection() {
    rejection.effect_rejection.matches_source_binding(self.environment.terminal_repository_publication_binding());
    self.terminal_cleanup.record_repository_publication_rejection(&self.session_id, rejection);
}
"""
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
    preparation_authorization = """
fn authorize_terminal_cleanup_effect_from_root() {
    repository.get(&effect.command.session_id);
    self.require_current_live_terminal_generation(&session)?;
    repository.owner(&effect.command.session_id);
    terminal_restore_target_for_thread(&workspace_id, &session, &effect.command.thread_id);
    session.authorize_terminal_cleanup_effect(effect);
    let source_dispatch = runtime.source_dispatch_observation(&session).await.terminal_source_dispatch()?;
    SessionTerminalCleanupPreparationAuthorization::try_new(
        effect, workspace_id, inherited_provider_disposal,
    ).with_source_dispatch(source_dispatch);
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
    canonical[APPLICATION_CLEANUP] += preparation_authorization
    canonical[APPLICATION_RESOURCE_RECONCILIATION] = """
fn terminal_cleanup_projection() { session.terminal_cleanup_work_action(); }
fn require_current_live_terminal_generation(&self, session: &PersistedSession) {
    realization_lease_is_live_at(current.expires_at_unix_ms, self.clock.now_ms());
}
fn authorize_terminal_cleanup_disposal_from_root() {
    repository.get(&effect.command.session_id);
    repository.owner(&effect.command.session_id);
    session.authorize_terminal_cleanup_disposal_effect(&owner_scope, effect);
}
fn record_terminal_cleanup_preparation_from_root() {
    repository.session_aggregate(&session_id);
    session.record_terminal_cleanup_preparation(lease, preparation);
    "terminal-cleanup-preparation";
}
fn record_terminal_cleanup_disposal_from_root() {
    repository.session_aggregate(&session_id);
    session.record_terminal_cleanup_disposal(lease, receipt);
    "terminal-cleanup-disposal";
}
fn record_terminal_repository_publication_effect_from_root() {
    repository.get(session_id);
    session.verified_terminal_cleanup();
    match effect {
        Published(receipt) => session.record_repository_publication_receipt(session_id, receipt),
        Rejected(rejection) => session.lifecycle().record_repository_publication_rejection(rejection),
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
"""
    canonical[SESSION_DELETE_DISPOSITION] = """
SessionDeleteLifecycleFactPlan
event_type: "session.deleted"
"""
    canonical[SESSION_CLEANUP_RECOVERY] = """
pub async fn reconcile_terminal_cleanup() {
    crate::drive_session_terminal_cleanup(session_id, lease, control, runtime);
}
"""
    canonical[WORKER_PUBLICATION_ROUTE] = """
async fn session_repository_publication_poll() {
    verify_terminal_cleanup_authority(...).await;
    terminal_cleanup_control(&service).terminal_repository_publication_command(...);
}
async fn session_repository_publication_complete() {
    verify_terminal_cleanup_authority(...).await;
    terminal_cleanup_control(&service).record_terminal_repository_publication_receipt(...);
}
async fn session_repository_publication_reject() {
    verify_terminal_cleanup_authority(...).await;
    terminal_cleanup_control(&service).record_terminal_repository_publication_rejection(...);
}
"""
    canonical[SESSION_REALIZATION_CONTROL_DEADLINE] = """
async fn renew_session_realization() {
    self.call(
        "renew",
        self.inner.renew_session_realization(...),
    )
    .await
}
"""
    canonical[RUNTIME_BOUND_ENVIRONMENT] = """
fn install_bound_environment_under_lifecycle() {
    let authority = peers.authority.provider_effect_authority(...);
    provider.prepare_terminal_effective_for_effect(spec, handle, expected_effect_fence, effect_fence, &authority,);
    provider.rebind_retained(spec, &authority,);
}
fn prepare_bound_environment_for_effect_under_lifecycle() {
    provider.observe_effective(spec, handle).await;
    self.install_bound_environment_under_lifecycle(installation, resident, mode).await?;
    Ok(PreparedBoundEnvironment::new(environment, handle, mode))
}
"""
    canonical[RUNTIME_HOST_TERMINAL_PREPARATION] = """
fn disposal_authorization() {
    let current = self.authorize_lease(&effect.command.session_id, &effect.lease)?;
    effect.sandbox_disposal_authorization_for_current_generation(&current)
}
fn cleanup_terminal_checkpoint() {
    let fence = self.effect_fence(effect)?;
    let authority = peers.authority.provider_effect_authority(...);
    with_current_sandbox_effect(&fence, &authority, || Ok(()));
    match (checkpoint, pending_checkpoint) {
        provider.cleanup_checkpoint_for_terminal(request, store, expected, fence, &authority,);
    }
    with_current_sandbox_effect(&fence, &authority, || Ok(()));
}
"""
    canonical[RUNTIME_HOST_TERMINAL_CLEANUP] = """
fn prepare_terminal_cleanup_effect() {
    authorization.verify_for(&effect);
    self.effect_fence(&effect)?;
    TerminalEnvironmentPreparation::artifact_capture_mode;
    self.artifact_harvester().harvest_with_fence_mode(
        thread,
        Some(ResourceOperationFence::Terminal(effect)),
        capture_mode,
    );
    SessionCleanupPreparation::try_new(&effect, provider_prepared_effect_fence, artifact_receipts);
}
fn dispose_terminal_cleanup_effect() {
    let environments = peers.environments;
    root_realization.lock().await;
    self.disposal_authorization(&effect)?;
    sessions.lock_lifecycle(session_id.as_str()).await;
    let authorization = self.disposal_authorization(&effect)?;
    self.prepare_terminal_environment_under_lifecycle(
        session_id, authorization.effect_fence(), BoundEnvironmentPreparationMode::TerminalDisposal);
    let authority = peers.authority.provider_effect_authority(...);
    provider.dispose_restored(&spec, expected, &authorization, &authority);
    with_current_sandbox_effect(authorization.effect_fence(), &authority, || Ok(()));
    match owner {
        (SessionEnvironmentOwner::Retiring(retirement), None) => {
            let exact_authorization = self.disposal_authorization(&effect)?;
            if exact_authorization != authorization { return Err(changed); }
            let authority = peers.authority.provider_effect_authority(...);
            environment.dispose_for_effect(&authorization, &authority);
        }
    }
    sessions.end_terminal_disposal(session_id);
    self.disposal_authorization(&effect)?;
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
    let projected_environment = match exact_environment { Some(env) => Some(env.clone()), None => self.installed_sessions.read(thread)? };
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
    # Owner-migration decision table: M1 exact child+all guards -> accept;
    # M2 move the complete function to the retired parent -> reject; M3 keep
    # both implementations -> reject. Missing authority/read/attachment edges
    # are separately mutated by the shared runner on fixtures and real source.
    for keep_child in (False, True):
        mutant = dict(canonical)
        if not keep_child:
            mutant[APPLICATION_CLEANUP] = mutant[APPLICATION_CLEANUP].replace(preparation_authorization, "")
        mutant[APPLICATION_RESOURCE_RECONCILIATION] += preparation_authorization
        assert session_effect_violations(mutant), ("retired/duplicate preparation owner", keep_child)
    assert repository_publication_writer_violations(canonical) == [], (
        "canonical Repository publication writer"
    )

    # Current-row admission decision table: C1 shared tombstone authorization;
    # C2 shared observation preservation; C3 SQLite calls the shared gate;
    # C4 Postgres calls it. R1 all true => canonical source passes above;
    # R2-R5 remove each edge => reject that exact missing guard. These source
    # checks supplement real SQL/Kani mutants; they do not prove SQL semantics.
    for owner, marker in (
        (SESSION_STORE, "current.admits_tombstone("),
        (SESSION_STORE, ".validate_observation_replacement(replacement)"),
        (SQLITE_SESSION_STORE, "validate_current_mutation("),
        (POSTGRES_SESSION_STORE, "validate_current_mutation("),
    ):
        mutant = dict(canonical)
        assert marker in mutant[owner]
        mutant[owner] = mutant[owner].replace(marker, "")
        expected = f"{owner}: missing Session external-effect guard {marker!r}"
        assert expected in session_effect_violations(mutant), expected

    # G48 cause/effect rules: C1 one expected-prior declaration reaches the
    # sole local Git writer under an exact ref lease; C2 the shared cleanup
    # driver records its typed outcome through the one Session-root CAS before
    # Clock cause/effect rule: the full authority check stays present, but an
    # ambient sample would disagree with the installed Coordinator clock.
    clock_mutant = dict(canonical)
    clock_mutant[APPLICATION_RESOURCE_RECONCILIATION] = clock_mutant[
        APPLICATION_RESOURCE_RECONCILIATION
    ].replace("self.clock.now_ms()", "SystemClock.now_ms()", 1)
    assert session_effect_violations(clock_mutant), "ambient terminal clock rejected"

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
            "pub(crate) fn record_repository_publication_rejection(",
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
            "hosted renewal forwarding",
            SESSION_REALIZATION_CONTROL_DEADLINE,
            ".renew_session_realization(",
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

    # Physical-disposal wiring decision table: C1 capture the existing current
    # provider authority in this function; C2 forward that same authority with
    # the typed disposal receipt. Accept C1+C2; either omission must fail even
    # when the old typed-fence and aggregate-receipt markers remain present.
    for marker in (
        "let authority = peers.authority.provider_effect_authority(...);",
        ".dispose_for_effect(&authorization, &authority)",
    ):
        mutant = dict(canonical)
        # Remove the physical branch's last capture, not the restored branch's
        # first capture: a sibling failure must not satisfy this oracle.
        mutant[RUNTIME_HOST_TERMINAL_CLEANUP] = "".join(
            mutant[RUNTIME_HOST_TERMINAL_CLEANUP].rsplit(marker, 1)
        )
        assert any("physical terminal disposal" in error
                   for error in session_effect_violations(mutant)), marker

    # Reused against real current owner sources by focused source validation.
    _migrated_effect_owner_mutation_selftest(canonical)

    moved_back = dict(canonical)
    cold_body = _rust_function(moved_back[RUNTIME_BOUND_ENVIRONMENT],
                               "install_bound_environment_under_lifecycle")
    assert cold_body is not None
    moved_back[RUNTIME_BOUND_ENVIRONMENT] = moved_back[RUNTIME_BOUND_ENVIRONMENT].replace(cold_body, "")
    moved_back[RUNTIME_HOST_TERMINAL_PREPARATION] += "\n" + cold_body
    assert any("cold terminal reconstruction authority" in error
               for error in session_effect_violations(moved_back)), "old Host facade cannot satisfy real owner"

    # Checkpoint C1 capture, C2 pre-I/O admission, C3 port forwarding, C4
    # post-response admission. Removing either authority edge is rejected in
    # this function even though cold reconstruction retains all its markers.
    for marker, replacement, last in (
        ("peers.authority.provider_effect_authority(", "", False),
        ("with_current_sandbox_effect(", "", False),
        ("with_current_sandbox_effect(", "", True),
        (".cleanup_checkpoint_for_terminal(request, store, expected, fence, &authority,)",
         ".cleanup_checkpoint_for_terminal(request, store, expected, fence)", False),
    ):
        mutant = dict(canonical)
        body = _rust_function(mutant[RUNTIME_HOST_TERMINAL_PREPARATION],
                              "cleanup_terminal_checkpoint")
        assert body is not None
        changed = replacement.join(body.rsplit(marker, 1)) if last else body.replace(marker, replacement, 1)
        mutant[RUNTIME_HOST_TERMINAL_PREPARATION] = mutant[RUNTIME_HOST_TERMINAL_PREPARATION].replace(
            body, changed, 1)
        assert any("terminal checkpoint current authority" in error
                   for error in session_effect_violations(mutant)), marker

    # Restored target S1 capture, S2 exact forwarding, S3 post-response check.
    # Ordinary physical disposal remains intact and cannot mask this branch.
    for marker in ("let authority = peers.authority.provider_effect_authority(...);",
                   ".dispose_restored(&spec, expected, &authorization, &authority)",
                   "with_current_sandbox_effect("):
        mutant = dict(canonical)
        mutant[RUNTIME_HOST_TERMINAL_CLEANUP] = mutant[RUNTIME_HOST_TERMINAL_CLEANUP].replace(marker, "", 1)
        assert any("restored target current authority" in error
                   for error in session_effect_violations(mutant)), marker

    # G48 authority/role removal rules: C1 each destructive Worker route
    # verifies the authenticated cleanup generation in its own function before
    # reaching the exact terminal-cleanup Control role. The independent lease
    # renewal edge is the only request bounded by this deadline decorator; the
    # durable cleanup sweep intentionally awaits complete effects. E1 accepts
    # both narrow paths. E2 rejects a route that relies on a sibling marker or a
    # renewal wrapper that bypasses/swallows its bounded Control outcome.
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
            "hosted renewal deadline",
            "renew_session_realization",
            "renew",
            "renew_session_realization",
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
        assert bounded in mutant[SESSION_REALIZATION_CONTROL_DEADLINE], (
            f"canonical {function} deadline fixture"
        )
        mutant[SESSION_REALIZATION_CONTROL_DEADLINE] = mutant[
            SESSION_REALIZATION_CONTROL_DEADLINE
        ].replace(bounded, direct, 1)
        assert session_effect_violations(mutant), f"removed {rule} rejected"

        swallowed = dict(canonical)
        swallowed_call = (
            bounded.replace("    self.call(", "    let _ = self.call(", 1)
            + ";\n    Ok(())"
        )
        swallowed[SESSION_REALIZATION_CONTROL_DEADLINE] = swallowed[
            SESSION_REALIZATION_CONTROL_DEADLINE
        ].replace(bounded, swallowed_call, 1)
        assert session_effect_violations(swallowed), (
            f"swallowed {rule} Control outcome rejected"
        )

    for rule, owner, earlier, later in (
        (
            "root rejection before outcome CAS",
            APPLICATION_RESOURCE_RECONCILIATION,
            "session.lifecycle().record_repository_publication_rejection(rejection)",
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

    # Source-admission cause/effect table: SG1 the application calls the one
    # aggregate gate before its CAS -> accept; SG2 bypass to the private sidecar
    # or remove/move source comparison after mutation -> refuse. This extends
    # the existing owner/order oracle instead of tolerating both paths.
    for owner, before, after in (
        (APPLICATION_RESOURCE_RECONCILIATION,
         "session.lifecycle().record_repository_publication_rejection(rejection)",
         "session.terminal_cleanup.record_repository_publication_rejection(rejection)"),
        (SESSION_REPOSITORY_PUBLICATION_AGGREGATE,
         "matches_source_binding(self.environment.terminal_repository_publication_binding())",
         "unchecked_source_binding()"),
    ):
        mutant = dict(canonical)
        mutant[owner] = mutant[owner].replace(before, after)
        assert session_effect_violations(mutant), "SG2 source admission bypass rejected"
    mutant = dict(canonical)
    earlier = "matches_source_binding(self.environment.terminal_repository_publication_binding())"
    later = ".record_repository_publication_rejection(&self.session_id, rejection)"
    mutant[SESSION_REPOSITORY_PUBLICATION_AGGREGATE] = (
        mutant[SESSION_REPOSITORY_PUBLICATION_AGGREGATE]
        .replace(earlier, "__SOURCE_ORDER__", 1).replace(later, earlier, 1)
        .replace("__SOURCE_ORDER__", later, 1)
    )
    assert session_effect_violations(mutant), "SG2 late source comparison rejected"

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
        ("repository root mutation model", SESSION_REPOSITORY_MUTATION),
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

    # T8-T10 cause/effect table: C1 current slot authority precedes typed
    # lowering; C2 the SAME function adds a Worker-UTC decision; C3 unrelated
    # sibling code samples UTC. C1&&!C2 => accept even with C3; !C1 or C2 =>
    # reject. Require the exact oracle so another fixture failure cannot count.
    # This source fitness rule complements the executable R4/R5 clock matrix;
    # it is not Rust semantic refinement or a Provider commit-time proof.
    missing_current = dict(canonical)
    missing_current[RUNTIME_HOST_TERMINAL_PREPARATION] = missing_current[
        RUNTIME_HOST_TERMINAL_PREPARATION
    ].replace(
        "self.authorize_lease(&effect.command.session_id, &effect.lease)?",
        "effect.lease.clone()",
    )
    assert any("typed Sandbox disposal authorization" in error
               for error in session_effect_violations(missing_current)), "T8 current slot required"
    for clock in (
        "authorization.effect_fence().validate_live_at(now);",
        "let now = wall_clock_now_ms();",
        "let now = std::time::SystemTime::now();",
    ):
        duplicate_clock = dict(canonical)
        duplicate_clock[RUNTIME_HOST_TERMINAL_PREPARATION] = duplicate_clock[
            RUNTIME_HOST_TERMINAL_PREPARATION
        ].replace("    effect.sandbox_disposal", f"    {clock}\n    effect.sandbox_disposal")
        assert any("must not compare Control expiry with Worker wall time" in error
                   for error in session_effect_violations(duplicate_clock)), "T9 duplicate UTC rejected"
    sibling_clock = dict(canonical)
    sibling_clock[RUNTIME_HOST_TERMINAL_PREPARATION] += (
        "\nfn unrelated_durable_timestamp() { wall_clock_now_ms(); }\n"
    )
    assert session_effect_violations(sibling_clock) == [], "T10 rule remains function-scoped"

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
        "session_id, authorization.effect_fence(), BoundEnvironmentPreparationMode::TerminalDisposal",
        "session_id, authorization.prepared_effect_fence(), BoundEnvironmentPreparationMode::TerminalDisposal",
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
        "    let projected_environment = match exact_environment { Some(env) => Some(env.clone()), None => self.installed_sessions.read(thread)? };",
        "let projected_environment = match exact_environment { Some(env) => Some(env.clone()), None => self.installed_sessions.read(thread)? };\n"
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


if __name__ == "__main__":
    import sys

    if sys.argv[1:] != ["--self-test"]:
        print("usage: python3 scripts/ci/_session_effect_fitness.py --self-test", file=sys.stderr)
        raise SystemExit(2)
    selftest()
    print("_session_effect_fitness selftest: the decision tables hold")
