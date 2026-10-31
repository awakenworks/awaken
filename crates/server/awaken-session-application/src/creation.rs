// @formal entrypoint session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
// @formal entrypoint session_creation.atomic_complete_root_precedes_every_projection
// @formal entrypoint session_creation.exact_replay_conflict_and_tombstone_are_closed
// @formal entrypoint session_creation.repository_receipt_is_the_only_post_create_effect_gate
// @formal entrypoint session_event_batch.initial_plan_and_wake_activity_are_atomic_with_root
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
//! Canonical durable Session creation protocol.
//!
//! Public protocols lower their wire inputs into [`SessionCreationIntent`].
//! This module alone owns the subsequent insert, finalization, realization,
//! activation, lifecycle-fact commit, and WorkQueue projection ordering.

use awaken_session_configuration_contract::mcp::SessionMcpAttachmentSet;
use awaken_session_configuration_contract::{
    CompiledSessionCreation, SessionCreationIntent, SessionToolConfiguration,
};
use awaken_session_contract::SessionBudgetState;
use awaken_session_contract::{
    IdempotencyRecord, PersistedSession, SessionAggregate, SessionExecutionState,
    SessionMutationPayload,
};
use awaken_session_realization_contract::SessionRealizationControlDisposition;
use awaken_session_realization_contract::SessionRealizationDriveError;
use awaken_session_types::RunError;

use std::sync::Arc;

use super::SessionMutationError;

mod baseline;
#[cfg(test)]
mod error_classification_tests;
mod profiled;
mod request;

pub use profiled::{CreateProfiledSessionCommand, ProfiledSessionRepositoryInput};
pub use request::{
    SessionBudgetRequest, SessionCreateProtocol, SessionCreateRequest, SessionCreateRequestError,
    SessionModelRequest, SessionToolsOverride,
};

/// The only value admitted to the Session root-insert boundary.
///
/// Construction consumes the finalized creation graph and verifies the exact
/// fresh-root shape. Keeping the field private prevents protocol adapters and
/// later mutation paths from persisting an unconsumed authoring intent through
/// the application create path.
pub(crate) struct CompleteSessionRoot(SessionAggregate);

fn root_has_consumed_creation_intent(session: &PersistedSession) -> bool {
    session.frozen_baseline().is_some()
}

impl CompleteSessionRoot {
    fn compile(
        session_id: String,
        title: Option<String>,
        metadata: std::collections::BTreeMap<String, String>,
        tools: SessionToolConfiguration,
        budget: SessionBudgetState,
        compiled: CompiledSessionCreation,
        (initial_events, initial_agent_configuration): (
            Option<awaken_session_contract::SessionInitialEventPlan>,
            Option<Box<awaken_session_configuration_contract::SessionAgentConfiguration>>,
        ),
    ) -> Result<Self, SessionCreationError> {
        let holder = compiled
            .baseline
            .environment
            .credential_realization
            .mcp_holder
            .clone();
        let mut resources = awaken_session_contract::SessionResourceState::default();
        resources
            .prepare(
                &awaken_session_types::SessionId::from(session_id.as_str()),
                compiled.initial_resources,
            )
            .map_err(|error| SessionCreationError::Unavailable(error.to_string()))?;
        let mcp = SessionMcpAttachmentSet::from_initial(compiled.initial_mcp, Some(holder))
            .map_err(|error| SessionCreationError::Unavailable(error.to_string()))?;
        let session = PersistedSession::frozen_with_budget(
            session_id,
            compiled.baseline,
            resources,
            mcp,
            title,
            metadata,
            tools,
            budget,
        );
        let mut root = Self::from_fresh(session)?;
        if let Some(configuration) = initial_agent_configuration {
            root.0
                .record_initial_agent_configuration(configuration)
                .map_err(|error| {
                    SessionCreationError::Rejected(RunError::bad_request(error.to_string()))
                })?;
        }
        if let Some(initial_events) = initial_events {
            root.0
                .activity()
                .install_initial_event_plan(initial_events)
                .map_err(|error| {
                    SessionCreationError::Rejected(RunError::bad_request(error.to_string()))
                })?;
        }
        Ok(root)
    }

    pub(super) fn from_fresh(session: PersistedSession) -> Result<Self, SessionCreationError> {
        if session.revision != awaken_session_types::SessionRevision(0)
            || !session
                .run_activities()
                .is_some_and(|(records, unrecorded)| records.is_empty() && !unrecorded)
            || !session
                .environment_materialization()
                .is_some_and(awaken_session_contract::SessionEnvironmentMaterialization::is_empty)
            || !root_has_consumed_creation_intent(&session)
            || session.execution != SessionExecutionState::Preparing
            || !session.needs_initial_realization_recovery()
        {
            return Err(SessionCreationError::Internal(
                "compiled Session root is incomplete or not recoverable".into(),
            ));
        }
        Ok(Self(
            SessionAggregate::try_from(session).map_err(crate::SessionStateViolation::from)?,
        ))
    }

    pub(super) fn as_session(&self) -> &PersistedSession {
        &self.0
    }

    pub(super) fn observe_creation(&mut self, now: u64) -> Result<(), SessionCreationError> {
        self.0
            .observe_creation(now)
            .map_err(|error| crate::SessionStateViolation::from(error).into())
    }

    pub(super) fn into_session(self) -> PersistedSession {
        self.0.into_snapshot()
    }
}

pub(crate) struct DurableSessionRoot(PersistedSession);

impl DurableSessionRoot {
    pub(super) fn from_applied(session: PersistedSession) -> Result<Self, SessionCreationError> {
        if session.revision != awaken_session_types::SessionRevision(1)
            || !session
                .environment_materialization()
                .is_some_and(awaken_session_contract::SessionEnvironmentMaterialization::is_empty)
            || session.created_at_unix_ms.is_none()
            || session.updated_at_unix_ms.is_none()
            || session.execution != SessionExecutionState::Preparing
            || !session.needs_initial_realization_recovery()
            || !root_has_consumed_creation_intent(&session)
        {
            return Err(SessionCreationError::Internal(
                "repository returned an incomplete applied Session root".into(),
            ));
        }
        Ok(Self(session))
    }

    pub(super) fn from_recovery(session: PersistedSession) -> Result<Self, &'static str> {
        if session.revision == awaken_session_types::SessionRevision(0)
            || !root_has_consumed_creation_intent(&session)
        {
            return Err("recovery loaded an incomplete Session root");
        }
        Ok(Self(session))
    }

    pub(super) fn as_session(&self) -> &PersistedSession {
        &self.0
    }

    pub(super) fn into_session(self) -> PersistedSession {
        self.0
    }
}

/// Applied create receipts are revalidated as a complete durable root before
/// the caller may accept or realize them. Replays intentionally retain the
/// repository's current aggregate because it may already have advanced beyond
/// the initial creation state.
pub(crate) enum CompleteSessionCreateResult {
    Applied(DurableSessionRoot),
    Replayed(DurableSessionRoot),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionCreationCompletion {
    AwaitRealization,
    AcceptDurableRoot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionCreateReceiptKind {
    Applied,
    Replayed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionCreateEffectPlan {
    ContinueAppliedRealization,
    AcceptAppliedRoot,
    ReturnDurableReplay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionCreateControlFailurePlan {
    ReplayDurableRootAndWake,
    FailTerminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionCreateReplayPlan {
    ReturnDurableRoot,
    RejectTerminalOccupation,
}

const fn session_create_replay_plan(
    execution: SessionExecutionState,
    completion: SessionCreationCompletion,
) -> SessionCreateReplayPlan {
    if matches!(completion, SessionCreationCompletion::AwaitRealization)
        && matches!(execution, SessionExecutionState::ActivationFailed)
    {
        SessionCreateReplayPlan::RejectTerminalOccupation
    } else {
        SessionCreateReplayPlan::ReturnDurableRoot
    }
}

/// Reuse the contract-owned disposition instead of teaching create another
/// list of retryable control variants.
#[must_use]
const fn session_create_control_failure_plan(
    disposition: SessionRealizationControlDisposition,
) -> SessionCreateControlFailurePlan {
    match disposition {
        SessionRealizationControlDisposition::NotReady
        | SessionRealizationControlDisposition::Retryable => {
            SessionCreateControlFailurePlan::ReplayDurableRootAndWake
        }
        SessionRealizationControlDisposition::Terminal => {
            SessionCreateControlFailurePlan::FailTerminal
        }
    }
}

/// Close the only post-create effect gate over the repository's typed receipt.
/// A replay never enters realization, activation, or Work projection.
#[must_use]
const fn session_create_effect_plan(
    receipt: SessionCreateReceiptKind,
    completion: SessionCreationCompletion,
) -> SessionCreateEffectPlan {
    match (receipt, completion) {
        (SessionCreateReceiptKind::Applied, SessionCreationCompletion::AwaitRealization) => {
            SessionCreateEffectPlan::ContinueAppliedRealization
        }
        (SessionCreateReceiptKind::Applied, SessionCreationCompletion::AcceptDurableRoot) => {
            SessionCreateEffectPlan::AcceptAppliedRoot
        }
        (SessionCreateReceiptKind::Replayed, _) => SessionCreateEffectPlan::ReturnDurableReplay,
    }
}

#[cfg(kani)]
mod create_effect_gate_verification {
    use super::*;

    /// @obligation session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
    /// @obligation session_creation.atomic_complete_root_precedes_every_projection
    /// @obligation session_creation.exact_replay_conflict_and_tombstone_are_closed
    /// @obligation session_creation.repository_receipt_is_the_only_post_create_effect_gate
    #[kani::proof]
    fn only_an_applied_awaited_create_may_enter_post_create_effects() {
        // Causes: C1 repository receipt is Applied/Replayed; C2 caller awaits
        // realization or accepts the durable root. Effects: E1 continue physical
        // effects, E2 accept and wake recovery, E3 return replayed truth. The
        // closed 2x2 table proves E1 iff Applied+Await and no Replay can reach E1.
        //
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | R1 | Applied | Await | E1 |
        // | R2 | Applied | Accept | E2 |
        // | R3 | Replayed | Await | E3 |
        // | R4 | Replayed | Accept | E3 |
        let receipt = if kani::any::<bool>() {
            SessionCreateReceiptKind::Applied
        } else {
            SessionCreateReceiptKind::Replayed
        };
        let completion = if kani::any::<bool>() {
            SessionCreationCompletion::AwaitRealization
        } else {
            SessionCreationCompletion::AcceptDurableRoot
        };
        let plan = session_create_effect_plan(receipt, completion);
        assert_eq!(
            plan == SessionCreateEffectPlan::ContinueAppliedRealization,
            receipt == SessionCreateReceiptKind::Applied
                && completion == SessionCreationCompletion::AwaitRealization
        );
        if receipt == SessionCreateReceiptKind::Replayed {
            assert_eq!(plan, SessionCreateEffectPlan::ReturnDurableReplay);
        }
    }

    /// @obligation session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
    /// @obligation session_creation.repository_receipt_is_the_only_post_create_effect_gate
    #[kani::proof]
    fn nonterminal_control_failures_only_replay_the_durable_create_root() {
        // Causes: C1 contract disposition is NotReady/Retryable/Terminal.
        // Effects: E1 replay exact receipt and wake recovery; E2 preserve the
        // terminal failure path. The closed table proves no nonterminal control
        // disposition can reach cleanup and no terminal disposition is accepted.
        //
        // | Rule | Disposition | Effect |
        // |---|---|---|
        // | P1 | NotReady or Retryable | E1 |
        // | P2 | Terminal | E2 |
        let disposition = match kani::any::<u8>() % 3 {
            0 => SessionRealizationControlDisposition::NotReady,
            1 => SessionRealizationControlDisposition::Retryable,
            _ => SessionRealizationControlDisposition::Terminal,
        };
        let plan = session_create_control_failure_plan(disposition);
        assert_eq!(
            plan == SessionCreateControlFailurePlan::ReplayDurableRootAndWake,
            disposition != SessionRealizationControlDisposition::Terminal
        );
    }

    /// @obligation session_creation.exact_replay_conflict_and_tombstone_are_closed
    #[kani::proof]
    fn create_replay_conflict_and_terminal_occupation_are_exact_and_closed() {
        // Cause/effect table: C1 crosses every durable execution state; C2 the
        // caller awaits realization or accepts the already durable root; C3 is
        // each repository identity conflict. E1 only Await+ActivationFailed is
        // the historical terminal occupation; E2 every other exact replay
        // returns durable truth; E3 each conflict maps to one distinct error
        // and no branch can fall through to create effects.
        let execution = match kani::any::<u8>() % 7 {
            0 => SessionExecutionState::Preparing,
            1 => SessionExecutionState::Activating,
            2 => SessionExecutionState::ActivationFailed,
            3 => SessionExecutionState::Running,
            4 => SessionExecutionState::Rescheduling,
            5 => SessionExecutionState::Idle,
            _ => SessionExecutionState::Terminated,
        };
        let completion = if kani::any::<bool>() {
            SessionCreationCompletion::AwaitRealization
        } else {
            SessionCreationCompletion::AcceptDurableRoot
        };
        assert_eq!(
            session_create_replay_plan(execution, completion)
                == SessionCreateReplayPlan::RejectTerminalOccupation,
            execution == SessionExecutionState::ActivationFailed
                && completion == SessionCreationCompletion::AwaitRealization
        );

        let conflict = match kani::any::<u8>() % 3 {
            0 => awaken_session_contract::SessionRepositoryConflict::AlreadyExists,
            1 => awaken_session_contract::SessionRepositoryConflict::Tombstoned,
            _ => awaken_session_contract::SessionRepositoryConflict::IdempotencyMismatch,
        };
        let error = SessionCreationError::repository(
            awaken_session_contract::SessionRepositoryError::Conflict(conflict.clone()),
        );
        assert!(match conflict {
            awaken_session_contract::SessionRepositoryConflict::AlreadyExists => {
                matches!(error, SessionCreationError::Conflict)
            }
            awaken_session_contract::SessionRepositoryConflict::Tombstoned => {
                matches!(error, SessionCreationError::Tombstoned)
            }
            awaken_session_contract::SessionRepositoryConflict::IdempotencyMismatch => {
                matches!(error, SessionCreationError::IdempotencyMismatch)
            }
        });
    }
}

/// Classify the repository-owned durable result at both create-replay entry
/// points. Awaiting callers retain the historical terminal conflict, while an
/// accepting caller receives the same failed asynchronous-operation root rather
/// than starting or projecting a replacement.
pub(super) fn validated_create_replay(
    session: PersistedSession,
    completion: SessionCreationCompletion,
) -> Result<PersistedSession, SessionCreationError> {
    let session = DurableSessionRoot::from_recovery(session)
        .map_err(|message| SessionCreationError::Internal(message.into()))?
        .into_session();
    match session_create_replay_plan(session.execution, completion) {
        SessionCreateReplayPlan::ReturnDurableRoot => Ok(session),
        SessionCreateReplayPlan::RejectTerminalOccupation => Err(SessionCreationError::Tombstoned),
    }
}

/// The durable create receipt a protocol replays and creates under. The
/// command identity (Session id and request fingerprint) is the protocol's;
/// the receipt key spelling is persisted and owned here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCreateReceipt {
    /// A client create retried under an owner-scoped idempotency key.
    ClientRequest,
    /// The one Session a durable DeploymentRun owns.
    DeploymentRun,
    /// A product-authored profiled Session.
    Profiled,
}

impl SessionCreateReceipt {
    /// The receipt record for one Session id and its request fingerprint.
    #[must_use]
    pub fn record(self, session_id: &str, request_fingerprint: String) -> IdempotencyRecord {
        let key = match self {
            Self::ClientRequest => format!("managed:session-create:{session_id}"),
            Self::DeploymentRun => format!("managed:deployment-session:{session_id}"),
            Self::Profiled => format!("profiled:create:{session_id}"),
        };
        IdempotencyRecord {
            key,
            payload_hash: request_fingerprint,
        }
    }
}

/// Protocol-neutral command accepted by the one durable Session creation driver.
pub struct CreateSessionCommand {
    pub owner_scope: String,
    pub session_id: String,
    pub intent: SessionCreationIntent,
    pub title: Option<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
    pub tools: SessionToolConfiguration,
    pub budget: SessionBudgetState,
    /// Transient external participants configured before the root insert. The
    /// root adopts them on Applied/Replayed; they are never serialized here.
    pub repository_configurations: Vec<super::ConfiguredSessionRepository>,
    /// Optional caller-compiled request identity. Private product protocols use
    /// the same repository receipt as every later Session command instead of
    /// storing a replay fingerprint in mutable metadata. Ordinary callers keep
    /// the historical aggregate-payload identity by leaving this absent.
    pub idempotency: Option<IdempotencyRecord>,
    /// Optional complete create-time Event plan. It is compiled by the protocol
    /// boundary before this command and installed in the original Session root.
    pub initial_events: Option<awaken_session_contract::SessionInitialEventPlan>,
    /// Pre-insert presentation seed; it grants no execution/configuration authority.
    /// CompleteSessionRoot supplies canonical compiled tools/MCP before recording
    /// the immutable first value. This transient seed is never itself history.
    pub initial_agent_configuration:
        Option<Box<awaken_session_configuration_contract::SessionAgentConfiguration>>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionCreationError {
    #[error("Session identity already exists")]
    Conflict,
    #[error("Session identity is terminally occupied")]
    Tombstoned,
    #[error("Session idempotency key was reused with another payload")]
    IdempotencyMismatch,
    #[error(transparent)]
    Rejected(#[from] RunError),
    #[error("Session creation dependency is unavailable: {0}")]
    Unavailable(String),
    #[error("Session creation failed because durable state is invalid: {0}")]
    Internal(String),
    /// The Session aggregate refused the compiled root: the same permanent
    /// class as `Internal`, carrying its typed source.
    #[error("Session creation failed because durable state is invalid: {0}")]
    Invalid(crate::SessionStateViolation),
}

/// Creation reuses the mutation boundary's single classification of a state
/// violation, so a list-price outage stays retryable here too.
impl From<crate::SessionStateViolation> for SessionCreationError {
    fn from(violation: crate::SessionStateViolation) -> Self {
        Self::mutation(violation.into())
    }
}

impl SessionCreationError {
    pub(crate) fn repository(error: awaken_session_contract::SessionRepositoryError) -> Self {
        match error {
            awaken_session_contract::SessionRepositoryError::Conflict(
                awaken_session_contract::SessionRepositoryConflict::Tombstoned,
            ) => Self::Tombstoned,
            error => Self::mutation(error.into()),
        }
    }

    fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => {
                Self::Internal("newly created Session disappeared".into())
            }
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::IdempotencyMismatch,
            SessionMutationError::Unavailable(message)
            | SessionMutationError::NotApplied(message) => Self::Unavailable(message),
            SessionMutationError::Internal(message) => Self::Internal(message),
            SessionMutationError::Invalid(violation) => Self::Invalid(violation),
        }
    }

    fn realization(error: SessionRealizationDriveError) -> Self {
        Self::Rejected(error.into())
    }

    /// An outage or invalid durable state cannot prove a create receipt
    /// absent; every other failure proves this exact receipt was not adopted.
    fn leaves_adoption_unknown(&self) -> bool {
        matches!(
            self,
            Self::Unavailable(_) | Self::Internal(_) | Self::Invalid(_)
        )
    }
}

/// Join the realized DTO to the controlled activation commit boundary.
async fn commit_creation_activation(
    mutations: &crate::SessionMutations,
    owner: &str,
    session: PersistedSession,
) -> Result<crate::SnapshotCommit, SessionMutationError> {
    let candidate = crate::mutation::aggregate_from_snapshot(session)
        .map_err(crate::mutation::repository_failure)?;
    mutations
        .commit_session_snapshot_with_outcome(owner, candidate, "activate", Vec::new())
        .await
}

fn compile_creation_configuration(
    compiler: &dyn awaken_tool_contract::ToolConfigurationCompiler,
    mut intent: SessionCreationIntent,
    tools: SessionToolConfiguration,
) -> Result<(CompiledSessionCreation, SessionToolConfiguration), RunError> {
    intent.control.toolsets = crate::tool_compilation::compile_tool_settings(
        compiler,
        SessionToolConfiguration {
            toolsets: intent.control.toolsets,
            client_tools: tools.client_tools.clone(),
        },
    )?
    .toolsets;
    let compiled = intent
        .finalize()
        .map_err(|error| RunError::bad_request(error.to_string()))?;
    crate::tool_compilation::compile_tool_settings(compiler, tools).map(|tools| (compiled, tools))
}

/// Session creation: a Managed request or a published Agent profile is
/// resolved into one frozen baseline, its Repository participants are
/// configured, and the complete root is committed through the one durable
/// create protocol before realization and Work projection.
///
/// It holds the CAS kernel, the clock and lifecycle signal, the read, input,
/// credential, Repository participant and Work components, the realization
/// and terminal cleanup components a create drives or compensates through,
/// the executable-projection refresh and tool compiler, the Resource Registry,
/// the Environment, model and list-price catalogs, the process placement and
/// the Runtime roles a baseline resolution reads.
pub struct SessionCreationCommands {
    mutations: Arc<crate::SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    lifecycle: Arc<crate::SessionLifecycle>,
    queries: Arc<crate::SessionQueries>,
    inputs: Arc<crate::SessionInputs>,
    credentials: Arc<crate::SessionCredentials>,
    repository_participants: Arc<crate::SessionRepositoryParticipants>,
    work_assignments: Arc<crate::SessionWorkAssignments>,
    realizations: Arc<crate::SessionRealizations>,
    terminal_cleanups: Arc<crate::SessionTerminalCleanups>,
    executable_projections: crate::ExecutableProjections,
    tool_configuration_compiler: Arc<dyn awaken_tool_contract::ToolConfigurationCompiler>,
    resource_registry: Option<Arc<dyn awaken_resource_contract::ResourceRegistry>>,
    environment_projection: Arc<
        dyn awaken_executable_environment_contract::SessionEnvironmentProjectionSource<
                Snapshot = awaken_session_configuration_contract::EnvironmentSnapshot,
            >,
    >,
    model_publication_resolver:
        Option<Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>>,
    managed_list_prices: Option<Arc<dyn awaken_session_contract::ManagedListPriceProvider>>,
    execution_placement: crate::SessionExecutionPlacement,
    default_model: String,
    thread_reads: Arc<dyn awaken_session_execution_contract::ThreadReads>,
    placement: awaken_session_execution_contract::SessionPlacementRoles,
}

/// The collaborators one [`SessionCreationCommands`] is built from.
pub(crate) struct SessionCreationCollaborators {
    pub(crate) mutations: Arc<crate::SessionMutations>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) lifecycle: Arc<crate::SessionLifecycle>,
    pub(crate) queries: Arc<crate::SessionQueries>,
    pub(crate) inputs: Arc<crate::SessionInputs>,
    pub(crate) credentials: Arc<crate::SessionCredentials>,
    pub(crate) repository_participants: Arc<crate::SessionRepositoryParticipants>,
    pub(crate) work_assignments: Arc<crate::SessionWorkAssignments>,
    pub(crate) realizations: Arc<crate::SessionRealizations>,
    pub(crate) terminal_cleanups: Arc<crate::SessionTerminalCleanups>,
    pub(crate) executable_projections: crate::ExecutableProjections,
    pub(crate) tool_configuration_compiler:
        Arc<dyn awaken_tool_contract::ToolConfigurationCompiler>,
    pub(crate) resource_registry: Option<Arc<dyn awaken_resource_contract::ResourceRegistry>>,
    pub(crate) environments: crate::SessionEnvironments,
    pub(crate) model_publication_resolver:
        Option<Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>>,
    pub(crate) managed_list_prices:
        Option<Arc<dyn awaken_session_contract::ManagedListPriceProvider>>,
    pub(crate) execution_placement: crate::SessionExecutionPlacement,
    pub(crate) runtime: crate::SessionExecutor,
}

impl SessionCreationCommands {
    pub(crate) fn new(collaborators: SessionCreationCollaborators) -> Self {
        let SessionCreationCollaborators {
            mutations,
            clock,
            lifecycle,
            queries,
            inputs,
            credentials,
            repository_participants,
            work_assignments,
            realizations,
            terminal_cleanups,
            executable_projections,
            tool_configuration_compiler,
            resource_registry,
            environments,
            model_publication_resolver,
            managed_list_prices,
            execution_placement,
            runtime,
        } = collaborators;
        Self {
            mutations,
            clock,
            lifecycle,
            queries,
            inputs,
            credentials,
            repository_participants,
            work_assignments,
            realizations,
            terminal_cleanups,
            executable_projections,
            tool_configuration_compiler,
            resource_registry,
            environment_projection: environments.projection,
            model_publication_resolver,
            managed_list_prices,
            execution_placement,
            default_model: runtime.default_model.clone(),
            thread_reads: runtime.thread_reads.clone(),
            placement: runtime.placement.clone(),
        }
    }

    /// This process's realization identity, which a create receipt names.
    #[must_use]
    pub fn runtime_incarnation(&self) -> &str {
        self.realizations.runtime_incarnation()
    }
}

impl SessionCreationCommands {
    /// Create one Session through the only durable creation protocol.
    pub async fn create_session(
        &self,
        command: CreateSessionCommand,
    ) -> Result<PersistedSession, SessionCreationError> {
        Box::pin(
            self.create_session_with_completion(
                command,
                SessionCreationCompletion::AwaitRealization,
            ),
        )
        .await
    }

    /// Accept one Session as soon as its complete root is durable. Physical
    /// realization, activation, and disposable Work projection are resumed by
    /// the existing lifecycle supervisor from that same aggregate.
    pub async fn accept_session(
        &self,
        command: CreateSessionCommand,
    ) -> Result<PersistedSession, SessionCreationError> {
        Box::pin(
            self.create_session_with_completion(
                command,
                SessionCreationCompletion::AcceptDurableRoot,
            ),
        )
        .await
    }

    pub(super) async fn create_session_with_completion(
        &self,
        command: CreateSessionCommand,
        completion: SessionCreationCompletion,
    ) -> Result<PersistedSession, SessionCreationError> {
        let mutations = &self.mutations;
        let CreateSessionCommand {
            owner_scope,
            session_id,
            intent,
            title,
            metadata,
            tools,
            budget,
            repository_configurations,
            idempotency,
            initial_events,
            initial_agent_configuration,
        } = command;
        // Compile the complete intent before insert so invalid input can never
        // strand a durable Session waiting for a second authoring path.
        let (compiled, tools) = match compile_creation_configuration(
            self.tool_configuration_compiler.as_ref(),
            intent,
            tools,
        ) {
            Ok(compiled) => compiled,
            Err(error) => {
                let first = SessionCreationError::Rejected(error);
                self.repository_participants
                    .compensate(
                        &session_id,
                        &repository_configurations,
                        "Session Repository compensation remains pending after compile rejection",
                    )
                    .await;
                return Err(first);
            }
        };
        let layout = awaken_session_configuration_contract::SessionSandboxLayout {
            workspace_id: owner_scope.clone(),
            agent_id: compiled.baseline.agent_id.clone(),
            agent_revision: compiled.baseline.agent_revision,
            runtime_placement: compiled.baseline.runtime_placement,
            model_override: compiled.baseline.model_override.clone(),
            runtime: compiled.baseline.runtime.clone(),
            mounts: compiled.baseline.mounts.clone(),
            env: compiled.baseline.env.clone(),
            environment: compiled.baseline.environment.clone(),
            resources: compiled.initial_resources.sandbox_layout_bindings(),
        };
        if let Err(error) = self.validate_session_sandbox_layout(&session_id, &layout) {
            let first = SessionCreationError::Rejected(error);
            self.repository_participants.compensate(
                &session_id,
                &repository_configurations,
                "Session Repository compensation remains pending after Sandbox-layout rejection",
            )
            .await;
            return Err(first);
        }
        let compiled_root = match CompleteSessionRoot::compile(
            session_id.clone(),
            title,
            metadata,
            tools,
            budget,
            compiled,
            (initial_events, initial_agent_configuration),
        ) {
            Ok(root) => root,
            Err(first) => {
                self.repository_participants
                    .compensate(
                        &session_id,
                        &repository_configurations,
                        "Session Repository compensation remains pending after root compilation",
                    )
                    .await;
                return Err(first);
            }
        };
        // Request identity predates server-owned first-materialization facts,
        // just as it predates observe_creation's timestamps. Keep the one
        // canonical hasher; admission/replacement hashes retain the full fact.
        let idempotency = idempotency.unwrap_or_else(|| {
            SessionMutationPayload::default_create_idempotency(compiled_root.as_session())
        });
        let create_result = match mutations
            .create_session_root(&owner_scope, compiled_root, idempotency.clone(), Vec::new())
            .await
        {
            Ok(result) => result,
            Err(first) => {
                match mutations
                    .replay_session_create(
                        &owner_scope,
                        &awaken_session_types::SessionId::from(&session_id),
                        &idempotency,
                    )
                    .await
                {
                    Ok(Some(session)) => return validated_create_replay(session, completion),
                    Ok(None) => {
                        self.repository_participants.compensate(
                            &session_id,
                            &repository_configurations,
                            "Session Repository compensation remains pending after create failure",
                        )
                        .await;
                    }
                    Err(error) if error.leaves_adoption_unknown() => {
                        // An unavailable/corrupt replay query cannot prove the
                        // atomic root transaction absent. Preserve participants
                        // until exact durable truth can classify adoption.
                    }
                    Err(_) => {
                        // Conflict/tombstone/idempotency mismatch proves this
                        // exact create receipt was not adopted. The compensation
                        // helper still rereads the root and preserves any
                        // participant referenced by a concurrent winner.
                        self.repository_participants.compensate(
                            &session_id,
                            &repository_configurations,
                            "Session Repository compensation remains pending after deterministic create rejection",
                        )
                        .await;
                    }
                }
                return Err(first);
            }
        };
        let (receipt, mut persisted) = match create_result {
            CompleteSessionCreateResult::Applied(root) => {
                (SessionCreateReceiptKind::Applied, root.into_session())
            }
            CompleteSessionCreateResult::Replayed(root) => {
                (SessionCreateReceiptKind::Replayed, root.into_session())
            }
        };
        match session_create_effect_plan(receipt, completion) {
            SessionCreateEffectPlan::ReturnDurableReplay => {
                // The repository receipt and aggregate are the sole replay
                // truth. Returning here prevents duplicate installation,
                // activation, Work dispatch, or cleanup from an uncommitted
                // newly lowered candidate.
                return validated_create_replay(persisted, completion);
            }
            SessionCreateEffectPlan::AcceptAppliedRoot => {
                // The root contains the frozen baseline, initial Event plan,
                // and idempotency receipt. Waking the sole recovery driver is
                // only an optimization: startup scanning recovers the same
                // root after process loss without a Job row or second lifecycle.
                self.lifecycle.request_reconciliation();
                return Ok(persisted);
            }
            SessionCreateEffectPlan::ContinueAppliedRealization => {}
        }

        let realized = if persisted.requires_external_environment_owner() {
            self.realizations
                .install_dispatch_projection(&owner_scope, &persisted)
                .await
                .map(|()| persisted.clone())
        } else {
            self.realizations
                .realize_session_after_refresh(&awaken_session_types::SessionId::from(&session_id))
                .await
        };
        persisted = match realized {
            Ok(session) => session,
            Err(error) => {
                let recoverable_control = matches!(
                    &error,
                    SessionRealizationDriveError::Control(control)
                        if session_create_control_failure_plan(control.disposition())
                            == SessionCreateControlFailurePlan::ReplayDurableRootAndWake
                );
                // Cleanup authority comes from the current durable execution
                // state, never from an effect error or a control-error label.
                // The accepting replay exposes that state without converting an
                // ActivationFailed root into a transport-only tombstone.
                let current = mutations
                    .replay_accepted_session_create(
                        &owner_scope,
                        &awaken_session_types::SessionId::from(&session_id),
                        &idempotency,
                    )
                    .await;
                match current {
                    Ok(Some(session)) if session.execution.is_terminal() => {
                        let _ = self
                            .terminal_cleanups
                            .release_terminal_resources(&owner_scope, &session_id)
                            .await;
                    }
                    Ok(Some(session)) if recoverable_control => {
                        self.lifecycle.request_reconciliation();
                        return Ok(session);
                    }
                    Ok(Some(_)) => {
                        // The root still owns unfinished physical responsibility.
                        // Keep it intact and wake the sole lifecycle driver, but
                        // retain the original fail-closed effect/control result.
                        self.lifecycle.request_reconciliation();
                    }
                    Ok(None) => {}
                    Err(replay_error) => return Err(replay_error),
                }
                return Err(SessionCreationError::realization(error));
            }
        };

        persisted = match commit_creation_activation(&self.mutations, &owner_scope, persisted).await
        {
            Ok(commit) => commit.into_session(),
            Err(SessionMutationError::Conflict) => {
                // The realization is complete, so a concurrent root writer (or
                // a lost acknowledgement from this final CAS) cannot invalidate
                // the create receipt. Return the exact receipt's current root
                // instead of exposing a retry-unsafe failed POST.
                let Some(session) = mutations
                    .replay_session_create(
                        &owner_scope,
                        &awaken_session_types::SessionId::from(&session_id),
                        &idempotency,
                    )
                    .await?
                else {
                    return Err(SessionCreationError::Conflict);
                };
                validated_create_replay(session, completion)?
            }
            Err(error) => return Err(SessionCreationError::mutation(error)),
        };

        // The aggregate is already the authoritative dispatch intent. An
        // ambiguous create failure here could induce a duplicate client create;
        // the canonical reconciler therefore retries this disposable projection.
        if let Err(error) = self.work_assignments.dispatch(&persisted).await {
            tracing::warn!(
                session = %session_id,
                error = ?error,
                "Session WorkQueue dispatch remains pending after create"
            );
        }
        if persisted.needs_event_reconciliation() {
            self.lifecycle.request_reconciliation();
        }
        Ok(persisted)
    }
}

#[async_trait::async_trait]
impl crate::SessionCreation for SessionCreationCommands {
    async fn refresh_executable_projections(
        &self,
    ) -> Result<awaken_session_application_contract::ExecutableProjectionAdmission, String> {
        self.executable_projections.refresh().await
    }

    async fn create_requested_session(
        &self,
        request: request::SessionCreateRequest,
        protocol: &dyn request::SessionCreateProtocol,
    ) -> Result<PersistedSession, request::SessionCreateRequestError> {
        SessionCreationCommands::create_requested_session(self, request, protocol).await
    }

    async fn replay_session_create(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        idempotency: &awaken_session_contract::IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.mutations
            .replay_session_create(owner_scope, session_id, idempotency)
            .await
    }

    async fn replay_accepted_session_create(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        idempotency: &awaken_session_contract::IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionCreationError> {
        self.mutations
            .replay_accepted_session_create(owner_scope, session_id, idempotency)
            .await
    }

    fn runtime_incarnation(&self) -> &str {
        SessionCreationCommands::runtime_incarnation(self)
    }
}

#[async_trait::async_trait]
impl crate::DeploymentSessionEnvironment for SessionCreationCommands {
    async fn deployment_environment(
        &self,
        environment_id: &str,
    ) -> Result<
        Option<awaken_environment_contract::EnvItem>,
        awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
    > {
        self.environment_projection.get(environment_id).await
    }

    fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.queries.agent_unavailable(workspace_id, agent_id)
    }

    fn deployment_unavailable_delegate(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        self.queries.unavailable_delegate(workspace_id, agent_id)
    }
}
