"""Canonical source owners and required markers for the Session effect checker."""

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
    "crates/server/awaken-session-application/src/terminal_cleanup/local_drive.rs"
)
APPLICATION_PUBLICATION_CONTROL = (
    "crates/server/awaken-session-application/src/terminal_cleanup.rs"
)
APPLICATION_EVENT_BATCHES = (
    "crates/server/awaken-session-application/src/event_batches.rs"
)
APPLICATION_TERMINAL = "crates/server/awaken-session-application/src/terminal.rs"
APPLICATION_LIFECYCLE = "crates/server/awaken-session-application/src/lifecycle.rs"
APPLICATION_WORK_DISPATCH = "crates/server/awaken-session-application/src/work_assignments.rs"
APPLICATION_TERMINAL_RECORDS = (
    "crates/server/awaken-session-application/src/terminal_cleanup/records.rs"
)
APPLICATION_RESOURCE_EFFECT_AUTHORIZATION = (
    "crates/server/awaken-session-application/src/resource_effect_authorization.rs"
)
PROTOCOL_SESSIONS = "crates/server/awaken-protocol-managed/src/state/sessions.rs"
SESSION_STORE = "crates/stores/awaken-session-store/src/lib.rs"
SQLITE_SESSION_STORE = "crates/stores/awaken-session-store/src/sqlite.rs"
POSTGRES_SESSION_STORE = "crates/stores/awaken-session-store/src/postgres.rs"
RUNTIME_HOST = "crates/server/awaken-session-executor/src/run_assembly/roles.rs"
REALIZATION_EFFECTS_ROLE = (
    "crates/server/awaken-session-realization/src/realization_executor/effects_role.rs"
)
INSTALLED_SESSION_PROJECTION = (
    "crates/server/awaken-session-executor/src/installed_session/frozen_projection.rs"
)
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
        "self.artifact_receipts != canonical.artifact_receipts",
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
        ".work_assignments.retire(&session)",
        ".ensure_terminal_cleanup_fence()",
        ".quiesce_terminal_delegations(session_id)",
        ".freeze_terminal_cleanup_targets(",
        '"terminal-cleanup-fence"',
        ".verified_terminal_cleanup()",
        ".authorize_terminal_cleanup_effect(effect)",
        "SessionTerminalCleanupPreparationAuthorization::try_new(",
    ),
    APPLICATION_PUBLICATION_CONTROL: (
        ".record_terminal_repository_publication_rejection_from_root(",
        "async fn record_terminal_repository_publication_rejection(",
    ),
    APPLICATION_RESOURCE_EFFECT_AUTHORIZATION: (
        "fn require_live_root_lease(",
        "async fn authorize_resource_materialization_intent(",
        "async fn authorize_checkpoint_release_artifact_effect(",
        "async fn authorize_terminal_memory_intent(",
    ),
    APPLICATION_EVENT_BATCHES: (".verified_terminal_cleanup()",),
    APPLICATION_TERMINAL_RECORDS: (
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
        ".lifecycle.request_reconciliation()",
        ".repository_publication_rejection(&command.session_id)",
        ".repository_publication_receipt(&command.session_id)",
    ),
    APPLICATION_LIFECYCLE: (
        "pub async fn commit_delete_intent(",
        ".request_delete(",
        ".commit_session_snapshot_with_outcome(",
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
    INSTALLED_SESSION_PROJECTION: (),
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
