// @formal evidence session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing
//! The realization boundary's one error, classified once by fault.
//!
//! Every realization operation fails with a [`RealizationError`]. Its variant
//! names the fault; [`RealizationError::disposition`] is the single decision of
//! what the caller does next; `From<RealizationError> for RunError` is the
//! single projection onto the neutral Run vocabulary. The claim resolver, the
//! projection synchronizer and the realization driver all derive their outcome
//! from that one decision, so one failure has one outcome on every path.
//! Codes are closed enums: a caller can match a code, never parse a message.

use std::borrow::Cow;

use awaken_provisioning_contract::{ProvisioningDeadline, SandboxError};
use awaken_resource_contract::RepositoryBindingVerifierError;
use awaken_session_realization_contract::{
    ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE, ENVIRONMENT_PROVISIONING_CODE,
    SessionRealizationControlDisposition, effect_failure_disposition, environment_effect_error,
    provisioning_disposition,
};
use awaken_session_state_contract::SessionEnvironmentReceiptRecoveryAction;
use awaken_session_types::{RunError, RunErrorKind};

/// What a caller does next with a failed realization operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The same request may succeed once authority or state settles, a raced
    /// concurrent change resolves or an unknown internal fault clears:
    /// relinquish the claim and retry, consuming one attempt of the caller's
    /// retry budget. An exhausted budget turns the retry into an absorbing
    /// outcome.
    Retry,
    /// No failure yet: nothing was realized for lack of capacity (ADR-0092),
    /// or the provider is still provisioning within its own deadline
    /// (ADR-0015). Park the request without consuming any retry budget.
    Defer,
    /// The effect can never succeed for this Session: absorb the Run.
    Terminal,
    /// The request itself is invalid: absorb the Run as the caller's fault.
    Rejected,
}

impl Disposition {
    /// The Worker effect class the Session realization contract defines for
    /// this disposition. `Rejected` and `Terminal` both absorb.
    #[must_use]
    pub const fn contract_class(self) -> SessionRealizationControlDisposition {
        match self {
            Self::Retry => SessionRealizationControlDisposition::Retryable,
            Self::Defer => SessionRealizationControlDisposition::NotReady,
            Self::Terminal | Self::Rejected => SessionRealizationControlDisposition::Terminal,
        }
    }

    /// The disposition of one contract Worker class. The contract's
    /// `Terminal` is this boundary's `Terminal`: a request fault is told apart
    /// only by [`Disposition::of_run_error`].
    #[must_use]
    pub const fn of_contract_class(class: SessionRealizationControlDisposition) -> Self {
        match class {
            SessionRealizationControlDisposition::NotReady => Self::Defer,
            SessionRealizationControlDisposition::Retryable => Self::Retry,
            SessionRealizationControlDisposition::Terminal => Self::Terminal,
        }
    }

    /// Classify a neutral Run error the way the contract does: a capacity or
    /// provisioning code on an Unavailable fault defers; any other Unavailable fault, a conflict
    /// and an unclassified internal fault retry; a BadRequest is the caller's
    /// fault and a classified internal fault is terminal.
    #[must_use]
    pub fn of_run_error(error: &RunError) -> Self {
        match (error.kind, effect_failure_disposition(error)) {
            (_, SessionRealizationControlDisposition::NotReady) => Self::Defer,
            (_, SessionRealizationControlDisposition::Retryable) => Self::Retry,
            (RunErrorKind::BadRequest, SessionRealizationControlDisposition::Terminal) => {
                Self::Rejected
            }
            (_, SessionRealizationControlDisposition::Terminal) => Self::Terminal,
        }
    }
}

/// Declare one closed code set: each variant has exactly one stable code.
macro_rules! closed_codes {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $code:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Every code of this class, for exhaustive decision tables.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The stable code protocol adapters project.
            #[must_use]
            pub const fn code(self) -> &'static str {
                match self {
                    $(Self::$variant => $code,)+
                }
            }
        }
    };
}

closed_codes! {
    /// The realization authority an effect relied on no longer authorizes it.
    /// A newer owner, lease or claim may proceed, so every cause retries.
    AuthorityLoss {
        /// The Session's realization proof is closed or no longer live.
        Closed => "session_realization_authority_closed",
        /// The authority clock could not start a realization proof.
        ProofClockUnavailable => "session_realization_authority_clock_unavailable",
        /// An Environment effect lost its exact owner fence.
        EffectFenceLost => "session_environment_effect_fence_lost",
        /// The realization generation changed under the effect.
        GenerationChanged => "session_realization_generation_changed",
        /// The Worker's registry proof refused the effect.
        WorkerDenied => "worker_execution_authority_denied",
        /// No Session is installed for the effect in this process.
        SessionAbsent => "session_realization_absent",
        /// The dispatch claim the effect was fenced by is stale.
        ClaimStale => "dispatch_claim_stale",
        /// The frozen Session root projection is older than the installed one.
        ProjectionStale => "session_projection_stale",
        /// An Environment adoption lost its authority.
        AdoptionAuthorityLost => "session_environment_adoption_authority_lost",
        /// The Environment realization was closed under the effect.
        RealizationClosed => "session_environment_realization_closed",
        /// The durable binding authority for the Environment is missing.
        BindingAuthorityMissing => "session_environment_binding_authority_missing",
        /// A terminal Repository rebind lost its authority.
        RepositoryRebindAuthorityLost => "session_terminal_repository_rebind_authority_lost",
        /// A checkpoint source owner changed under the effect.
        CheckpointSourceOwnerChanged => "session_checkpoint_source_owner_changed",
        /// The terminal Environment owner changed under the effect.
        TerminalOwnerChanged => "session_terminal_environment_owner_changed",
        /// The terminal Environment preparation changed under the effect.
        TerminalPreparationChanged => "session_terminal_environment_preparation_changed",
        /// The terminal restore target changed under the effect.
        RestoreTargetChanged => "session_terminal_restore_target_changed",
        /// The terminal cleanup generation is stale.
        CleanupGenerationStale => "session_terminal_cleanup_generation_stale",
        /// The terminal cleanup effect expired.
        CleanupEffectExpired => "session_terminal_cleanup_effect_expired",
        /// The terminal cleanup disposal is not authorized.
        CleanupDisposalUnauthorized => "session_terminal_cleanup_disposal_unauthorized",
        /// The terminal cleanup preparation is not authorized.
        CleanupPreparationUnauthorized => "session_terminal_cleanup_preparation_unauthorized",
        /// The terminal cleanup disposal changed under the effect.
        CleanupDisposalChanged => "session_terminal_cleanup_disposal_changed",
    }
}

closed_codes! {
    /// The Environment or its evidence is transitional, unobserved or not yet
    /// prepared. The same effect may proceed once it settles, so every cause
    /// retries.
    PendingState {
        DispatchSandboxBindingUnavailable => "dispatch_sandbox_binding_unavailable",
        RepositoryPublicationTransportUnavailable => "repository_publication_transport_unavailable",
        CheckpointSourceNotPrepared => "session_checkpoint_source_not_prepared",
        AdoptionIndeterminate => "session_environment_adoption_indeterminate",
        CandidateIntentUnknown => "session_environment_candidate_intent_unknown",
        CandidateObservationClosed => "session_environment_candidate_observation_closed",
        CheckpointStoreUnavailable => "session_environment_checkpoint_store_unavailable",
        EffectObservationIndeterminate => "session_environment_effect_observation_indeterminate",
        LiveSourceNotReady => "session_environment_live_source_not_ready",
        ObservationIndeterminate => "session_environment_observation_indeterminate",
        OwnedRootMissing => "session_environment_owned_root_missing",
        RevocationPendingAdoption => "session_environment_revocation_pending_adoption",
        LiveAdoptionIndeterminate => "session_live_environment_adoption_indeterminate",
        LiveOwnerNotResident => "session_live_environment_owner_not_resident",
        TerminalArtifactHarvestPending => "session_terminal_artifact_harvest_pending",
        TerminalBackgroundNotQuiescent => "session_terminal_background_not_quiescent",
        TerminalCheckpointLegacyUnfenced => "session_terminal_checkpoint_legacy_unfenced",
        TerminalCheckpointPolicyMissing => "session_terminal_checkpoint_policy_missing",
        TerminalCheckpointSourceMissing => "session_terminal_checkpoint_source_missing",
        TerminalCheckpointWorkspaceMissing => "session_terminal_checkpoint_workspace_missing",
        TerminalCleanupProjectionMissing => "session_terminal_cleanup_projection_missing",
        TerminalCleanupProviderPreparationInvalid => "session_terminal_cleanup_provider_preparation_invalid",
        TerminalCleanupWorkspaceMissing => "session_terminal_cleanup_workspace_missing",
        TerminalEnvironmentNotPrepared => "session_terminal_environment_not_prepared",
        TerminalEnvironmentNotQuiescent => "session_terminal_environment_not_quiescent",
        TerminalPreparationIndeterminate => "session_terminal_environment_preparation_indeterminate",
        TerminalEnvironmentStateMissing => "session_terminal_environment_state_missing",
        TerminalHandNotQuiescent => "session_terminal_hand_not_quiescent",
        TerminalMcpNotQuiescent => "session_terminal_mcp_not_quiescent",
        TerminalMemoryEvidenceUnreconciled => "session_terminal_memory_evidence_unreconciled",
        TerminalMemorySourceAbsent => "session_terminal_memory_source_absent",
        TerminalRepositorySourceRebindIndeterminate => "session_terminal_repository_source_rebind_indeterminate",
        TerminalRestoreWorkspaceMissing => "session_terminal_restore_workspace_missing",
        TerminalUnpublishedCandidate => "session_terminal_unpublished_candidate",
        /// A terminal effect's provider or store step failed; terminal cleanup
        /// stays pending and retries whatever the step's own class was.
        TerminalEffectUnavailable => "unavailable",
    }
}

closed_codes! {
    /// Installed or observed state contradicts the exact projection the effect
    /// was authorized for. Retrying the same effect reproduces the conflict.
    ProjectionConflict {
        EffectIncompatible => "session_environment_effect_incompatible",
        ObservationIncompatible => "session_environment_observation_incompatible",
        TerminalCleanupWorkspaceMismatch => "session_terminal_cleanup_workspace_mismatch",
        TerminalBindingMismatch => "session_terminal_environment_binding_mismatch",
        TerminalProjectionConflict => "session_terminal_environment_projection_conflict",
        TerminalRestoreOwnerConflict => "session_terminal_restore_owner_conflict",
        TerminalRestoreRequestConflict => "session_terminal_restore_request_conflict",
        /// A claimed Run's Agent publication contradicts the frozen Session.
        PublicationConflict => "session_runtime_publication_conflict",
    }
}

/// A lock a panicking holder left poisoned. Realization denies the effect
/// instead of unwinding through the poisoned state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RealizationLock {
    #[error("installed Sessions lock poisoned")]
    InstalledSessions,
    #[error("Environment binding sink lock poisoned")]
    EnvironmentBindingSink,
    #[error("Memory execution context lock poisoned")]
    MemoryExecutionContext,
}

/// Stable code of every [`RealizationError::Poisoned`].
pub const POISONED_CODE: &str = "session_realization_lock_poisoned";

/// Durable or frozen evidence an effect consumes is malformed or does not
/// prove what the effect needs. No retry of the same effect can repair it.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceFault {
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
    #[error("invalid Session sandbox binding: {0}")]
    InvalidBinding(serde_json::Error),
    #[error("Session Environment effect has an invalid source binding: {0}")]
    InvalidSourceBinding(serde_json::Error),
    #[error("frozen Session model override publication is invalid: {0}")]
    ModelOverridePublication(awaken_session_configuration_contract::SessionModelResolutionError),
    #[error(transparent)]
    Fingerprint(#[from] awaken_agent_contract::CanonicalJsonHashError),
    #[error(transparent)]
    SessionInput(#[from] awaken_session_configuration_contract::resource::SessionInputError),
    #[error(transparent)]
    MemoryReference(#[from] awaken_resource_contract::MemoryMaterializationReferenceError),
    #[error(transparent)]
    TerminalCleanup(#[from] awaken_session_state_contract::SessionCleanupError),
}

/// Each evidence source converts through [`EvidenceFault`].
macro_rules! evidence_sources {
    ($($source:ty),+ $(,)?) => {
        $(impl From<$source> for RealizationError {
            fn from(error: $source) -> Self {
                Self::Evidence(error.into())
            }
        })+
    };
}

evidence_sources!(
    serde_json::Error,
    awaken_agent_contract::CanonicalJsonHashError,
    awaken_session_configuration_contract::resource::SessionInputError,
    awaken_resource_contract::MemoryMaterializationReferenceError,
    awaken_session_state_contract::SessionCleanupError,
);

impl From<EvidenceFault> for RealizationError {
    fn from(error: EvidenceFault) -> Self {
        Self::Evidence(error)
    }
}

/// The Session aggregate refused an Environment intent, receipt or law. The
/// refusal carries its own retry classification
/// ([`SessionEnvironmentReceiptRecoveryAction`]); realization reads it and
/// never a variant name.
#[derive(Debug, thiserror::Error)]
pub enum EnvironmentRefusal {
    #[error(transparent)]
    Receipt(#[from] awaken_session_state_contract::SessionEnvironmentReceiptError),
    #[error(transparent)]
    Transition(#[from] awaken_session_state_contract::SessionEnvironmentTransitionError),
}

impl EnvironmentRefusal {
    /// The refusal's own classification: retry once the Session moves on,
    /// or final for the same evidence.
    #[must_use]
    pub const fn recovery_action(&self) -> SessionEnvironmentReceiptRecoveryAction {
        match self {
            Self::Receipt(error) => error.recovery_action(),
            Self::Transition(error) => error.recovery_action(),
        }
    }
}

/// Each refusal source converts through [`EnvironmentRefusal`].
macro_rules! refusal_sources {
    ($($source:ty),+ $(,)?) => {
        $(impl From<$source> for RealizationError {
            fn from(error: $source) -> Self {
                Self::EnvironmentRefused(error.into())
            }
        })+
    };
}

refusal_sources!(
    awaken_session_state_contract::SessionEnvironmentReceiptError,
    awaken_session_state_contract::SessionEnvironmentTransitionError,
);

/// Why a request cannot be realized as stated. Retrying it unchanged fails
/// the same way.
#[derive(Debug, thiserror::Error)]
pub enum InvalidRequest {
    #[error("{0}")]
    Stated(Cow<'static, str>),
    /// The provider cannot lay out the Session's frozen Environment.
    #[error(transparent)]
    EnvironmentLayout(SandboxError),
    /// The provider cannot prepare the Session's frozen Environment.
    #[error(transparent)]
    EnvironmentPreparation(#[from] awaken_provisioning_contract::PrepareError),
}

/// One failed realization operation, classified by fault.
#[derive(Debug, thiserror::Error)]
pub enum RealizationError {
    /// The sandbox provider failed; its own class decides the disposition.
    #[error(transparent)]
    Environment(#[from] SandboxError),
    /// The realization authority the effect relied on is gone.
    #[error("{message}")]
    AuthorityLost {
        cause: AuthorityLoss,
        message: Cow<'static, str>,
    },
    /// The Environment or its evidence has not settled yet.
    #[error("{message}")]
    Pending {
        state: PendingState,
        message: Cow<'static, str>,
    },
    /// The provider still reports the Environment provisioning. `deadline`
    /// is the provider's bound on that wait as read at the observation; the
    /// provider owns it, so this failure judges it and never reads a clock.
    #[error("{message}")]
    Provisioning {
        deadline: Option<ProvisioningDeadline>,
        message: Cow<'static, str>,
    },
    /// Installed state contradicts the projection the effect was authorized for.
    #[error("{message}")]
    ProjectionConflict {
        conflict: ProjectionConflict,
        message: Cow<'static, str>,
    },
    /// The Repository binding authority refused or failed the binding.
    #[error(transparent)]
    RepositoryBinding(#[from] RepositoryBindingVerifierError),
    /// Credential material could not be resolved for an effect.
    #[error(transparent)]
    Credential(#[from] awaken_credential_contract::CredentialMaterialError),
    /// A selected File's content could not be read for a projection.
    #[error(transparent)]
    FileContent(#[from] awaken_resource_contract::FileContentSourceError),
    /// A CacheVolume could not be prepared before the provider binds it.
    #[error(transparent)]
    CacheVolume(#[from] crate::cache_volume::CacheVolumeError),
    /// A Session MCP operation failed; it keeps its own class and code.
    #[error(transparent)]
    Mcp(#[from] awaken_session_mcp::McpError),
    /// A neutral Run failure from a contract port; it keeps its own class.
    #[error("{}", .0.message)]
    Run(#[from] RunError),
    /// Evidence the effect consumes is malformed or contradicts it.
    #[error(transparent)]
    Evidence(EvidenceFault),
    /// The Session aggregate refused the Environment event; the refusal's
    /// own classification decides the disposition.
    #[error(transparent)]
    EnvironmentRefused(EnvironmentRefusal),
    /// The request cannot be realized as stated.
    #[error(transparent)]
    InvalidRequest(#[from] InvalidRequest),
    /// A realization invariant does not hold; no retry can repair it.
    #[error("{0}")]
    Invariant(Cow<'static, str>),
    /// A lock's holder panicked; the effect is denied.
    #[error(transparent)]
    Poisoned(#[from] RealizationLock),
}

impl RealizationError {
    pub fn authority_lost(cause: AuthorityLoss, message: impl Into<Cow<'static, str>>) -> Self {
        Self::AuthorityLost {
            cause,
            message: message.into(),
        }
    }

    pub fn pending(state: PendingState, message: impl Into<Cow<'static, str>>) -> Self {
        Self::Pending {
            state,
            message: message.into(),
        }
    }

    /// The provider observed the Environment still provisioning, with the
    /// deadline (if any) that observation carried.
    pub fn provisioning(
        deadline: Option<ProvisioningDeadline>,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::Provisioning {
            deadline,
            message: message.into(),
        }
    }

    /// A terminal effect's provider or store step failed: terminal cleanup
    /// stays pending and retries, whatever the step's own class was.
    pub fn terminal_effect_pending(error: impl std::fmt::Display) -> Self {
        Self::pending(PendingState::TerminalEffectUnavailable, error.to_string())
    }

    pub fn conflict(conflict: ProjectionConflict, message: impl Into<Cow<'static, str>>) -> Self {
        Self::ProjectionConflict {
            conflict,
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<Cow<'static, str>>) -> Self {
        Self::InvalidRequest(InvalidRequest::Stated(message.into()))
    }

    pub fn invariant(message: impl Into<Cow<'static, str>>) -> Self {
        Self::Invariant(message.into())
    }

    /// No Session is installed for the operation in this process.
    #[must_use]
    pub fn session_absent(session: &str) -> Self {
        Self::authority_lost(
            AuthorityLoss::SessionAbsent,
            format!("Session `{session}` is not installed in this process"),
        )
    }

    /// The one decision of what the caller does next with this failure.
    #[must_use]
    pub fn disposition(&self) -> Disposition {
        match self {
            Self::Environment(error) => environment_disposition(error),
            Self::AuthorityLost { .. } | Self::Pending { .. } | Self::Poisoned(_) => {
                Disposition::Retry
            }
            Self::Provisioning { deadline, .. } => {
                Disposition::of_contract_class(provisioning_disposition(deadline.as_ref()))
            }
            Self::ProjectionConflict { .. } | Self::Evidence(_) | Self::Invariant(_) => {
                Disposition::Terminal
            }
            Self::EnvironmentRefused(refusal) => match refusal.recovery_action() {
                SessionEnvironmentReceiptRecoveryAction::Retry => Disposition::Retry,
                SessionEnvironmentReceiptRecoveryAction::Reject => Disposition::Terminal,
            },
            Self::RepositoryBinding(error) => repository_binding_disposition(error),
            Self::Credential(awaken_credential_contract::CredentialMaterialError::Unavailable) => {
                Disposition::Retry
            }
            Self::Credential(_) => Disposition::Terminal,
            Self::CacheVolume(error) => error.disposition(),
            // An unknown read may succeed later; a digest mismatch or a purged
            // source never will.
            Self::FileContent(
                awaken_resource_contract::FileContentSourceError::Indeterminate { .. },
            ) => Disposition::Retry,
            Self::FileContent(_) => Disposition::Terminal,
            Self::Mcp(error) => Disposition::of_run_error(&mcp_run_error(error)),
            Self::Run(error) => Disposition::of_run_error(error),
            Self::InvalidRequest(_) => Disposition::Rejected,
        }
    }

    /// When a deferred request is worth observing again: a provision within
    /// its deadline needs no new look after that deadline, so the caller's
    /// backoff never sleeps past it. Every other failure leaves the cadence
    /// to the caller.
    #[must_use]
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::Provisioning {
                deadline: Some(deadline),
                ..
            } => deadline.remaining(),
            _ => None,
        }
    }

    /// The stable code of this failure, as its Run projection carries it.
    #[must_use]
    pub fn code(&self) -> Cow<'_, str> {
        match self {
            Self::Environment(error) => Cow::Owned(environment_effect_error(error).code),
            Self::AuthorityLost { cause, .. } => Cow::Borrowed(cause.code()),
            Self::Pending { state, .. } => Cow::Borrowed(state.code()),
            Self::Provisioning { deadline, .. } => Cow::Borrowed(provisioning_code(*deadline)),
            Self::ProjectionConflict { conflict, .. } => Cow::Borrowed(conflict.code()),
            Self::RepositoryBinding(error) => Cow::Borrowed(repository_binding_code(error)),
            Self::Mcp(error) => Cow::Borrowed(error.code.as_str()),
            Self::Run(error) => Cow::Borrowed(error.code.as_str()),
            Self::InvalidRequest(_) => Cow::Borrowed("invalid_request"),
            Self::Poisoned(_) => Cow::Borrowed(POISONED_CODE),
            Self::Credential(_) => self.class_code(UnrecoverableFault::Credential),
            Self::FileContent(_) => self.class_code(UnrecoverableFault::FileContent),
            Self::CacheVolume(_) => self.class_code(UnrecoverableFault::CacheVolume),
            Self::Evidence(_) | Self::EnvironmentRefused(_) => {
                self.class_code(UnrecoverableFault::Evidence)
            }
            Self::Invariant(_) => self.class_code(UnrecoverableFault::Invariant),
        }
    }

    /// The code of a failure whose variant carries no finer code: its
    /// disposition's generic code, or the variant's own closed code when it is
    /// terminal. That code is never the unclassified internal code, so the
    /// contract keeps classifying the projection as terminal rather than as an
    /// unknown, retryable fault.
    fn class_code(&self, terminal: UnrecoverableFault) -> Cow<'static, str> {
        Cow::Borrowed(match self.disposition() {
            Disposition::Retry => "unavailable",
            Disposition::Defer => ENVIRONMENT_CAPACITY_UNAVAILABLE_CODE,
            Disposition::Rejected => "invalid_request",
            Disposition::Terminal => terminal.code(),
        })
    }
}

closed_codes! {
    /// A failure no retry can repair whose variant carries no finer code.
    UnrecoverableFault {
        /// Credential material is permanently unusable for the effect.
        Credential => "credential_material_rejected",
        /// A selected File's content is corrupt or purged.
        FileContent => "file_content_unrecoverable",
        /// A CacheVolume cannot be prepared at its location.
        CacheVolume => "cache_volume_unrecoverable",
        /// Evidence the effect consumes is malformed.
        Evidence => "session_realization_evidence_invalid",
        /// A realization invariant does not hold.
        Invariant => "session_realization_invariant_violated",
    }
}

/// Stable code of a provisioning wait with no deadline: no owner progresses
/// it (or the observation predates deadlines), so it is a failed attempt.
pub const PROVISIONING_UNBOUNDED_CODE: &str = "session_environment_provisioning";
/// Stable code of a provisioning wait observed at or after the provider's
/// deadline: a failed attempt.
pub const PROVISIONING_DEADLINE_ELAPSED_CODE: &str =
    "session_environment_provisioning_deadline_elapsed";

/// The code of a provisioning wait: within its deadline it is the contract's
/// not-ready code; otherwise it names why the wait became a failed attempt.
fn provisioning_code(deadline: Option<ProvisioningDeadline>) -> &'static str {
    match (deadline, provisioning_disposition(deadline.as_ref())) {
        (_, SessionRealizationControlDisposition::NotReady) => ENVIRONMENT_PROVISIONING_CODE,
        (None, _) => PROVISIONING_UNBOUNDED_CODE,
        (Some(_), _) => PROVISIONING_DEADLINE_ELAPSED_CODE,
    }
}

/// A Session MCP failure keeps its fault and code across the Run boundary.
fn mcp_run_error(error: &awaken_session_mcp::McpError) -> RunError {
    RunError {
        kind: match error.kind {
            awaken_session_mcp::McpErrorKind::Internal => RunErrorKind::Internal,
            awaken_session_mcp::McpErrorKind::BadRequest => RunErrorKind::BadRequest,
            awaken_session_mcp::McpErrorKind::Unavailable => RunErrorKind::Unavailable,
        },
        message: error.message.clone(),
        code: error.code.clone(),
    }
}

fn environment_disposition(error: &SandboxError) -> Disposition {
    use awaken_agent_contract::resilience::{Classify, Disposition as Provider};
    if matches!(error, SandboxError::CapacityUnavailable(_)) {
        return Disposition::Defer;
    }
    match error.disposition() {
        Provider::Transient | Provider::Quota { .. } => Disposition::Retry,
        Provider::Permanent => Disposition::Terminal,
    }
}

fn repository_binding_disposition(error: &RepositoryBindingVerifierError) -> Disposition {
    match error {
        RepositoryBindingVerifierError::InvalidRequest { .. }
        | RepositoryBindingVerifierError::Denied { .. } => Disposition::Rejected,
        // The binding authority changed concurrently: an optimistic-concurrency
        // loss, retried within the budget against the settled authority.
        RepositoryBindingVerifierError::Conflict { .. }
        | RepositoryBindingVerifierError::AuthorityUnavailable { .. }
        | RepositoryBindingVerifierError::Indeterminate { .. } => Disposition::Retry,
        RepositoryBindingVerifierError::CorruptAuthority { .. } => Disposition::Terminal,
    }
}

/// Stable code of a Repository binding authority that returned corrupt data.
const REPOSITORY_BINDING_CORRUPT_CODE: &str = "repository_binding_authority_corrupt";

fn repository_binding_code(error: &RepositoryBindingVerifierError) -> &'static str {
    match error {
        RepositoryBindingVerifierError::InvalidRequest { .. }
        | RepositoryBindingVerifierError::Denied { .. } => "invalid_request",
        RepositoryBindingVerifierError::Conflict { .. } => {
            awaken_session_types::error::CONFLICT_CODE
        }
        RepositoryBindingVerifierError::AuthorityUnavailable { .. }
        | RepositoryBindingVerifierError::Indeterminate { .. } => "unavailable",
        RepositoryBindingVerifierError::CorruptAuthority { .. } => REPOSITORY_BINDING_CORRUPT_CODE,
    }
}

/// A Repository binding failure keeps its fault across the Run boundary: a
/// concurrently changed authority stays a conflict.
fn repository_binding_run_error(error: &RepositoryBindingVerifierError) -> RunError {
    let message = error.to_string();
    let code = repository_binding_code(error);
    match error {
        RepositoryBindingVerifierError::InvalidRequest { .. }
        | RepositoryBindingVerifierError::Denied { .. } => {
            RunError::bad_request_classified(code, message)
        }
        RepositoryBindingVerifierError::Conflict { .. } => {
            RunError::conflict_classified(code, message)
        }
        RepositoryBindingVerifierError::AuthorityUnavailable { .. }
        | RepositoryBindingVerifierError::Indeterminate { .. } => {
            RunError::unavailable_classified(code, message)
        }
        RepositoryBindingVerifierError::CorruptAuthority { .. } => {
            RunError::classified(code, message)
        }
    }
}

/// The one projection of a realization failure onto the neutral Run error.
/// Its kind and code are chosen so that the contract's own classification of
/// the projected error equals [`RealizationError::disposition`].
impl From<RealizationError> for RunError {
    fn from(error: RealizationError) -> Self {
        let code = error.code().into_owned();
        let disposition = error.disposition();
        match error {
            RealizationError::Environment(error) => environment_effect_error(&error),
            RealizationError::Run(error) => error,
            RealizationError::Mcp(error) => mcp_run_error(&error),
            RealizationError::RepositoryBinding(error) => repository_binding_run_error(&error),
            other => {
                let message = other.to_string();
                match disposition {
                    Disposition::Retry => RunError::unavailable_classified(code, message),
                    // Every deferring variant's code is a not-ready code.
                    Disposition::Defer => RunError::unavailable_classified(code, message),
                    Disposition::Terminal => RunError::classified(code, message),
                    Disposition::Rejected => RunError::bad_request_classified(code, message),
                }
            }
        }
    }
}

/// A CacheVolume failure is classified here, with every other realization
/// failure, so one place decides its disposition.
impl crate::cache_volume::CacheVolumeError {
    /// A wrong location is a deployment fault no retry repairs; a provider step
    /// keeps the provider's class; every other failure may succeed later.
    #[must_use]
    pub fn disposition(&self) -> Disposition {
        match self {
            Self::UnsupportedLocation(_) => Disposition::Terminal,
            Self::Environment { source, .. } => {
                RealizationError::Environment(source.clone()).disposition()
            }
            Self::Task(_)
            | Self::Directory(_)
            | Self::InitializerExited(_)
            | Self::Initializer(_) => Disposition::Retry,
            Self::Volume { source, .. } => source.disposition(),
        }
    }
}

#[cfg(test)]
#[path = "realization_error/tests.rs"]
mod tests;
