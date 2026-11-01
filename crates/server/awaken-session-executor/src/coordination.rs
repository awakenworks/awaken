// @formal evidence delegation.independent_child_scheduling_lease
//! Runtime-host adapters for fixed Agent coordination and durable child settlement.
//!
//! Both adapters route through the Session application's existing authority. A
//! co-located Host reaches it through the executor's one
//! [`SessionApplicationBinding`]; a database-less Worker
//! uses the current dispatch claim and the existing authenticated
//! [`awaken_session_execution_contract::ClaimedSessionCoordinationControl`] transport.

use std::sync::{Arc, Weak};

use awaken_dispatch_contract::{
    DispatchSettlementError, DispatchSettlementObserver, RunClaim, RunDispatch,
};
use awaken_ext_builtin_tools::{
    AgentCoordinator, AgentListRequest, AgentMessageReceipt, AgentMessageRequest,
    AgentMessageTarget, AgentRosterEntry,
};
use awaken_runtime_contract::llm::{
    Error as LlmError, ModelRequestAdmission, ModelRequestAdmissionRequest, ModelRequestGate,
};
use awaken_session_execution_contract::{
    ClaimedSessionCoordinationControl, SessionAgentMessageCommand, SessionAgentMessageReceipt,
    SessionAgentRosterEntry, SessionAgentTarget, SessionApplicationBinding,
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
};
use awaken_tool_contract::tool::ToolError;

#[cfg(test)]
use awaken_session_execution_contract::{SessionAgentBoundaryCommand, SessionAgentCoordination};

pub(crate) type ClaimedCoordination = dyn ClaimedSessionCoordinationControl<RunClaim>;

fn model_admission_error(
    disposition: awaken_session_execution_contract::SessionRealizationControlDisposition,
    message: String,
) -> LlmError {
    use awaken_session_execution_contract::SessionRealizationControlDisposition as D;
    match disposition {
        D::Terminal => LlmError::Binding(message),
        D::Retryable => LlmError::Provider(message),
        D::NotReady => LlmError::Overloaded {
            message,
            retry_after: None,
        },
    }
}

#[derive(Clone)]
pub(crate) enum CoordinationEndpoint {
    Local(SessionApplicationBinding),
    Remote {
        control: Arc<ClaimedCoordination>,
        installed_sessions: Weak<crate::installed_session::InstalledSessions>,
    },
}

impl crate::RunCapabilities {
    /// Select the immutable authority chosen by service construction. An
    /// expired local application remains a failed local endpoint; it never
    /// falls through to a different remote authority.
    pub(crate) fn coordination_endpoint(&self) -> Option<CoordinationEndpoint> {
        use awaken_session_execution_contract::SessionApplicationMode as M;
        // An unbound binding selects no endpoint, exactly as Standalone does.
        match self.session_application.mode().ok()? {
            M::Local => Some(CoordinationEndpoint::Local(
                self.session_application.clone(),
            )),
            M::Standalone => None,
            M::ClaimedWorker => self
                .worker
                .session_coordination_control
                .as_ref()
                .map(|control| CoordinationEndpoint::Remote {
                    control: control.clone(),
                    installed_sessions: Arc::downgrade(&self.installed_sessions),
                }),
        }
    }

    /// Select the endpoint once and share its immutable observer across the
    /// two existing admission/settlement roles on each Worker.
    pub(crate) fn dispatch_settlement_observer(
        &self,
    ) -> Option<Arc<HostDispatchSettlementObserver>> {
        self.coordination_endpoint()
            .map(|endpoint| Arc::new(HostDispatchSettlementObserver::new(endpoint)))
    }
}

impl CoordinationEndpoint {
    /// The Session's current claim, read from the installed Sessions the
    /// endpoint upgraded (`None` once they are released, which fails closed).
    fn current_claim(
        installed_sessions: Option<Arc<crate::installed_session::InstalledSessions>>,
        session_id: &str,
        source_run_id: &str,
    ) -> Result<(Arc<crate::installed_session::InstalledSessions>, RunClaim), ToolError> {
        let installed_sessions = installed_sessions.ok_or_else(|| {
            ToolError::DefinitiveFailure("Session coordination owner is unavailable".into())
        })?;
        let claim = installed_sessions
            .read_run(session_id, |run| run.dispatch_claim.clone())
            .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))?
            .flatten()
            .ok_or_else(|| {
                ToolError::DefinitiveFailure(
                    "Session coordination requires a current dispatch claim".to_string(),
                )
            })?;
        if claim.run_id.0 != source_run_id {
            return Err(ToolError::DefinitiveFailure(
                "Session coordination source Run does not own the current dispatch claim"
                    .to_string(),
            ));
        }
        Ok((installed_sessions, claim))
    }

    async fn list(
        &self,
        session_id: &awaken_session_types::SessionId,
        source_run_id: &str,
    ) -> Result<Vec<SessionAgentRosterEntry>, ToolError> {
        match self {
            Self::Local(application) => application
                .agent_coordination()
                .map_err(|_| {
                    ToolError::DefinitiveFailure(
                        "Session coordination application is unavailable".into(),
                    )
                })?
                .list_session_agents(session_id)
                .await
                .map_err(tool_error),
            Self::Remote {
                control,
                installed_sessions,
            } => {
                // Retain the original owner until this control future finishes.
                let (_owner, claim) = Self::current_claim(
                    installed_sessions.upgrade(),
                    session_id.as_str(),
                    source_run_id,
                )?;
                control
                    .list_session_agents(&claim, session_id.as_str())
                    .await
                    .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))
            }
        }
    }

    async fn admit_model_request(
        &self,
        session_id: &awaken_session_types::SessionId,
        request: &ModelRequestAdmissionRequest,
        claimed: Option<&RunClaim>,
    ) -> Result<bool, LlmError> {
        if claimed.is_some_and(|claim| claim.run_id.0 != request.run_id.0) {
            return Err(LlmError::Binding(
                "Session model request Run differs from its dispatch claim".into(),
            ));
        }
        match self {
            Self::Local(application) => application
                .agent_coordination()
                .map_err(|_| {
                    LlmError::Provider("Session coordination application is unavailable".into())
                })?
                .admit_session_model_request(session_id, &request.thread_id, &request.run_id)
                .await
                .map_err(|error| {
                    model_admission_error(
                        awaken_session_execution_contract::effect_failure_disposition(&error),
                        error.to_string(),
                    )
                }),
            Self::Remote { control, .. } => {
                let claim = claimed.ok_or_else(|| {
                    LlmError::Binding(
                        "Remote Session model request requires its exact dispatch claim".into(),
                    )
                })?;
                control
                    .admit_session_model_request(
                        claim,
                        session_id.as_str(),
                        &request.thread_id,
                        &request.run_id,
                    )
                    .await
                    .map_err(|error| model_admission_error(error.disposition(), error.to_string()))
            }
        }
    }

    async fn send(
        &self,
        source_run_id: &str,
        command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, ToolError> {
        match self {
            Self::Local(application) => {
                let application = application.agent_coordination().map_err(|_| {
                    ToolError::DefinitiveFailure(
                        "Session coordination application is unavailable".into(),
                    )
                })?;
                // Admission can realize another Agent's full Session projection
                // before child execution starts. Break that nested poll stack at
                // the local application port, using the same cancellation owner
                // as child execution; dropping the tool must not detach admission.
                crate::agent_runner::AbortOnDropTask::spawn(async move {
                    application.send_session_agent_message(command).await
                })
                .join()
                .await
                .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))?
                .map_err(tool_error)
            }
            Self::Remote {
                control,
                installed_sessions,
            } => {
                // Retain the original owner until this control future finishes.
                let (_owner, claim) = Self::current_claim(
                    installed_sessions.upgrade(),
                    command.session_id.as_str(),
                    source_run_id,
                )?;
                control
                    .send_session_agent_message(&claim, command)
                    .await
                    .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))
            }
        }
    }
}

/// Runtime-facing adapter over the already-selected local/remote Session
/// authority. It owns no budget value: every call crosses the endpoint and
/// reconciles committed cumulative usage at the application root.
pub(crate) struct HostModelRequestGate {
    endpoint: CoordinationEndpoint,
    session_id: String,
    claim: Option<RunClaim>,
}

impl HostModelRequestGate {
    pub(crate) fn new(endpoint: CoordinationEndpoint, session_id: impl Into<String>) -> Self {
        Self {
            endpoint,
            session_id: session_id.into(),
            claim: None,
        }
    }

    fn claimed(endpoint: CoordinationEndpoint, session_id: String, claim: RunClaim) -> Self {
        Self {
            endpoint,
            session_id,
            claim: Some(claim),
        }
    }
}

/// Bind the existing Session gate once for each exact dispatch claim. The
/// durable Worker invokes this factory in its canonical attempt-context seam;
/// no Session-wide mutable claim cache is consulted for model admission.
pub(crate) fn claimed_model_request_gate_factory(
    endpoint: CoordinationEndpoint,
    session_id: impl Into<String>,
) -> awaken_dispatch_execution::ClaimedModelRequestGateFactory {
    let session_id = session_id.into();
    Arc::new(move |claim| {
        let gate: Arc<dyn ModelRequestGate> = Arc::new(HostModelRequestGate::claimed(
            endpoint.clone(),
            session_id.clone(),
            claim.clone(),
        ));
        gate
    })
}

#[async_trait::async_trait]
impl ModelRequestGate for HostModelRequestGate {
    fn process_bound(&self) -> bool {
        true
    }
    async fn admit_model_request(
        &self,
        request: ModelRequestAdmissionRequest,
    ) -> Result<ModelRequestAdmission, LlmError> {
        self.endpoint
            .admit_model_request(
                &awaken_session_types::SessionId::from(&self.session_id),
                &request,
                self.claim.as_ref(),
            )
            .await
            .map(|admitted| {
                if admitted {
                    ModelRequestAdmission::Admit
                } else {
                    ModelRequestAdmission::Pause(
                        awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached,
                    )
                }
            })
    }
}

pub(crate) struct HostAgentCoordinator {
    endpoint: CoordinationEndpoint,
}

impl HostAgentCoordinator {
    pub(crate) fn new(endpoint: CoordinationEndpoint) -> Self {
        Self { endpoint }
    }
}

fn tool_error(error: awaken_session_types::RunError) -> ToolError {
    match error.kind {
        awaken_session_types::RunErrorKind::BadRequest => {
            ToolError::InvalidArguments(error.message)
        }
        awaken_session_types::RunErrorKind::Internal
        | awaken_session_types::RunErrorKind::Unavailable
        | awaken_session_types::RunErrorKind::Conflict => {
            ToolError::DefinitiveFailure(error.message)
        }
    }
}

#[async_trait::async_trait]
impl AgentCoordinator for HostAgentCoordinator {
    async fn list_agents(
        &self,
        request: AgentListRequest,
    ) -> Result<Vec<AgentRosterEntry>, ToolError> {
        if request.source_thread_id.trim().is_empty() || request.source_run_id.trim().is_empty() {
            return Err(ToolError::DefinitiveFailure(
                "Agent roster lookup requires Runtime-owned source coordinates".into(),
            ));
        }
        self.endpoint
            .list(
                &awaken_session_types::SessionId::from(request.source_thread_id),
                &request.source_run_id,
            )
            .await
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| AgentRosterEntry {
                        agent_id: entry.agent_id,
                        name: entry.name,
                        description: entry.description,
                    })
                    .collect()
            })
    }

    async fn send_message(
        &self,
        request: AgentMessageRequest,
    ) -> Result<AgentMessageReceipt, ToolError> {
        let session_id = request.source_thread_id.clone();
        let source_run_id = request.source_run_id.clone();
        let target = match request.target {
            AgentMessageTarget::Spawn { agent_id } => SessionAgentTarget::Spawn { agent_id },
            AgentMessageTarget::ExistingThread { session_thread_id } => {
                SessionAgentTarget::ExistingThread {
                    thread_id: awaken_agent_contract::agent::thread::Id(session_thread_id),
                }
            }
        };
        self.endpoint
            .send(
                &source_run_id,
                SessionAgentMessageCommand {
                    session_id: awaken_session_types::SessionId::from(session_id),
                    source_thread_id: awaken_agent_contract::agent::thread::Id(
                        request.source_thread_id,
                    ),
                    source_run_id: awaken_agent_contract::agent::run::Id(request.source_run_id),
                    source_call_id: request.source_call_id,
                    operation_id: request.operation_id,
                    target,
                    message: request.message,
                },
            )
            .await
            .map(|receipt| AgentMessageReceipt {
                session_thread_id: receipt.thread_id.0,
                accepted: true,
            })
    }
}

/// The dispatch worker's only coordinated-activity settlement hook. It derives
/// child reports from committed transcript truth and invokes the same local or
/// remote Session authority for both a child boundary and the deterministic
/// self-affine report continuation before its queue row can settle.
pub(crate) struct HostDispatchSettlementObserver {
    endpoint: CoordinationEndpoint,
}

impl HostDispatchSettlementObserver {
    pub(crate) fn new(endpoint: CoordinationEndpoint) -> Self {
        Self { endpoint }
    }

    fn local_observer(
        application: &SessionApplicationBinding,
    ) -> Result<awaken_session_run_delivery::SessionDispatchObserver, DispatchSettlementError> {
        application
            .agent_coordination()
            .map(awaken_session_run_delivery::SessionDispatchObserver::new)
            .map_err(|_| {
                DispatchSettlementError("Session coordination application is unavailable".into())
            })
    }
}

#[async_trait::async_trait]
impl awaken_dispatch_execution::SessionRunAdmissionObserver for HostDispatchSettlementObserver {
    async fn prepare_session_run_activity(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, DispatchSettlementError> {
        match &self.endpoint {
            CoordinationEndpoint::Local(application) => awaken_dispatch_execution::SessionRunAdmissionObserver::prepare_session_run_activity(
                &Self::local_observer(application)?, dispatch, claim, mode,
            ).await,
            // The authenticated Coordinator prepares outside its own guard;
            // a remote Worker can never supply a trusted root revision.
            CoordinationEndpoint::Remote { .. } => Ok(None),
        }
    }

    async fn admit_session_run_activity(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        mode: SessionRunActivityAdmissionMode,
        preparation: Option<awaken_session_execution_contract::PreparedSessionRunActivity>,
    ) -> Result<SessionRunActivityAdmission, DispatchSettlementError> {
        let Some(session) =
            awaken_session_run_delivery::SessionDispatchObserver::activity_session(dispatch, claim)
        else {
            return Ok(SessionRunActivityAdmission::Rejected);
        };
        match &self.endpoint {
            CoordinationEndpoint::Local(application) => {
                awaken_dispatch_execution::SessionRunAdmissionObserver::admit_session_run_activity(
                    &Self::local_observer(application)?,
                    dispatch,
                    claim,
                    mode,
                    preparation,
                )
                .await
            }
            CoordinationEndpoint::Remote { control, .. } => control
                .admit_session_run_activity(
                    claim,
                    &session.0,
                    &dispatch.activation.snapshot.root_agent_id.0,
                    dispatch.run_id(),
                    mode,
                )
                .await
                .map_err(|error| DispatchSettlementError(error.to_string())),
        }
    }
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for HostDispatchSettlementObserver {
    fn owns_dispatch_fence(&self) -> bool {
        matches!(&self.endpoint, CoordinationEndpoint::Remote { .. })
    }

    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        state: &awaken_agent_contract::agent::run::RunState,
        cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        let Some(command) = awaken_session_run_delivery::SessionDispatchObserver::boundary_command(
            dispatch,
            claim,
            state,
            cancellation_requested,
        )?
        else {
            return Ok(());
        };
        match &self.endpoint {
            CoordinationEndpoint::Local(application) => {
                Self::local_observer(application)?
                    .settle_boundary(command)
                    .await
            }
            CoordinationEndpoint::Remote { control, .. } => control
                .settle_session_agent_boundary(claim, command)
                .await
                .map_err(|error| DispatchSettlementError(error.to_string())),
        }
    }

    async fn after_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        state: &awaken_agent_contract::agent::run::RunState,
        cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        let Some(command) = awaken_session_run_delivery::SessionDispatchObserver::boundary_command(
            dispatch,
            claim,
            state,
            cancellation_requested,
        )?
        else {
            return Ok(());
        };
        match &self.endpoint {
            CoordinationEndpoint::Local(application) => {
                Self::local_observer(application)?
                    .finalize_boundary(command)
                    .await
            }
            // Only the Coordinator's successful /dispatch/settle CAS can
            // finalize remotely. Never re-authorize a retired claim over HTTP.
            CoordinationEndpoint::Remote { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingSessionAgentCoordination {
    boundaries: std::sync::Mutex<Vec<SessionAgentBoundaryCommand>>,
    message_gate: Option<Arc<AdmissionGate>>,
    model_error: std::sync::Mutex<Option<awaken_session_types::RunError>>,
}

#[cfg(test)]
#[derive(Default)]
struct AdmissionGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    dropped: tokio::sync::Notify,
}

#[cfg(test)]
struct AdmissionDropReceipt(Arc<AdmissionGate>);

#[cfg(test)]
impl Drop for AdmissionDropReceipt {
    fn drop(&mut self) {
        self.0.dropped.notify_one();
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl SessionAgentCoordination for RecordingSessionAgentCoordination {
    // Fixture rule P1: the recorder observes child boundaries without admitting root activity.
    // Prepared preparation refuses without changing any recorded effect.
    async fn prepare_prepared_session_run_activity(
        &self,
        _session: &awaken_session_types::SessionId,
        _agent: &str,
        _run: &awaken_agent_contract::agent::run::Id,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::unavailable(
            "the recorder observes child boundaries without admitting root activity",
        ))
    }

    // Fixture rule P1: the recorder observes child boundaries without admitting root activity.
    // Prepared admission refuses without changing any recorded effect.
    async fn admit_prepared_session_run_activity(
        &self,
        _session: &awaken_session_types::SessionId,
        _run: &awaken_agent_contract::agent::run::Id,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
        _preparation: awaken_session_execution_contract::PreparedSessionRunActivity,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivityAdmission,
        awaken_session_types::RunError,
    > {
        Err(awaken_session_types::RunError::unavailable(
            "the recorder observes child boundaries without admitting root activity",
        ))
    }

    async fn admit_session_run_activity(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _agent_id: &str,
        _run_id: &awaken_agent_contract::agent::run::Id,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivityAdmission,
        awaken_session_types::RunError,
    > {
        Err(awaken_session_types::RunError::bad_request(
            "recording coordination fixture does not admit Session activity",
        ))
    }

    async fn admit_session_model_request(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _thread_id: &awaken_agent_contract::agent::thread::Id,
        _run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, awaken_session_types::RunError> {
        Err(self.model_error.lock().unwrap().take().unwrap_or_else(|| {
            awaken_session_types::RunError::bad_request(
                "recording coordination fixture does not admit model requests",
            )
        }))
    }

    async fn list_session_agents(
        &self,
        _session_id: &awaken_session_types::SessionId,
    ) -> Result<Vec<SessionAgentRosterEntry>, awaken_session_types::RunError> {
        Ok(Vec::new())
    }

    async fn send_session_agent_message(
        &self,
        _command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, awaken_session_types::RunError> {
        if let Some(gate) = &self.message_gate {
            let _receipt = AdmissionDropReceipt(gate.clone());
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        Err(awaken_session_types::RunError::bad_request("unused"))
    }

    async fn settle_session_agent_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        self.boundaries.lock().unwrap().push(command);
        Ok(())
    }

    async fn finalize_session_agent_boundary(
        &self,
        _command: SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        // This fixture has no retained Session Event aggregate. Its only
        // reachable root continuation is a child-report Run, for which the
        // production application also classifies post-Dispatch finalization as
        // an inert non-Event effect.
        Ok(())
    }

    async fn interrupt_session_thread(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_types::RunError> {
        Ok(())
    }

    async fn reply_session_thread_tool(
        &self,
        _command: awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::RemoteControl;
    use super::*;
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

    struct EndpointModel;

    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for EndpointModel {
        async fn infer(
            &self,
            _request: awaken_runtime_contract::llm::ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse>
        {
            unreachable!("endpoint selection does not execute inference")
        }
    }

    #[tokio::test]
    async fn local_admission_task_propagates_errors_and_never_detaches_on_drop() {
        // Causes: C1 the real local endpoint enters application admission;
        // C2 application returns an error / the owning tool future is dropped.
        // Rules A1: release -> original error, completed future dropped;
        // A2: drop -> blocked application future dropped without release.
        // Notify receipts establish entry and destruction; timeouts bound hangs,
        // never stand in for proof that the cancellation boundary was reached.
        // The Coordinator delegation HTTP test covers successful full admission
        // and projection on the default stack, before child execution starts.
        for cancel in [false, true] {
            let gate = Arc::new(AdmissionGate::default());
            let application: Arc<dyn SessionAgentCoordination> =
                Arc::new(RecordingSessionAgentCoordination {
                    message_gate: Some(gate.clone()),
                    ..Default::default()
                });
            let endpoint = CoordinationEndpoint::Local(
                crate::test_application_bindings::coordination_binding(application.clone()),
            );
            let command = SessionAgentMessageCommand {
                session_id: "session".into(),
                source_thread_id: ThreadId("root".into()),
                source_run_id: RunId("run".into()),
                source_call_id: "call".into(),
                operation_id: "operation".into(),
                target: SessionAgentTarget::Spawn {
                    agent_id: "child".into(),
                },
                message: "work".into(),
            };
            let mut send = Box::pin(endpoint.send("run", command));
            tokio::select! {
                biased;
                result = &mut send => panic!("admission completed before release: {result:?}"),
                () = gate.entered.notified() => {},
                () = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("admission did not enter"),
            }
            if cancel {
                drop(send);
            } else {
                gate.release.notify_one();
                assert!(send.await.unwrap_err().to_string().contains("unused"));
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), gate.dropped.notified())
                .await
                .expect("the owning future must destroy the application future");
        }
    }

    fn assert_one_observer_roles(observer: &Arc<HostDispatchSettlementObserver>) {
        let settlement: Arc<dyn DispatchSettlementObserver> = observer.clone();
        let admission: Arc<dyn awaken_dispatch_execution::SessionRunAdmissionObserver> =
            observer.clone();
        assert_eq!(
            Arc::as_ptr(&settlement) as *const (),
            Arc::as_ptr(&admission) as *const (),
            "O1 one observer allocation for two unchanged roles"
        );
    }

    #[tokio::test]
    async fn commit_boundary_binds_late_coordination_without_retaining_run_assembly() {
        // Causes: C1 the Coordinator has no local pool; C2 its commit factory
        // predates application binding; the resolver follows complete binding; C3 the bound application stays live or
        // drops after Worker construction; C4 the composing executor drops;
        // C5 an exact exhausted child claim carries parent affinity/epoch;
        // C6 the actual Coordinator resolver remains live across Host drop,
        // settlement and committed-truth assertions.
        // Effects: E1 no RunAssembly is retained; E2 late binding delivers the
        // exact child boundary and settles Done; E3 a dead owner refuses
        // settlement without fabricating completion or undoing committed Ended.
        // Rules B1=C1+C2+C4+C5+C6+live(C3)=>E1+E2;
        // B2=C1+C2+C4+C5+C6+dead(C3)=>E1+E3. The real queue, commit, factory,
        // Runtime terminalization and existing coordination fixture own this
        // observation; no shadow claim model or physical executor is involved.
        use awaken_dispatch::{
            DispatchClaims as _, DispatchControlReads as _, DispatchEnqueue as _,
            DispatchReads as _,
        };
        use awaken_dispatch_execution::session_execution::CommitBoundaryWorkers as _;

        for owner_live in [true, false] {
            let now = awaken_time::system_clock().now().as_epoch_ms();
            let clock = Arc::new(awaken_lease_testkit::ManualClock::new(now));
            let queue = Arc::new(
                awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_store(
                    awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
                    None,
                ),
            );
            let mut dependencies =
                crate::SessionExecutorBuilder::ephemeral(Arc::new(EndpointModel), "stub")
                    .await
                    .with_ephemeral_dispatch(queue.clone());
            dependencies.execution.ingress = crate::RunIngress::Durable;
            dependencies.execution.local = crate::LocalExecution::RemoteWorkersOnly;
            let host = dependencies.build().expect("Coordinator components");
            let assembly = Arc::downgrade(&host.run_assembly);
            let boundaries = crate::host::boundary_run::commit_boundary_workers(&host.run_assembly);
            let parent = ThreadId("boundary-parent".into());
            let child = ThreadId("boundary-child".into());
            let run = RunId("boundary-child-run".into());
            let opened = boundaries
                .open_commit(&parent)
                .await
                .expect("original commit");
            let application = Arc::new(RecordingSessionAgentCoordination::default());
            let port: Arc<dyn SessionAgentCoordination> = application.clone();
            crate::test_application_bindings::select_coordination(&host, port.clone())
                .expect("late application binding");
            let resolver = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
                .expect("complete applications precede resolver construction");
            queue
                .enqueue(
                    RunDispatch::new(RunActivation::new(
                        run.clone(),
                        child.clone(),
                        ExecutableAgentSnapshot::builder("researcher")
                            .model(ModelBinding::new("test", "model", "native"))
                            .fingerprint("researcher-v1")
                            .build(),
                        Vec::new(),
                    ))
                    .for_session(parent.clone())
                    .with_session_activity_epoch(17),
                )
                .await
                .expect("original dispatch");
            let first = queue
                .claim_run(&run, "old-worker", 1_000, &Default::default())
                .await
                .unwrap()
                .unwrap();
            clock.set(first.lease.expires_ms() + 1);
            let second = queue
                .claim_run(&run, "old-worker", 1_000, &Default::default())
                .await
                .unwrap()
                .unwrap();
            clock.set(second.lease.expires_ms() + 1);
            let claimed = queue
                .claim_retry_exhausted("boundary-worker", 1_000, 1)
                .await
                .unwrap()
                .expect("exact exhausted claim");
            let worker = opened
                .boundary_worker(&claimed, None)
                .await
                .expect("environment-free boundary");
            drop(port);
            let application = if owner_live {
                Some(application)
            } else {
                drop(application);
                None
            };
            drop(host);
            assert!(assembly.upgrade().is_none(), "B1-B2/E1");
            let result = worker.terminalize_retry_exhausted(&claimed).await;
            if let Some(application) = application {
                assert!(result.is_ok(), "B1/E2: {result:?}");
                {
                    let boundaries = application.boundaries.lock().unwrap();
                    assert_eq!(boundaries.len(), 1, "B1 late binding was observed");
                    assert_eq!(boundaries[0].session_id.as_str(), parent.0, "B1 parent");
                    assert_eq!(boundaries[0].source_thread_id, child, "B1 child");
                    assert_eq!(boundaries[0].source_run_id, run, "B1 Run");
                    assert_eq!(boundaries[0].session_activity_epoch, 17, "B1 epoch");
                }
                assert!(queue.list_dispatches().await.unwrap().is_empty(), "B1 Done");
            } else {
                assert!(result.is_err(), "B2 dead owner must refuse settlement");
                assert_eq!(
                    queue.list_dispatches().await.unwrap().len(),
                    1,
                    "B2 pending"
                );
                assert!(
                    queue
                        .completion_events_after(0, 10)
                        .await
                        .unwrap()
                        .is_empty(),
                    "B2 no fabricated completion"
                );
            }
            assert_eq!(
                opened.committed().run_state(&run),
                Some(RunState::Ended(EndCause::Indeterminate)),
                "B1-B2 preserve original committed terminal truth",
            );
            drop(resolver);
        }
    }

    #[tokio::test]
    async fn expired_local_coordination_refuses_without_selecting_remote_authority() {
        // R1 local selection/live owner => Local for both observer roles.
        // R2 same immutable selection/expired owner/remote transport present =>
        // failed Local for every consumer; no authority switch or rebind.
        let local = Arc::new(RecordingSessionAgentCoordination::default());
        let local_port: Arc<dyn SessionAgentCoordination> = local.clone();
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(EndpointModel), "stub")
            .await
            .with_session_control(Arc::new(RemoteControl::default())))
        .build()
        .expect("validated Host dependencies");
        let shared = host.session_application_binding().clone();
        assert_eq!(
            shared.agent_coordination().err(),
            Some(awaken_session_execution_contract::SessionApplicationBindingError::Unbound),
            "B1"
        );
        crate::test_application_bindings::select_coordination(&host, local_port.clone())
            .expect("test coordination port binds once");
        assert!(shared.agent_coordination().is_ok(), "B2");
        // O1a live Local -> the sole factory selects Local once, then both
        // existing roles project the same allocation. A retained observer's
        // binding cannot keep the application alive after its final owner drops.
        let observer = host
            .run_assembly
            .capabilities
            .dispatch_settlement_observer()
            .expect("B2/O1a");
        assert!(
            matches!(&observer.endpoint, CoordinationEndpoint::Local(_)),
            "O1a"
        );
        assert_one_observer_roles(&observer);
        drop(observer);
        drop(local_port);
        drop(local);
        assert_eq!(
            shared.agent_coordination().err(),
            Some(
                awaken_session_execution_contract::SessionApplicationBindingError::RequiredApplicationUnavailable(
                    "agent coordination"
                )
            ),
            "B3"
        );

        assert!(
            matches!(
                host.run_assembly.capabilities.coordination_endpoint(),
                Some(CoordinationEndpoint::Local(_))
            ),
            "R2/E1-E2"
        );
        let endpoint = host
            .run_assembly
            .capabilities
            .coordination_endpoint()
            .unwrap();
        let error = endpoint
            .list(
                &awaken_session_types::SessionId::from("expired"),
                "source-run",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ToolError::DefinitiveFailure(message) if message == "Session coordination application is unavailable"),
            "dead local authority refuses before remote claim lookup or RPC"
        );
        // Both existing observer roles keep the same failed Local endpoint.
        let observer = host
            .run_assembly
            .capabilities
            .dispatch_settlement_observer()
            .expect("R2/E1");
        assert!(
            matches!(&observer.endpoint, CoordinationEndpoint::Local(_)),
            "R2/E1"
        );
        assert_one_observer_roles(&observer);
    }

    #[tokio::test]
    async fn claimed_model_gate_uses_each_run_claim_despite_a_shared_installed_session() {
        // Causes: C1 parent and child Runs overlap in one Session; C2 the
        // installed Session holds the other Run's claim; C3 a request names a Run
        // other than the gate's bound claim; C4 an unclaimed remote context.
        // Effects: E1 parent/child model requests carry their own exact claim;
        // E2 C3/C4 fail before the remote control is entered. Decision rules:
        // R1=C1+C2+matching Run=>E1, R2=C3=>E2, R3=C4=>E2. The Coordinator's
        // existing claim guard separately rejects a stale owner/epoch.
        let parent = RunClaim {
            run_id: RunId("parent".into()),
            owner: "worker".into(),
            epoch: 1,
        };
        let child = RunClaim {
            run_id: RunId("child".into()),
            owner: "worker".into(),
            epoch: 2,
        };
        let installed_sessions = Arc::new(crate::installed_session::InstalledSessions::default());
        let control = Arc::new(RemoteControl::default());
        let endpoint = CoordinationEndpoint::Remote {
            control: control.clone(),
            installed_sessions: Arc::downgrade(&installed_sessions),
        };
        let factory = claimed_model_request_gate_factory(endpoint.clone(), "session");
        installed_sessions.install_run("session", |run| run.dispatch_claim = Some(parent.clone()));
        assert_eq!(
            factory(&child)
                .admit_model_request(ModelRequestAdmissionRequest {
                    run_id: child.run_id.clone(),
                    thread_id: ThreadId("child-thread".into()),
                })
                .await
                .unwrap(),
            ModelRequestAdmission::Admit,
            "R1 child ignores parent installed Session"
        );
        installed_sessions.install_run("session", |run| run.dispatch_claim = Some(child.clone()));
        assert_eq!(
            factory(&parent)
                .admit_model_request(ModelRequestAdmissionRequest {
                    run_id: parent.run_id.clone(),
                    thread_id: ThreadId("parent-thread".into()),
                })
                .await
                .unwrap(),
            ModelRequestAdmission::Admit,
            "R1 parent ignores child installed Session"
        );
        assert!(
            factory(&child)
                .admit_model_request(ModelRequestAdmissionRequest {
                    run_id: parent.run_id.clone(),
                    thread_id: ThreadId("wrong".into()),
                })
                .await
                .is_err(),
            "R2 mismatched Run is fenced locally"
        );
        assert!(
            HostModelRequestGate::new(endpoint, "session")
                .admit_model_request(ModelRequestAdmissionRequest {
                    run_id: parent.run_id.clone(),
                    thread_id: ThreadId("unclaimed".into()),
                })
                .await
                .is_err(),
            "R3 remote execution cannot borrow an installed Session claim"
        );
        assert_eq!(
            *control.calls.lock().unwrap(),
            vec![("model", child), ("model", parent)],
            "R1-R3 exact remote calls"
        );
    }

    #[tokio::test]
    async fn settlement_adapter_uses_only_trusted_affinity_and_committed_assistant_text() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 dispatch has exact parent/child/run/epoch; C2
        // claim Run matches; C3 the trusted claimed dispatch carries/omits a
        // cancellation request; C4 its frozen snapshot carries the execution
        // Agent identity. Effects: E1 one Session boundary is emitted with exact
        // durable coordinates, cancellation provenance, and C4 identity; the
        // Session application, not this Worker adapter, reads committed
        // state/report truth. E2 a wrong claim is rejected before the
        // application; C5 an ordinary
        // session-affined child has no activity epoch, so E3 settlement is not
        // intercepted; C6 a self-affine primary report carries the transferred
        // epoch and exact claim, so E4 it reaches the same Session boundary port
        // without fabricating another child.
        //
        // | Rule | Exact | Cancel requested | Role | Effect |
        // |---|---|---|---|---|
        // | O1 | yes | no | child | E1(false) |
        // | O2 | yes | yes | child | E1(true) |
        // | O3 | no | any | child | E2 |
        // | O4 | any | any | uncoordinated | E3 |
        // | O5 | yes | no | primary report | E4 |
        //
        // The adapter owns no receipt store.
        let parent = ThreadId("primary".into());
        let child = ThreadId("child".into());
        let run = RunId("child-run".into());
        let application = Arc::new(RecordingSessionAgentCoordination::default());
        let application_port: Arc<dyn SessionAgentCoordination> = application.clone();
        let observer = HostDispatchSettlementObserver::new(CoordinationEndpoint::Local(
            crate::test_application_bindings::coordination_binding(application_port.clone()),
        ));
        let dispatch = RunDispatch::new(RunActivation::new(
            run.clone(),
            child.clone(),
            ExecutableAgentSnapshot::builder("researcher")
                .model(ModelBinding::parse("test", "model", "native").expect("valid binding"))
                .fingerprint("researcher-v1")
                .build(),
            Vec::new(),
        ))
        .for_session(parent)
        .with_session_activity_epoch(17);
        let claim = RunClaim {
            run_id: run.clone(),
            owner: "worker".into(),
            epoch: 3,
        };
        observer
            .before_settle(
                &dispatch,
                &claim,
                &RunState::Ended(EndCause::NaturalEnd),
                false,
            )
            .await
            .expect("O1");
        {
            let boundaries = application.boundaries.lock().unwrap();
            assert_eq!(boundaries.len(), 1, "O1/E1");
            assert_eq!(boundaries[0].source_thread_id, child, "O1/E1");
            assert_eq!(boundaries[0].source_run_id, run, "O1/E1");
            assert_eq!(boundaries[0].source_agent_id, "researcher", "O1/E1");
            assert_eq!(boundaries[0].session_activity_epoch, 17, "O1/E1");
            assert!(!boundaries[0].cancellation_requested, "O1/E1");
        }

        observer
            .before_settle(
                &dispatch,
                &claim,
                &RunState::Ended(EndCause::NaturalEnd),
                true,
            )
            .await
            .expect("O2");
        assert!(
            application.boundaries.lock().unwrap()[1].cancellation_requested,
            "O2/E1"
        );

        let wrong = RunClaim {
            run_id: RunId("other".into()),
            owner: "worker".into(),
            epoch: 3,
        };
        assert!(
            observer
                .before_settle(
                    &dispatch,
                    &wrong,
                    &RunState::Ended(EndCause::NaturalEnd),
                    false,
                )
                .await
                .is_err(),
            "O3/E2"
        );
        assert_eq!(application.boundaries.lock().unwrap().len(), 2, "O3/E2");

        let ordinary_child =
            RunDispatch::new(dispatch.activation.clone()).for_session(ThreadId("primary".into()));
        observer
            .before_settle(
                &ordinary_child,
                &wrong,
                &RunState::Ended(EndCause::NaturalEnd),
                false,
            )
            .await
            .expect("O4 ordinary synchronous/Advisor child is outside this observer");
        let self_affine_root = RunDispatch::new(dispatch.activation.clone())
            .for_session(dispatch.thread_id().clone())
            .with_session_activity_epoch(99);
        observer
            .before_settle(
                &self_affine_root,
                &claim,
                &RunState::Ended(EndCause::NaturalEnd),
                false,
            )
            .await
            .expect("O5 self-affine report transfers to the Session boundary");
        let boundaries = application.boundaries.lock().unwrap();
        assert_eq!(boundaries.len(), 3, "O4-O5/E3-E4");
        assert_eq!(boundaries[2].source_thread_id, child, "O5/E4 self affinity");
        assert_eq!(boundaries[2].session_activity_epoch, 99, "O5/E4");
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod lifetime_tests;

#[cfg(test)]
pub(crate) fn test_claimed_control() -> Arc<
    impl awaken_session_execution_contract::ClaimedSessionRealizationControl
    + awaken_session_execution_contract::ClaimedSessionCoordinationControl<RunClaim>,
> {
    Arc::new(test_support::RemoteControl::default())
}
