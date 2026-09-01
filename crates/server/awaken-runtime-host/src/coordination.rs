//! Runtime-host adapters for fixed Agent coordination and durable child settlement.
//!
//! Both adapters route through the Session application's existing authority. A
//! co-located Host holds only its weak application edge; a database-less Worker
//! uses the current dispatch claim and the existing authenticated
//! [`awaken_run_ingress::ClaimedSessionControl`] transport.

use std::sync::{Arc, Weak};

use awaken_ext_builtin_tools::{
    AgentCoordinator, AgentListRequest, AgentMessageReceipt, AgentMessageRequest,
    AgentMessageTarget, AgentRosterEntry,
};
use awaken_run_ingress::{
    DispatchSettlementError, DispatchSettlementObserver, RunClaim, RunDispatch,
};
use awaken_run_ingress_contract::ClaimedSessionControl;
use awaken_runtime_contract::llm::{
    ModelRequestAdmission, ModelRequestAdmissionRequest, ModelRequestGate,
};
use awaken_runtime_contract::tool::ToolError;
use awaken_session_contract::{
    SessionAgentBoundaryCommand, SessionAgentCoordination, SessionAgentMessageCommand,
    SessionAgentMessageReceipt, SessionAgentRosterEntry, SessionAgentTarget,
    SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
};

#[derive(Clone)]
pub(crate) enum CoordinationEndpoint {
    Local(Weak<dyn SessionAgentCoordination>),
    Remote {
        control: Arc<dyn ClaimedSessionControl>,
        slots: crate::session_slot::SessionRuntimeSlots,
    },
}

impl crate::SharedHost {
    /// Select the one Session coordination authority for every consumer. A live
    /// co-located application wins; an expired Weak falls through to the claimed
    /// remote control instead of becoming a dead local endpoint.
    pub(crate) fn coordination_endpoint(&self) -> Option<CoordinationEndpoint> {
        let local = self
            .agent_coordination
            .read()
            .expect("Agent coordination application lock poisoned")
            .clone()
            .filter(|application| application.strong_count() > 0);
        local.map(CoordinationEndpoint::Local).or_else(|| {
            self.session_control
                .as_ref()
                .map(|control| CoordinationEndpoint::Remote {
                    control: control.clone(),
                    slots: self.session_slots.clone(),
                })
        })
    }

    pub(crate) fn dispatch_settlement_observer(
        &self,
    ) -> Option<Arc<dyn awaken_run_ingress::DispatchSettlementObserver>> {
        self.coordination_endpoint().map(|endpoint| {
            Arc::new(HostDispatchSettlementObserver::new(endpoint))
                as Arc<dyn awaken_run_ingress::DispatchSettlementObserver>
        })
    }
}

impl CoordinationEndpoint {
    fn current_claim(
        slots: &crate::session_slot::SessionRuntimeSlots,
        session_id: &str,
        source_run_id: &str,
    ) -> Result<RunClaim, ToolError> {
        let claim = slots
            .read(session_id, |slot| slot.dispatch_claim.clone())
            .flatten()
            .ok_or_else(|| {
                ToolError::Execution(
                    "Session coordination requires a current dispatch claim".to_string(),
                )
            })?;
        if claim.run_id.0 != source_run_id {
            return Err(ToolError::Execution(
                "Session coordination source Run does not own the current dispatch claim"
                    .to_string(),
            ));
        }
        Ok(claim)
    }

    async fn list(
        &self,
        session_id: &str,
        source_run_id: &str,
    ) -> Result<Vec<SessionAgentRosterEntry>, ToolError> {
        match self {
            Self::Local(application) => application
                .upgrade()
                .ok_or_else(|| {
                    ToolError::Execution("Session coordination application is unavailable".into())
                })?
                .list_session_agents(session_id)
                .await
                .map_err(tool_error),
            Self::Remote { control, slots } => {
                let claim = Self::current_claim(slots, session_id, source_run_id)?;
                control
                    .list_session_agents(&claim, session_id)
                    .await
                    .map_err(|error| ToolError::Execution(error.to_string()))
            }
        }
    }

    async fn admit_model_request(
        &self,
        session_id: &str,
        request: &ModelRequestAdmissionRequest,
    ) -> Result<bool, String> {
        match self {
            Self::Local(application) => application
                .upgrade()
                .ok_or_else(|| "Session coordination application is unavailable".to_string())?
                .admit_session_model_request(session_id, &request.thread_id, &request.run_id)
                .await
                .map_err(|error| error.to_string()),
            Self::Remote { control, slots } => {
                let claim = Self::current_claim(slots, session_id, &request.run_id.0)
                    .map_err(|error| error.to_string())?;
                control
                    .admit_session_model_request(
                        &claim,
                        session_id,
                        &request.thread_id,
                        &request.run_id,
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
        }
    }

    async fn admit_run_activity(
        &self,
        claim: &RunClaim,
        session_id: &str,
        agent_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, DispatchSettlementError> {
        match self {
            Self::Local(application) => application
                .upgrade()
                .ok_or_else(|| {
                    DispatchSettlementError(
                        "Session coordination application is unavailable".into(),
                    )
                })?
                .admit_session_run_activity(session_id, agent_id, run_id, mode)
                .await
                .map_err(|error| DispatchSettlementError(error.to_string())),
            Self::Remote { control, .. } => control
                .admit_session_run_activity(claim, session_id, agent_id, run_id, mode)
                .await
                .map_err(|error| DispatchSettlementError(error.to_string())),
        }
    }

    async fn send(
        &self,
        source_run_id: &str,
        command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, ToolError> {
        match self {
            Self::Local(application) => application
                .upgrade()
                .ok_or_else(|| {
                    ToolError::Execution("Session coordination application is unavailable".into())
                })?
                .send_session_agent_message(command)
                .await
                .map_err(tool_error),
            Self::Remote { control, slots } => {
                let claim = Self::current_claim(slots, &command.session_id, source_run_id)?;
                control
                    .send_session_agent_message(&claim, command)
                    .await
                    .map_err(|error| ToolError::Execution(error.to_string()))
            }
        }
    }

    async fn settle(
        &self,
        claim: &RunClaim,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), DispatchSettlementError> {
        match self {
            Self::Local(application) => application
                .upgrade()
                .ok_or_else(|| {
                    DispatchSettlementError(
                        "Session coordination application is unavailable".into(),
                    )
                })?
                .settle_session_agent_boundary(command)
                .await
                .map_err(|error| DispatchSettlementError(error.to_string())),
            Self::Remote { control, .. } => control
                .settle_session_agent_boundary(claim, command)
                .await
                .map_err(|error| DispatchSettlementError(error.to_string())),
        }
    }
}

/// Runtime-facing adapter over the already-selected local/remote Session
/// authority. It owns no budget value: every call crosses the endpoint and
/// reconciles committed cumulative usage at the application root.
pub(crate) struct HostModelRequestGate {
    endpoint: CoordinationEndpoint,
    session_id: String,
}

impl HostModelRequestGate {
    pub(crate) fn new(endpoint: CoordinationEndpoint, session_id: impl Into<String>) -> Self {
        Self {
            endpoint,
            session_id: session_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl ModelRequestGate for HostModelRequestGate {
    async fn admit_model_request(
        &self,
        request: ModelRequestAdmissionRequest,
    ) -> Result<ModelRequestAdmission, String> {
        self.endpoint
            .admit_model_request(&self.session_id, &request)
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

fn tool_error(error: awaken_session_contract::RunError) -> ToolError {
    match error.kind {
        awaken_session_contract::RunErrorKind::BadRequest => {
            ToolError::InvalidArguments(error.message)
        }
        awaken_session_contract::RunErrorKind::Internal
        | awaken_session_contract::RunErrorKind::Unavailable => ToolError::Execution(error.message),
    }
}

#[async_trait::async_trait]
impl AgentCoordinator for HostAgentCoordinator {
    async fn list_agents(
        &self,
        request: AgentListRequest,
    ) -> Result<Vec<AgentRosterEntry>, ToolError> {
        if request.source_thread_id.trim().is_empty() || request.source_run_id.trim().is_empty() {
            return Err(ToolError::Execution(
                "Agent roster lookup requires Runtime-owned source coordinates".into(),
            ));
        }
        self.endpoint
            .list(&request.source_thread_id, &request.source_run_id)
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
                    session_id,
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
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for HostDispatchSettlementObserver {
    fn owns_session_run_reservation_fence(&self) -> bool {
        matches!(&self.endpoint, CoordinationEndpoint::Remote { .. })
    }

    async fn admit_session_run_activity(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, DispatchSettlementError> {
        let Some(session_id) = dispatch.session_thread_id.as_ref() else {
            return Ok(SessionRunActivityAdmission::Rejected);
        };
        if dispatch.run_id() != &claim.run_id
            || dispatch.thread_id() != session_id
            || dispatch.session_activity_epoch.is_some()
        {
            return Ok(SessionRunActivityAdmission::Rejected);
        }
        self.endpoint
            .admit_run_activity(
                claim,
                &session_id.0,
                &dispatch.activation.snapshot.root_agent_id.0,
                dispatch.run_id(),
                mode,
            )
            .await
    }

    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        committed_state: &awaken_agent_contract::agent::run::RunState,
        cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        // `session_thread_id` is shared routing vocabulary: Session roots,
        // synchronous delegation, and Advisor invocations may all carry it.
        // The admitted activity epoch classifies either an asynchronous child
        // or its deterministic self-affine primary report continuation; Runs
        // without that epoch remain outside this observer.
        let Some(activity_epoch) = dispatch.session_activity_epoch else {
            return Ok(());
        };
        let session_id = dispatch.session_thread_id.as_ref().ok_or_else(|| {
            DispatchSettlementError(
                "coordinated child settlement has no parent Session affinity".into(),
            )
        })?;
        if activity_epoch == 0
            || dispatch.run_id() != &claim.run_id
            || !matches!(
                committed_state,
                awaken_agent_contract::agent::run::RunState::Awaiting
                    | awaken_agent_contract::agent::run::RunState::Ended(_)
            )
        {
            return Err(DispatchSettlementError(
                "coordinated child settlement coordinates are inconsistent".into(),
            ));
        }

        self.endpoint
            .settle(
                claim,
                SessionAgentBoundaryCommand {
                    session_id: session_id.0.clone(),
                    source_thread_id: dispatch.thread_id().clone(),
                    source_run_id: dispatch.run_id().clone(),
                    source_agent_id: dispatch.activation.snapshot.root_agent_id.0.clone(),
                    session_activity_epoch: activity_epoch,
                    cancellation_requested,
                },
            )
            .await
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingSessionAgentCoordination {
    boundaries: std::sync::Mutex<Vec<SessionAgentBoundaryCommand>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl SessionAgentCoordination for RecordingSessionAgentCoordination {
    async fn list_session_agents(
        &self,
        _session_id: &str,
    ) -> Result<Vec<SessionAgentRosterEntry>, awaken_session_contract::RunError> {
        Ok(Vec::new())
    }

    async fn send_session_agent_message(
        &self,
        _command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, awaken_session_contract::RunError> {
        Err(awaken_session_contract::RunError::bad_request("unused"))
    }

    async fn settle_session_agent_boundary(
        &self,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.boundaries.lock().unwrap().push(command);
        Ok(())
    }

    async fn interrupt_session_thread(
        &self,
        _session_id: &str,
        _child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_contract::RunError> {
        Ok(())
    }

    async fn reply_session_thread_tool(
        &self,
        _command: awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), awaken_session_contract::RunError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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

    struct RemoteControl;

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for RemoteControl {
        async fn begin_session_realization(
            &self,
            _command: awaken_session_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
        }

        async fn activate_session_realization(
            &self,
            _command: awaken_session_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
        }

        async fn acknowledge_session_realization(
            &self,
            _command: awaken_session_contract::AcknowledgeSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_contract::FailSessionRealization,
        ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
            Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
        }
    }

    #[async_trait::async_trait]
    impl ClaimedSessionControl for RemoteControl {
        async fn resume_frozen(
            &self,
            _claim: &RunClaim,
            _session_id: &str,
        ) -> Result<
            Option<awaken_session_contract::SessionRealizationDirective>,
            awaken_run_ingress_contract::ClaimedSessionControlError,
        > {
            Ok(None)
        }
    }

    #[test]
    fn dead_local_coordination_falls_through_to_the_remote_authority() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 a local Weak coordination edge exists but its
        // application is dropped; C2 a claimed remote control exists. Effects:
        // E1 every tool/availability/settlement consumer selects Remote; E2 no
        // consumer captures the dead Local endpoint. Decision table:
        // R1(live local,C2)->Local; R2(C1+C2)->E1+E2; !local+!remote is covered by
        // Managed build's fail-closed authority test.
        let local = Arc::new(RecordingSessionAgentCoordination::default());
        let local_port: Arc<dyn SessionAgentCoordination> = local.clone();
        let dead = Arc::downgrade(&local_port);
        drop(local_port);
        drop(local);
        let host = crate::SharedHost::new(Arc::new(EndpointModel), "stub")
            .with_session_control(Arc::new(RemoteControl));
        *host
            .agent_coordination
            .write()
            .expect("test coordination lock") = Some(dead);

        assert!(
            matches!(
                host.coordination_endpoint(),
                Some(CoordinationEndpoint::Remote { .. })
            ),
            "R2/E1-E2"
        );
        assert!(host.dispatch_settlement_observer().is_some(), "R2/E1");
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
            Arc::downgrade(&application_port),
        ));
        let dispatch = RunDispatch::new(RunActivation::new(
            run.clone(),
            child.clone(),
            ExecutableAgentSnapshot::builder("researcher")
                .model(ModelBinding::new("test", "model", "native"))
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
