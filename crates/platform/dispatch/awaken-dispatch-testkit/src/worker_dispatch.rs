//! The one test composition of the Worker dispatch HTTP service.
//!
//! Tests start from [`worker_dispatch_dependencies`] and replace only the
//! collaborators they exercise with struct-update syntax:
//!
//! ```ignore
//! WorkerDispatchService::new(RegisteredDispatchDependencies {
//!     session_work: work.clone(),
//!     ..worker_dispatch_dependencies(dispatch, directory, clock)
//! })
//! ```
//!
//! Every collaborator is present, as in production. Authority and committed
//! truth ports default to [`UnconfiguredCollaborators`], which refuses every
//! request; the post-commit projections (completion, terminal observer, live
//! stream) accept and drop; Session Work reports `NotRequired`, the answer the
//! Work queue gives a Thread outside any Session Environment.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use awaken_agent_contract::stream::sink::{Error as StreamSinkError, Sink as StreamSink};
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_dispatch_contract::{
    CompletionSink, DispatchSettlementError, DispatchSettlementObserver, RunDispatch,
};
use awaken_runtime_contract::runs::RunError;
use awaken_session_application_contract::{
    SessionWorkAcquisition, SessionWorkGrant, SessionWorkOwnership, SessionWorkerLeaseAuthority,
};
use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_contract as model;
use awaken_session_execution_contract as session;
use awaken_session_execution_contract::SessionRealizationControlFailure;
use awaken_session_types::SessionId;
use awaken_time::WallClock;
use awaken_worker_contract::LeastLoadedPolicy;
use awaken_worker_contract::{RunClaim, WorkerDirectory};
use awaken_worker_transport_security::{FixedWorkerLeasePolicy, HeaderWorkerAuthenticator};

use awaken_dispatch_http::{
    RegisteredDispatchDependencies, WORKER_DISPATCH_MAX_ATTEMPTS, WORKER_REGISTRY_TTL_MS,
    WorkerDispatchAuthority,
};

/// Every collaborator present; header authentication, the default lease
/// policy and the production registry TTL and retry budget.
#[must_use]
pub fn worker_dispatch_dependencies(
    dispatch: Arc<dyn WorkerDispatchAuthority>,
    directory: Arc<dyn WorkerDirectory>,
    clock: Arc<dyn WallClock>,
) -> RegisteredDispatchDependencies {
    let unconfigured = Arc::new(UnconfiguredCollaborators);
    RegisteredDispatchDependencies {
        execution_bindings: None,
        execution_stops: None,
        dispatch,
        checkpoint: unconfigured.clone(),
        directory,
        policy: Arc::new(LeastLoadedPolicy),
        realizations: unconfigured.clone(),
        terminal_cleanup: unconfigured.clone(),
        session_preparation: unconfigured.clone(),
        session_environment_bindings: unconfigured.clone(),
        coordination: unconfigured.clone(),
        session_work: unconfigured.clone(),
        authenticator: Arc::new(HeaderWorkerAuthenticator),
        recovery: unconfigured.clone(),
        completion: unconfigured.clone(),
        terminal_observer: unconfigured.clone(),
        stream_sink: unconfigured,
        credential_deliveries: None,
        clock,
        lease_policy: Arc::new(FixedWorkerLeasePolicy::default()),
        registry_ttl_ms: WORKER_REGISTRY_TTL_MS,
        max_attempts: WORKER_DISPATCH_MAX_ATTEMPTS,
    }
}

/// Committed truth in which the claimed Run is in `state`, as the Worker
/// committed it before settling. Partitioning is not modelled: the snapshot
/// answers for the requested logical Thread.
pub struct CommittedRunRecovery(pub RunState);

#[async_trait]
impl RunRecoverySource for CommittedRunRecovery {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        Ok(RunRecoverySnapshot {
            commit_timestamps: Default::default(),
            thread_id: thread_id.clone(),
            claimed_run_id: claimed_run_id.clone(),
            runs: vec![RunRecord {
                id: claimed_run_id.clone(),
                thread_id: thread_id.clone(),
                state: self.0.clone(),
            }],
            latest_run_id: Some(claimed_run_id.clone()),
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 1,
        })
    }
}

/// Fixture default for every port a test does not exercise.
pub struct UnconfiguredCollaborators;

const UNCONFIGURED: &str = "not configured by this test fixture";

fn refused() -> SessionRealizationControlFailure {
    SessionRealizationControlFailure::Unavailable(UNCONFIGURED.into())
}

fn run_refused() -> RunError {
    RunError::unavailable(UNCONFIGURED)
}

#[async_trait]
impl StreamCheckpointStore for UnconfiguredCollaborators {
    async fn get(&self, _run_id: &str) -> Observed<StreamCheckpoint> {
        Observed::Unknown(StreamCheckpointError::Storage(UNCONFIGURED.into()).into())
    }

    async fn put(&self, _checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        Err(StreamCheckpointError::Storage(UNCONFIGURED.into()))
    }

    async fn delete(&self, _run_id: &str) -> Result<(), StreamCheckpointError> {
        Err(StreamCheckpointError::Storage(UNCONFIGURED.into()))
    }
}

#[async_trait]
impl RunRecoverySource for UnconfiguredCollaborators {
    async fn recovery_snapshot(
        &self,
        _thread_id: &ThreadId,
        _claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        Err(RecoveryError::Rejected(UNCONFIGURED.into()))
    }
}

impl CompletionSink for UnconfiguredCollaborators {
    fn settled(&self, _run_id: &RunId, _state: &RunState) {}
}

#[async_trait]
impl DispatchSettlementObserver for UnconfiguredCollaborators {
    async fn before_settle(
        &self,
        _dispatch: &RunDispatch,
        _claim: &RunClaim,
        _committed_state: &RunState,
        _cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        Ok(())
    }
}

#[async_trait]
impl StreamSink for UnconfiguredCollaborators {
    async fn send(
        &self,
        _event: awaken_agent_contract::stream::event::Event,
    ) -> Result<(), StreamSinkError> {
        Ok(())
    }
}

#[async_trait]
impl SessionWorkerLeaseAuthority for UnconfiguredCollaborators {
    async fn acquire_session_work(
        &self,
        _session_id: &SessionId,
        _worker_owner: &str,
        _acquisition: SessionWorkAcquisition,
    ) -> Result<SessionWorkOwnership, RunError> {
        Ok(SessionWorkOwnership::NotRequired)
    }

    async fn release_session_work(&self, _lease: &SessionWorkGrant) -> Result<bool, RunError> {
        Err(run_refused())
    }

    async fn release_worker_session_work(&self, _worker_owner: &str) -> Result<usize, RunError> {
        Ok(0)
    }
}

#[async_trait]
impl session::SessionRealizationControl for UnconfiguredCollaborators {
    async fn begin_session_realization(
        &self,
        _command: session::BeginSessionRealization,
    ) -> Result<session::SessionRealizationDirective, SessionRealizationControlFailure> {
        Err(refused())
    }

    async fn renew_session_realization(
        &self,
        _command: session::RenewSessionRealization,
    ) -> Result<session::SessionRealizationLeaseReceipt, SessionRealizationControlFailure> {
        Err(refused())
    }

    async fn activate_session_realization(
        &self,
        _command: session::ActivateSessionRealization,
    ) -> Result<session::SessionRealizationDirective, SessionRealizationControlFailure> {
        Err(refused())
    }

    async fn acknowledge_session_realization(
        &self,
        _command: session::AcknowledgeSessionRealization,
    ) -> Result<session::SessionRealizationDirective, SessionRealizationControlFailure> {
        Err(refused())
    }

    async fn fail_session_realization(
        &self,
        _command: session::FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(refused())
    }
}

#[async_trait]
impl session::SessionPreparationControl for UnconfiguredCollaborators {
    async fn claim_next_session_preparation(
        &self,
        _target: session::SessionRealizationTarget,
    ) -> Result<Option<session::SessionPreparationAssignment>, SessionRealizationControlFailure>
    {
        Err(refused())
    }
}

#[async_trait]
impl session::SessionTerminalCleanupQueue for UnconfiguredCollaborators {
    async fn claim_next_terminal_cleanup(
        &self,
        _target: session::SessionRealizationTarget,
        _after_session_id: Option<&SessionId>,
    ) -> Result<Option<session::SessionTerminalCleanupAssignment>, SessionRealizationControlFailure>
    {
        Err(refused())
    }

    async fn terminal_cleanup_work(
        &self,
        _session_id: &SessionId,
        _lease: &SessionRealizationLease,
    ) -> Result<Option<session::SessionTerminalCleanupWork>, SessionRealizationControlFailure> {
        Err(refused())
    }
}

#[async_trait]
impl session::SessionTerminalPreparationControl for UnconfiguredCollaborators {
    async fn authorize_terminal_cleanup_effect(
        &self,
        _effect: &model::SessionTerminalCleanupEffect,
    ) -> Result<
        session::SessionTerminalCleanupPreparationAuthorization,
        SessionRealizationControlFailure,
    > {
        Err(refused())
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        _lease: &SessionRealizationLease,
        _preparation: model::SessionCleanupPreparation,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(refused())
    }
}

#[async_trait]
impl session::SessionTerminalDisposalControl for UnconfiguredCollaborators {
    async fn authorize_terminal_cleanup_disposal(
        &self,
        _effect: &model::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, SessionRealizationControlFailure> {
        Err(refused())
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        _lease: &SessionRealizationLease,
        _receipt: model::SessionCleanupDisposalReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(refused())
    }
}

#[async_trait]
impl session::SessionTerminalRepositoryPublicationControl for UnconfiguredCollaborators {
    async fn terminal_repository_publication_command(
        &self,
        _session_id: &SessionId,
        _lease: &SessionRealizationLease,
    ) -> Result<
        Option<session::SessionRepositoryPublicationProjection>,
        SessionRealizationControlFailure,
    > {
        Err(refused())
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        _session_id: &SessionId,
        _lease: &SessionRealizationLease,
        _receipt: model::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(refused())
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        _session_id: &SessionId,
        _lease: &SessionRealizationLease,
        _rejection: model::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure> {
        Err(refused())
    }
}

#[async_trait]
impl session::SessionEnvironmentBindingSink for UnconfiguredCollaborators {
    async fn authorize(
        &self,
        _intent: &model::SessionEnvironmentEffectIntent,
    ) -> Result<model::SessionEnvironmentEffectAuthorization, RunError> {
        Err(run_refused())
    }

    async fn persist(
        &self,
        _receipt: model::SessionEnvironmentReceipt,
    ) -> Result<model::SessionEnvironmentState, RunError> {
        Err(run_refused())
    }
}

#[async_trait]
impl session::SessionAgentCoordination for UnconfiguredCollaborators {
    async fn prepare_prepared_session_run_activity(
        &self,
        _session: &SessionId,
        _agent: &str,
        _run: &RunId,
        _mode: session::SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, RunError> {
        Err(run_refused())
    }

    async fn admit_prepared_session_run_activity(
        &self,
        _session: &SessionId,
        _run: &RunId,
        _mode: session::SessionRunActivityAdmissionMode,
        _preparation: session::PreparedSessionRunActivity,
    ) -> Result<session::SessionRunActivityAdmission, RunError> {
        Err(run_refused())
    }

    async fn admit_session_run_activity(
        &self,
        _session_id: &SessionId,
        _agent_id: &str,
        _run_id: &RunId,
        _mode: session::SessionRunActivityAdmissionMode,
    ) -> Result<session::SessionRunActivityAdmission, RunError> {
        Err(run_refused())
    }

    async fn admit_session_model_request(
        &self,
        _session_id: &SessionId,
        _thread_id: &ThreadId,
        _run_id: &RunId,
    ) -> Result<bool, RunError> {
        Err(run_refused())
    }

    async fn list_session_agents(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<session::SessionAgentRosterEntry>, RunError> {
        Err(run_refused())
    }

    async fn send_session_agent_message(
        &self,
        _command: session::SessionAgentMessageCommand,
    ) -> Result<session::SessionAgentMessageReceipt, RunError> {
        Err(run_refused())
    }

    async fn settle_session_agent_boundary(
        &self,
        _command: session::SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        Err(run_refused())
    }

    async fn finalize_session_agent_boundary(
        &self,
        _command: session::SessionAgentBoundaryCommand,
    ) -> Result<(), RunError> {
        Err(run_refused())
    }

    async fn interrupt_session_thread(
        &self,
        _session_id: &SessionId,
        _child_thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        Err(run_refused())
    }

    async fn reply_session_thread_tool(
        &self,
        _command: model::SessionThreadToolReplyCommand,
    ) -> Result<(), RunError> {
        Err(run_refused())
    }
}
