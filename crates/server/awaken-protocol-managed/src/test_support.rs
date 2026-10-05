// @formal surface exclude=test_only
// @formal owner crates/server/awaken-protocol-managed/src/lib.rs
// @formal reason: Static owner: Managed protocol fixtures and scenario helpers. Dynamic path: the module
// @formal reason: is reachable only through the exact cfg(any(test, feature = test-support)) declaration
// @formal reason: and never enters the default product graph. Evidence:
// @formal reason: crates/server/awaken-protocol-managed/src/lib.rs.
//! Explicit test-only startup for the split Environment owners.

#[path = "test_support/managed_event_observer.rs"]
mod managed_event_observer;
pub use managed_event_observer::{
    DurableFact, wait_for_delivered_lifecycle_facts, wait_for_session_events,
};
#[path = "test_support/committed_audit.rs"]
mod committed_audit;
pub use committed_audit::{
    AwaitTargets, StateLog, audited_lifecycle_records, fixture_state_log, fixture_thread_audit,
    fixture_usage_command, lifecycle_audit_pausing_on, lifecycle_audit_records,
    remember_ticket_await_target, tool_call_target,
};

use awaken_session_execution_contract::SessionRuntime;

use awaken_runtime_contract::tool_batch::ToolBatch;

use crate::types::tunnel::{Tunnel, TunnelCertificate, TunnelToken};

use crate::{ManagedTunnelApplication, ManagedTunnelApplicationError, ManagedTunnelScope};

/// Protocol fixtures exercise application ordering, not OS suspend behavior.
/// Keep one explicit advancing test clock; unsupported production Runtimes
/// still fail closed instead of receiving a default timing capability.
pub fn observe_realization_request() -> Result<
    awaken_runtime_contract::authority_lease::AuthorityProofStarted,
    awaken_session_error_contract::RunError,
> {
    static ORIGIN: std::sync::OnceLock<awaken_time::mono::Instant> = std::sync::OnceLock::new();
    let origin = *ORIGIN.get_or_init(awaken_time::mono::now);
    let now = awaken_time::mono::now();
    Ok(
        awaken_runtime_contract::authority_lease::AuthorityProofStarted::from_clock_sample(
            now,
            now.duration_since(origin),
        ),
    )
}

/// Empty hosted Tunnel port for composition tests.
///
/// Open Awaken deliberately leaves the Cloud-owned Tunnel port absent. Tests
/// that qualify the hosted composition seam inject this adapter so the real
/// Control router is mounted without copying Cloud state or business behavior
/// into the open repository.
#[derive(Debug, Default)]
pub struct EmptyManagedTunnelApplication;

#[async_trait::async_trait]
impl ManagedTunnelApplication for EmptyManagedTunnelApplication {
    async fn create_tunnel(
        &self,
        _scope: ManagedTunnelScope,
        _display_name: Option<String>,
    ) -> Result<Tunnel, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn retrieve_tunnel(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
    ) -> Result<Tunnel, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn list_tunnels(
        &self,
        _scope: ManagedTunnelScope,
        _include_archived: bool,
    ) -> Result<Vec<Tunnel>, ManagedTunnelApplicationError> {
        Ok(Vec::new())
    }

    async fn archive_tunnel(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
    ) -> Result<Tunnel, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn reveal_token(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
    ) -> Result<TunnelToken, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn rotate_token(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _reason: Option<String>,
    ) -> Result<TunnelToken, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn create_certificate(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _ca_certificate_pem: String,
    ) -> Result<TunnelCertificate, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn retrieve_certificate(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _certificate_id: &str,
    ) -> Result<TunnelCertificate, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn list_certificates(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _include_archived: bool,
    ) -> Result<Vec<TunnelCertificate>, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn archive_certificate(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _certificate_id: &str,
    ) -> Result<TunnelCertificate, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }
}

/// Validate the complete Session projection accepted by protocol test doubles
/// and return the legacy preparation view only when the selected installation
/// mode performs preparation. Keeping this rule here prevents each fake from
/// recreating a partial projection or silently accepting empty coordinates.
pub fn complete_session_projection_init(
    thread: &str,
    projection: &awaken_session_realization_contract::FrozenSessionProjection,
    mode: &awaken_session_realization_contract::SessionProjectionInstallMode,
) -> Result<
    Option<awaken_session_configuration_contract::SessionInit>,
    awaken_session_error_contract::RunError,
> {
    if thread.is_empty()
        || projection.workspace_id.is_empty()
        || projection.baseline.agent_id.is_empty()
    {
        return Err(awaken_session_error_contract::RunError::bad_request(
            "test Session projection must carry complete frozen coordinates",
        ));
    }
    Ok(mode.prepares_session().then(|| projection.session_init()))
}

/// Complete the preparation half of a protocol test Runtime through the same
/// closed authorization join as production. Keeping this in the shared test
/// adapter prevents each router/state fixture from recreating the retired
/// one-stage cleanup path or silently ignoring the effect echoed by Control.
pub fn complete_terminal_cleanup_preparation(
    effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    authorization: &awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
) -> Result<
    awaken_session_state_contract::SessionCleanupPreparation,
    awaken_session_error_contract::RunError,
> {
    authorization
        .verify_for(effect)
        .map_err(|error| awaken_session_error_contract::RunError::internal(error.to_string()))?;
    let provider_prepared_effect_fence = effect
        .sandbox_effect_fence()
        .map_err(|error| awaken_session_error_contract::RunError::internal(error.to_string()))?;
    awaken_session_state_contract::SessionCleanupPreparation::try_new(
        effect,
        provider_prepared_effect_fence,
        Vec::new(),
    )
    .map_err(|error| awaken_session_error_contract::RunError::internal(error.to_string()))
}

/// Complete the physical half of a protocol test Runtime from the one typed
/// aggregate command. The receipt constructor remains the canonical identity
/// owner; this helper owns no cleanup state or compatibility behavior.
#[must_use]
pub fn complete_terminal_cleanup_disposal(
    effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
) -> awaken_session_state_contract::SessionCleanupDisposalReceipt {
    awaken_session_state_contract::SessionCleanupDisposalReceipt::new(&effect.command)
}

/// This crate's own tests: the Managed Environment surfaces over the testkit's
/// volatile stores (integration tests use the testkit's function directly).
#[cfg(test)]
pub(crate) fn environment_components() -> (
    std::sync::Arc<crate::EnvironmentAuthoringState>,
    std::sync::Arc<awaken_environment_execution_application::EnvironmentExecutionApplication>,
) {
    let fixture = awaken_protocol_managed_testkit::environment_fixture(awaken_time::system_clock());
    (
        std::sync::Arc::new(crate::EnvironmentAuthoringState::new(
            fixture.application,
            fixture.sandbox_policies,
        )),
        fixture.execution,
    )
}

/// Shared integration-test Runtime for the canonical asynchronous coordination
/// path. It exposes one real child Thread and appends one ordinary child Run per
/// accepted primary Run; no `DelegatedRun` hint or child-named store participates.
#[derive(Clone)]
pub struct CoordinatedRuntimeFake {
    completed_user_dispatches: std::sync::Arc<
        std::sync::Mutex<Vec<awaken_session_execution_contract::SessionAgentBoundaryCommand>>,
    >,
    runs: std::sync::Arc<std::sync::atomic::AtomicU64>,
    reserved_user_runs: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, awaken_session_execution_contract::AdmitSessionRun>,
        >,
    >,
    user_run_states: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, awaken_agent_contract::agent::run::RunState>,
        >,
    >,
    user_run_outcomes: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, awaken_session_application_contract::StepOutcome>,
        >,
    >,
    root_messages:
        std::sync::Arc<std::sync::Mutex<Vec<awaken_agent_contract::agent::message::Message>>>,
    root_message_cursors: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    child_messages:
        std::sync::Arc<std::sync::Mutex<Vec<awaken_agent_contract::agent::message::Message>>>,
    child_message_cursors: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    coordination_operation_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    lifecycle: std::sync::Arc<std::sync::Mutex<Vec<awaken_agent_contract::RunLifecycleEvent>>>,
    interrupts: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    rejected_interrupt: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    disposition_by_thread: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<(String, String), awaken_agent_contract::ThreadDisposition>,
        >,
    >,
    disposition_commit_cursors:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<(String, String), u64>>>,
    archive_commits: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    reject_archive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    defer_child_completion: std::sync::Arc<std::sync::atomic::AtomicBool>,
    defer_next_child_opening: std::sync::Arc<std::sync::atomic::AtomicBool>,
    deferred_child_opening:
        std::sync::Arc<std::sync::Mutex<Option<awaken_agent_contract::RunLifecycleEvent>>>,
    hold_next_user_run_activation: std::sync::Arc<std::sync::atomic::AtomicBool>,
    user_run_activation_entered: std::sync::Arc<tokio::sync::Notify>,
    user_run_activation_release: std::sync::Arc<tokio::sync::Notify>,
    live: std::sync::Arc<
        tokio::sync::broadcast::Sender<awaken_agent_contract::stream::event::Observation>,
    >,
    live_subscribed: std::sync::Arc<tokio::sync::Notify>,
}

struct FakeLiveSubscription {
    receiver: tokio::sync::broadcast::Receiver<awaken_agent_contract::stream::event::Observation>,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionThreadLiveSubscription for FakeLiveSubscription {
    async fn recv(
        &mut self,
    ) -> Result<
        Option<awaken_agent_contract::stream::event::Observation>,
        awaken_session_error_contract::RunError,
    > {
        loop {
            match self.receiver.recv().await {
                Ok(event) => return Ok(Some(event)),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(None),
            }
        }
    }
}

impl Default for CoordinatedRuntimeFake {
    fn default() -> Self {
        Self {
            completed_user_dispatches: Default::default(),
            runs: Default::default(),
            reserved_user_runs: Default::default(),
            user_run_states: Default::default(),
            user_run_outcomes: Default::default(),
            root_messages: Default::default(),
            root_message_cursors: Default::default(),
            child_messages: Default::default(),
            child_message_cursors: Default::default(),
            coordination_operation_id: Default::default(),
            lifecycle: Default::default(),
            interrupts: Default::default(),
            rejected_interrupt: Default::default(),
            disposition_by_thread: Default::default(),
            disposition_commit_cursors: Default::default(),
            archive_commits: Default::default(),
            reject_archive: Default::default(),
            defer_child_completion: Default::default(),
            defer_next_child_opening: Default::default(),
            deferred_child_opening: Default::default(),
            hold_next_user_run_activation: Default::default(),
            user_run_activation_entered: Default::default(),
            user_run_activation_release: Default::default(),
            live: std::sync::Arc::new(tokio::sync::broadcast::channel(32).0),
            live_subscribed: Default::default(),
        }
    }
}

impl CoordinatedRuntimeFake {
    fn committed_usage(thread_id: &str) -> awaken_runtime_contract::llm::ThreadUsage {
        let mut usage = awaken_runtime_contract::llm::ThreadUsage::default();
        usage.record(
            "test-model",
            awaken_runtime_contract::llm::TokenUsage {
                prompt_tokens: if thread_id == Self::CHILD_THREAD_ID {
                    5
                } else {
                    10
                },
                ..Default::default()
            },
        );
        usage
    }
    pub const CHILD_THREAD_ID: &'static str = "sthr_projection_child";

    fn receipt() -> awaken_agent_contract::agent::content::ContentBlock {
        awaken_agent_contract::agent::content::ContentBlock::text(
            serde_json::json!({
                "accepted": true,
                "session_thread_id": Self::CHILD_THREAD_ID,
            })
            .to_string(),
        )
    }

    /// Drain the exact physical-partition interrupt calls observed by the fake.
    /// Child entries include both parent and logical child identities so tests
    /// cannot accidentally pass when an adapter opens a child-named partition.
    pub fn take_interrupts(&self) -> Vec<String> {
        std::mem::take(&mut *self.interrupts.lock().unwrap())
    }

    /// Exact activation coordinates for explicit simulated post-Dispatch
    /// delivery. Terminal transcript projection cannot manufacture this fence.
    pub fn completed_user_dispatches(
        &self,
    ) -> std::sync::Arc<
        std::sync::Mutex<Vec<awaken_session_execution_contract::SessionAgentBoundaryCommand>>,
    > {
        self.completed_user_dispatches.clone()
    }

    /// Reject one exact physical interrupt target while continuing to record
    /// every call. This models a partial fan-out failure without inventing a
    /// second interrupt implementation in protocol tests.
    pub fn reject_interrupt(&self, target: Option<String>) {
        *self.rejected_interrupt.lock().unwrap() = target;
    }

    /// Return the durable archive transitions observed by the fake. Retries of
    /// an already-Archived child do not add a second transition.
    pub fn archive_commits(&self) -> Vec<(String, String)> {
        self.archive_commits.lock().unwrap().clone()
    }

    pub fn reject_archive(&self, reject: bool) {
        self.reject_archive
            .store(reject, std::sync::atomic::Ordering::SeqCst);
    }

    /// Hold the child at Running so an HTTP Thread stream can attach before the
    /// fake publishes live deltas and the terminal commit.
    pub fn defer_child_completion(&self) {
        self.defer_child_completion
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Hold only the next child's Running commit while the accepted parent
    /// send and root boundary remain committed. The later opening receives the
    /// next global source cursor, never a retroactive cursor in the prefix.
    pub fn defer_next_child_opening(&self) {
        self.defer_next_child_opening
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn commit_deferred_child_opening(&self) {
        let mut opening = self
            .deferred_child_opening
            .lock()
            .unwrap()
            .take()
            .expect("one deferred child Running commit");
        let root_cursor = self
            .root_message_cursors
            .lock()
            .unwrap()
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        let child_cursor = self
            .child_message_cursors
            .lock()
            .unwrap()
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        let mut lifecycle = self.lifecycle.lock().unwrap();
        let source_cursor = lifecycle
            .iter()
            .map(|event| event.source_commit_cursor)
            .chain([root_cursor, child_cursor])
            .max()
            .unwrap_or_default()
            + 1;
        opening.source_commit_cursor = source_cursor;
        opening.cursor = awaken_agent_contract::encode_run_lifecycle_cursor(source_cursor, 0)
            .expect("deferred child cursor fits Runtime encoding");
        lifecycle.push(opening);
    }

    /// Pause the next canonical User Run activation after its Session activity
    /// receipt is durable, without replacing reservation, execution, or Run
    /// recovery. This one-shot gate exists only to observe that exact boundary.
    pub fn hold_next_user_run_activation(&self) {
        self.hold_next_user_run_activation
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub async fn wait_for_user_run_activation(&self) {
        self.user_run_activation_entered.notified().await;
    }

    pub fn release_user_run_activation(&self) {
        self.user_run_activation_release.notify_one();
    }

    pub async fn wait_for_live_subscription(&self) {
        self.live_subscribed.notified().await;
    }

    fn current_child_response(&self) -> (awaken_agent_contract::agent::run::Id, usize) {
        let run_ordinal = self.runs.load(std::sync::atomic::Ordering::SeqCst);
        let run_id = awaken_agent_contract::agent::run::Id(format!("coord-run-{run_ordinal}"));
        let step = self
            .child_messages
            .lock()
            .unwrap()
            .iter()
            .filter_map(|message| message.id.assistant_step_of(&run_id))
            .max()
            .map_or(0, |step| step + 1);
        (run_id, step)
    }

    pub fn publish_child_live_text(&self, text: &str) {
        let (run_id, step) = self.current_child_response();
        let _ = self.live.send(
            awaken_agent_contract::stream::event::Observation::assistant_delta(
                run_id,
                awaken_agent_contract::agent::thread::Id(Self::CHILD_THREAD_ID.into()),
                step,
                0,
                awaken_agent_contract::event::AgentEvent::Delta(
                    awaken_agent_contract::event::Delta::TextDelta { delta: text.into() },
                ),
            ),
        );
    }

    pub fn publish_root_live_text(&self, session_id: &str, text: &str) {
        let _ = self.live.send(
            awaken_agent_contract::stream::event::Observation::assistant_delta(
                awaken_agent_contract::agent::run::Id("root-live-run".into()),
                awaken_agent_contract::agent::thread::Id(session_id.into()),
                0,
                0,
                awaken_agent_contract::event::AgentEvent::Delta(
                    awaken_agent_contract::event::Delta::TextDelta { delta: text.into() },
                ),
            ),
        );
    }

    pub fn publish_child_live_reasoning(&self, text: &str) {
        let (run_id, step) = self.current_child_response();
        let _ = self.live.send(
            awaken_agent_contract::stream::event::Observation::assistant_delta(
                run_id,
                awaken_agent_contract::agent::thread::Id(Self::CHILD_THREAD_ID.into()),
                step,
                0,
                awaken_agent_contract::event::AgentEvent::Delta(
                    awaken_agent_contract::event::Delta::ReasoningDelta { delta: text.into() },
                ),
            ),
        );
    }

    pub fn publish_child_live_tool(&self) {
        let (run_id, step) = self.current_child_response();
        let _ = self.live.send(
            awaken_agent_contract::stream::event::Observation::assistant_delta(
                run_id,
                awaken_agent_contract::agent::thread::Id(Self::CHILD_THREAD_ID.into()),
                step,
                0,
                awaken_agent_contract::event::AgentEvent::Delta(
                    awaken_agent_contract::event::Delta::ToolCallDelta {
                        id: "ordinary-proof".into(),
                        name: "read".into(),
                        args_delta: "{}".into(),
                    },
                ),
            ),
        );
    }

    pub fn commit_child_ordinary_message(&self, text: &str) {
        use awaken_agent_contract::agent::content::ContentBlock;
        use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
        let (run_id, step) = self.current_child_response();
        let run_ordinal = self.runs.load(std::sync::atomic::Ordering::SeqCst);
        // The deferred parent Run already committed its terminal at base+3.
        // The shared ThreadCommit partition gives this later child Message the
        // next cursor; reusing base+3 would retroactively insert a fact before
        // a Session event that was already listable.
        let source_commit_cursor = run_ordinal.saturating_sub(1) * 4 + 4;
        let mut messages = self.child_messages.lock().unwrap();
        let mut cursors = self.child_message_cursors.lock().unwrap();
        messages.push(Message::new(
            MessageId::assistant(&run_id, step),
            Role::Assistant,
            vec![
                ContentBlock::text(text),
                ContentBlock::tool_use(
                    "ordinary-proof",
                    "read",
                    serde_json::json!({"path":"docs"}),
                ),
            ],
        ));
        cursors.push(source_commit_cursor);
    }

    pub fn complete_deferred_child(
        &self,
        text: &str,
        admission: awaken_session_execution_contract::SessionRunActivityAdmission,
    ) -> awaken_session_state_contract::SessionRuntimeIntervalObservation {
        use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
        use awaken_agent_contract::agent::run::{EndCause, RunState};
        let run_ordinal = self.runs.load(std::sync::atomic::Ordering::SeqCst);
        let base = run_ordinal.saturating_sub(1) * 4;
        let (run_id, step) = self.current_child_response();
        let awaken_session_execution_contract::SessionRunActivityAdmission::Admitted {
            session_activity_epoch,
        } = admission
        else {
            panic!("deferred child requires the committed activity receipt");
        };
        let source_commit_cursor = base + 5;
        let observation = awaken_session_state_contract::SessionRuntimeIntervalObservation {
            activity_epoch: session_activity_epoch,
            thread_id: awaken_agent_contract::agent::thread::Id(Self::CHILD_THREAD_ID.into()),
            run_id: run_id.clone(),
            lifecycle_cursor: awaken_agent_contract::encode_run_lifecycle_cursor(
                source_commit_cursor,
                0,
            )
            .unwrap(),
            source_commit_cursor,
        };
        {
            let mut messages = self.child_messages.lock().unwrap();
            let mut cursors = self.child_message_cursors.lock().unwrap();
            messages.push(Message::text(
                MessageId::assistant(&run_id, step),
                Role::Assistant,
                text,
            ));
            cursors.push(source_commit_cursor);
        }
        self.lifecycle
            .lock()
            .unwrap()
            .push(awaken_agent_contract::RunLifecycleEvent {
                cursor: observation.lifecycle_cursor,
                source_commit_cursor,
                thread_id: awaken_agent_contract::agent::thread::Id(Self::CHILD_THREAD_ID.into()),
                run_id,
                kind: awaken_agent_contract::RunLifecycleEventKind::Completed,
                state: RunState::Ended(EndCause::NaturalEnd),
                await_reason: None,
            });
        observation
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRuntime for CoordinatedRuntimeFake {
    async fn run(
        &self,
        _agent: &str,
        thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        use awaken_agent_contract::agent::content::ContentBlock;
        use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
        use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};

        let run_ordinal = self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let base = (run_ordinal - 1) * 4;
        let (root_run_id, synthetic_root_lifecycle) = {
            let mut lifecycle = self.lifecycle.lock().unwrap();
            if let Some(root) = lifecycle
                .iter()
                .find(|event| event.thread_id.0 == thread && event.source_commit_cursor == base + 1)
            {
                (root.run_id.clone(), false)
            } else {
                let root_run_id = RunId(format!("coord-root-{run_ordinal}"));
                lifecycle.push(awaken_agent_contract::RunLifecycleEvent {
                    cursor: awaken_agent_contract::encode_run_lifecycle_cursor(base + 1, 0)
                        .unwrap(),
                    source_commit_cursor: base + 1,
                    thread_id: awaken_agent_contract::agent::thread::Id(thread.into()),
                    run_id: root_run_id.clone(),
                    kind: awaken_agent_contract::RunLifecycleEventKind::Running,
                    state: RunState::Running,
                    await_reason: None,
                });
                (root_run_id, true)
            }
        };
        let call_id = format!("send-{run_ordinal}");
        let target = if run_ordinal == 1 {
            serde_json::json!({"agent_id":"researcher","message":"find the docs"})
        } else {
            serde_json::json!({
                "session_thread_id": Self::CHILD_THREAD_ID,
                "message":"verify them",
            })
        };
        let delta = vec![
            Message::new(
                MessageId::assistant(&root_run_id, 0),
                Role::Assistant,
                vec![
                    ContentBlock::text(format!("coordinating {run_ordinal}")),
                    ContentBlock::ToolUse {
                        id: call_id.clone(),
                        name: awaken_managed_session_events::SEND_MESSAGE_TOOL_ID.into(),
                        input: target,
                    },
                ],
            ),
            Message::new(
                MessageId::tool_result(&call_id),
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: call_id.clone(),
                    content: vec![Self::receipt()],
                    is_error: false,
                }],
            ),
        ];
        {
            let mut messages = self.root_messages.lock().unwrap();
            let mut cursors = self.root_message_cursors.lock().unwrap();
            messages.extend(delta.clone());
            cursors.extend([base + 1, base + 1]);
        }
        self.coordination_operation_id
            .lock()
            .unwrap()
            .get_or_insert_with(|| ToolBatch::operation_id_for_step(&root_run_id, 0, &call_id));
        let run_id = RunId(format!("coord-run-{run_ordinal}"));
        let running = awaken_agent_contract::RunLifecycleEvent {
            cursor: awaken_agent_contract::encode_run_lifecycle_cursor(base + 2, 0).unwrap(),
            source_commit_cursor: base + 2,
            thread_id: awaken_agent_contract::agent::thread::Id(Self::CHILD_THREAD_ID.into()),
            run_id: run_id.clone(),
            kind: awaken_agent_contract::RunLifecycleEventKind::Running,
            state: RunState::Running,
            await_reason: None,
        };
        if self
            .defer_next_child_opening
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let replaced = self.deferred_child_opening.lock().unwrap().replace(running);
            assert!(replaced.is_none(), "only one deferred child opening");
        } else {
            self.lifecycle.lock().unwrap().push(running);
        }
        if !self
            .defer_child_completion
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            {
                let mut messages = self.child_messages.lock().unwrap();
                let mut cursors = self.child_message_cursors.lock().unwrap();
                messages.push(Message::new(
                    MessageId::assistant(&run_id, 0),
                    Role::Assistant,
                    vec![
                        // DeepSeek and other reasoning providers commit this alongside
                        // the public answer. Managed message projection must retain it
                        // in Thread truth without leaking it into a cross-Thread wire.
                        ContentBlock::thinking("private child reasoning"),
                        ContentBlock::text(if run_ordinal == 1 {
                            "here are the docs"
                        } else {
                            "the docs are verified"
                        }),
                    ],
                ));
                cursors.push(base + 3);
            }
            self.lifecycle
                .lock()
                .unwrap()
                .push(awaken_agent_contract::RunLifecycleEvent {
                    cursor: awaken_agent_contract::encode_run_lifecycle_cursor(base + 3, 0)
                        .unwrap(),
                    source_commit_cursor: base + 3,
                    thread_id: awaken_agent_contract::agent::thread::Id(
                        Self::CHILD_THREAD_ID.into(),
                    ),
                    run_id,
                    kind: awaken_agent_contract::RunLifecycleEventKind::Completed,
                    state: RunState::Ended(EndCause::NaturalEnd),
                    await_reason: None,
                });
        }
        if synthetic_root_lifecycle {
            let root_terminal_cursor = if self
                .defer_child_completion
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                base + 3
            } else {
                base + 4
            };
            self.lifecycle
                .lock()
                .unwrap()
                .push(awaken_agent_contract::RunLifecycleEvent {
                    cursor: awaken_agent_contract::encode_run_lifecycle_cursor(
                        root_terminal_cursor,
                        0,
                    )
                    .unwrap(),
                    source_commit_cursor: root_terminal_cursor,
                    thread_id: awaken_agent_contract::agent::thread::Id(thread.into()),
                    run_id: root_run_id,
                    kind: awaken_agent_contract::RunLifecycleEventKind::Completed,
                    state: RunState::Ended(EndCause::NaturalEnd),
                    await_reason: None,
                });
        }
        Ok(awaken_session_application_contract::StepOutcome::ended(
            delta,
            EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        unreachable!()
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        unreachable!()
    }

    fn model(&self) -> String {
        "test-model".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for CoordinatedRuntimeFake);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for CoordinatedRuntimeFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for CoordinatedRuntimeFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for CoordinatedRuntimeFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_runtime_contract::authority_lease::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        crate::test_support::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        complete_session_projection_init(thread, &projection, &mode)?;
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for CoordinatedRuntimeFake {
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_error_contract::RunError,
    > {
        if self
            .user_run_states
            .lock()
            .unwrap()
            .contains_key(&command.run_id.0)
        {
            return Ok(awaken_session_execution_contract::SessionRunReservation::Completed);
        }
        let mut reservations = self.reserved_user_runs.lock().unwrap();
        if let Some(existing) = reservations.get(&command.run_id.0) {
            if existing != &command {
                return Err(awaken_session_error_contract::RunError::bad_request(
                    "test reservation replay changed its canonical User Run",
                ));
            }
            return Ok(awaken_session_execution_contract::SessionRunReservation::AlreadyReserved);
        }
        reservations.insert(command.run_id.0.clone(), command);
        Ok(awaken_session_execution_contract::SessionRunReservation::Reserved)
    }

    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivation,
        awaken_session_error_contract::RunError,
    > {
        if self
            .user_run_states
            .lock()
            .unwrap()
            .contains_key(&delivery.run_id.0)
        {
            return Ok(awaken_session_execution_contract::SessionRunActivation::Completed);
        }
        let command = self
            .reserved_user_runs
            .lock()
            .unwrap()
            .get(&delivery.run_id.0)
            .cloned()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::bad_request(
                    "test activation has no matching User Run reservation",
                )
            })?;
        if command.session_id != delivery.session_id {
            return Err(awaken_session_error_contract::RunError::bad_request(
                "test activation changed its Session identity",
            ));
        }
        if self
            .hold_next_user_run_activation
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.user_run_activation_entered.notify_one();
            self.user_run_activation_release.notified().await;
        }

        let ordinal = self.runs.load(std::sync::atomic::Ordering::SeqCst) + 1;
        let base = (ordinal - 1) * 4;
        {
            let mut messages = self.root_messages.lock().unwrap();
            let mut cursors = self.root_message_cursors.lock().unwrap();
            messages.extend(command.messages.clone());
            cursors.extend(std::iter::repeat_n(base + 1, command.messages.len()));
        }

        self.lifecycle
            .lock()
            .unwrap()
            .push(awaken_agent_contract::RunLifecycleEvent {
                cursor: awaken_agent_contract::encode_run_lifecycle_cursor(base + 1, 0).unwrap(),
                source_commit_cursor: base + 1,
                thread_id: awaken_agent_contract::agent::thread::Id(command.session_id.to_string()),
                run_id: command.run_id.clone(),
                kind: awaken_agent_contract::RunLifecycleEventKind::Running,
                state: awaken_agent_contract::agent::run::RunState::Running,
                await_reason: None,
            });
        let deferred = self
            .defer_child_completion
            .load(std::sync::atomic::Ordering::SeqCst);
        // The production application already boxes each Event reconciliation.
        // Keep this integration fake's reused legacy Run body behind the same
        // heap boundary so the test does not require a non-default Tokio stack.
        let outcome = Box::pin(
            <Self as awaken_session_execution_contract::SessionRuntime>::run(
                self,
                &command.agent_id,
                command.session_id.as_str(),
                command
                    .messages
                    .last()
                    .map(|message| message.content.clone())
                    .unwrap_or_default(),
            ),
        )
        .await?;
        let root_terminal_cursor = if deferred { base + 3 } else { base + 4 };
        let boundary = awaken_session_execution_contract::SessionAgentBoundaryCommand {
            session_id: delivery.session_id.clone(),
            source_thread_id: awaken_agent_contract::agent::thread::Id(String::from(
                delivery.session_id,
            )),
            source_run_id: delivery.run_id,
            source_agent_id: command.agent_id.clone(),
            session_activity_epoch: delivery.session_activity_epoch,
            cancellation_requested: false,
        };
        self.lifecycle
            .lock()
            .unwrap()
            .push(awaken_agent_contract::RunLifecycleEvent {
                cursor: awaken_agent_contract::encode_run_lifecycle_cursor(root_terminal_cursor, 0)
                    .unwrap(),
                source_commit_cursor: root_terminal_cursor,
                thread_id: awaken_agent_contract::agent::thread::Id(String::from(
                    command.session_id,
                )),
                run_id: command.run_id.clone(),
                kind: awaken_agent_contract::RunLifecycleEventKind::Completed,
                state: outcome.state().clone(),
                await_reason: outcome.await_reason().cloned(),
            });
        self.user_run_states
            .lock()
            .unwrap()
            .insert(command.run_id.0.clone(), outcome.state().clone());
        self.user_run_outcomes
            .lock()
            .unwrap()
            .insert(command.run_id.0, outcome);
        self.completed_user_dispatches
            .lock()
            .unwrap()
            .push(boundary);
        Ok(awaken_session_execution_contract::SessionRunActivation::Activated)
    }

    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        _input_message_ids: Vec<String>,
        _sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        let run_id = admission.run_id().clone();
        match admission {
            awaken_session_execution_contract::AdmittedSessionRun::Reserved(delivery)
            | awaken_session_execution_contract::AdmittedSessionRun::AlreadyReserved(delivery) => {
                let _ = self.activate_session_run(delivery).await?;
            }
            awaken_session_execution_contract::AdmittedSessionRun::AlreadyActivated(_)
            | awaken_session_execution_contract::AdmittedSessionRun::Completed { .. } => {}
            awaken_session_execution_contract::AdmittedSessionRun::RecoveryClaimed { .. } => {
                return Err(awaken_session_error_contract::RunError::unavailable(
                    "test reservation recovery has not completed",
                ));
            }
        }
        self.user_run_outcomes
            .lock()
            .unwrap()
            .get(&run_id.0)
            .cloned()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "test committed Session Run outcome is unavailable",
                )
            })
    }

    async fn session_run_state(
        &self,
        _session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::agent::run::RunState>,
        awaken_session_error_contract::RunError,
    > {
        Ok(self.user_run_states.lock().unwrap().get(&run_id.0).cloned())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        _observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_error_contract::RunError,
    > {
        let run_count = self.runs.load(std::sync::atomic::Ordering::SeqCst);
        let operation_id = self.coordination_operation_id.lock().unwrap().clone();
        Ok(operation_id
            .map(
                |created_by_operation_id| awaken_session_execution_contract::CoordinatedThreadLink {
                    session_id: session_id.clone(),
                    thread_id: awaken_agent_contract::agent::thread::Id(
                        Self::CHILD_THREAD_ID.into(),
                    ),
                    target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
                        agent_id: "researcher".into(),
                    },
                    created_by_operation_id,
                    latest_run_id: Some(awaken_agent_contract::agent::run::Id(format!(
                        "coord-run-{run_count}"
                    ))),
                },
            )
            .into_iter()
            .collect())
    }

    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_error_contract::RunError,
    > {
        if thread_id != session_id && thread_id != Self::CHILD_THREAD_ID {
            return Ok(None);
        }
        let subscription = FakeLiveSubscription {
            receiver: self.live.subscribe(),
        };
        self.live_subscribed.notify_one();
        Ok(Some(Box::new(subscription)))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        let target = format!("child:{session_id}:{}", child_thread_id.0);
        self.interrupts.lock().unwrap().push(target.clone());
        if self.rejected_interrupt.lock().unwrap().as_ref() == Some(&target) {
            return Err(awaken_session_error_contract::RunError::unavailable(
                "injected child interrupt failure",
            ));
        }
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_session_error_contract::RunError,
    > {
        self.streaming_runs()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "StreamingRunRuntime is unsupported by this runtime",
                )
            })?
            .interrupt(thread)
            .await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .thread_reads()
                .ok_or_else(|| {
                    awaken_session_error_contract::RunError::unavailable(
                        "ThreadReadRuntime is unsupported by this runtime",
                    )
                })?
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::BudgetRuntime for CoordinatedRuntimeFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Resume an exact `BudgetReached` ticket without injecting transcript
    /// content. Implementations reuse the foreground Run driver or the existing
    /// durable dispatch row; they must not enqueue a replacement Run.
    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_application_contract::SessionBudgetResumeDelivery,
    ) -> Result<
        awaken_session_application_contract::SessionBudgetResumeDisposition,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .resume_budget_reached(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Discover current budget-paused continuations from committed Run tickets
    /// and existing Session-affine dispatch rows. This is a recovery query over
    /// those authorities, never a pause registry.
    async fn session_budget_resume_tickets(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<
        Vec<awaken_session_application_contract::SessionBudgetResumeTicket>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_budget_resume_tickets(session_id)
            .await
    }

    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, awaken_session_error_contract::RunError>
    {
        Ok(
            awaken_session_application_contract::session_usage_from_thread_usage(
                Self::committed_usage(thread),
            ),
        )
    }

    async fn session_thread_usage(
        &self,
        _session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, awaken_session_error_contract::RunError>
    {
        Ok(
            awaken_session_application_contract::session_usage_from_thread_usage(
                Self::committed_usage(thread_id),
            ),
        )
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRunRuntime for CoordinatedRuntimeFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one request with its protocol-projected, backend-neutral content
    /// owner. Adapters that do not support attributed capture may use the
    /// compatibility default; runtime hosts override it and persist attribution
    /// on the durable activation.
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one user request, installing `sink` as the Run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`awaken_session_application_contract::StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run_streaming(agent, thread, content, sink).await
    }

    async fn interrupt(&self, thread: &str) -> Result<(), awaken_session_error_contract::RunError> {
        let target = format!("primary:{thread}");
        self.interrupts.lock().unwrap().push(target.clone());
        if self.rejected_interrupt.lock().unwrap().as_ref() == Some(&target) {
            return Err(awaken_session_error_contract::RunError::unavailable(
                "injected primary interrupt failure",
            ));
        }
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next Run resolves the new executor.
    async fn rebind_model(
        &self,
        thread: &str,
        model: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .rebind_model(thread, model)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Whether the thread's selected model accepts a system message after the
    /// conversation has started. Runtimes whose models do not support this can
    /// fail admission before the Managed adapter persists the inbound event.
    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        awaken_session_execution_contract::NeutralRuntime
            .supports_mid_conversation_system(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The tool currently blocking `thread`, if any. Admission uses this
    /// read-only projection to reject events that cannot legally precede the
    /// matching result; resume remains the sole mutating authority.
    async fn pending_tool(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_application_contract::Pending>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .pending_tool(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for CoordinatedRuntimeFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    async fn committed_messages(
        &self,
        _thread: &str,
    ) -> Result<
        Vec<awaken_agent_contract::agent::message::Message>,
        awaken_session_error_contract::RunError,
    > {
        Ok(self.root_messages.lock().unwrap().clone())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let run_count = self.runs.load(std::sync::atomic::Ordering::SeqCst);
        if run_count == 0 {
            return Ok(None);
        }
        // Recovery-fixture decision table: C1 a Thread has committed Messages,
        // C2 it has an Archived state command. E1 every Message has its exact
        // same-index commit coordinate; E2 the state command has its recorded
        // archive coordinate; E3 the snapshot high-water covers both. Empty
        // vectors remain aligned by construction. These are projections of this
        // fake's one commit path, never fallback coordinates minted at read time.
        let (messages, message_commit_cursors) = if thread_id == session_id {
            let messages = self.root_messages.lock().unwrap();
            let cursors = self.root_message_cursors.lock().unwrap();
            (messages.clone(), cursors.clone())
        } else if thread_id == Self::CHILD_THREAD_ID {
            let messages = self.child_messages.lock().unwrap();
            let cursors = self.child_message_cursors.lock().unwrap();
            (messages.clone(), cursors.clone())
        } else {
            return Ok(None);
        };
        debug_assert_eq!(messages.len(), message_commit_cursors.len());
        let lifecycle = self.lifecycle.lock().unwrap();
        let thread_lifecycle = lifecycle
            .iter()
            .filter(|event| event.thread_id.0 == thread_id)
            .cloned()
            .collect::<Vec<_>>();
        let Some(run_id) = thread_lifecycle.last().map(|event| event.run_id.clone()) else {
            return Ok(None);
        };
        let mut runs = Vec::<awaken_agent_contract::agent::run::Record>::new();
        for event in &thread_lifecycle {
            if let Some(run) = runs.iter_mut().find(|run| run.id == event.run_id) {
                run.state = event.state.clone();
            } else {
                runs.push(awaken_agent_contract::agent::run::Record {
                    id: event.run_id.clone(),
                    thread_id: event.thread_id.clone(),
                    state: event.state.clone(),
                });
            }
        }
        let disposition_key = (session_id.to_string(), thread_id.to_string());
        let (mut state, mut state_commit_cursors) = {
            let dispositions = self.disposition_by_thread.lock().unwrap();
            let cursors = self.disposition_commit_cursors.lock().unwrap();
            if dispositions.get(&disposition_key)
                == Some(&awaken_agent_contract::ThreadDisposition::Archived)
            {
                (
                    vec![awaken_agent_contract::archive_thread_command()],
                    cursors
                        .get(&disposition_key)
                        .copied()
                        .into_iter()
                        .collect::<Vec<_>>(),
                )
            } else {
                (Vec::new(), Vec::new())
            }
        };
        debug_assert_eq!(state.len(), state_commit_cursors.len());
        let store_cursor = lifecycle
            .iter()
            .map(|event| event.source_commit_cursor)
            .chain(message_commit_cursors.iter().copied())
            .chain(state_commit_cursors.iter().copied())
            .max()
            .unwrap_or_default();
        // M1-M2/E5: the root (10) and child (5) counters are committed Thread
        // state, matching the Budget projection above. One shared fixture fact
        // must survive either read path and repeated Session aggregation. It
        // is committed with the Thread's own work, so a later disposition
        // commit does not move it.
        use awaken_agent_contract::agent::state::StateKey as _;
        state.push(awaken_runtime_contract::llm::ThreadUsageKey::write(
            &Self::committed_usage(thread_id),
        ));
        state_commit_cursors.push(
            thread_lifecycle
                .iter()
                .map(|event| event.source_commit_cursor)
                .chain(message_commit_cursors.iter().copied())
                .max()
                .unwrap_or(store_cursor),
        );
        Ok(Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                claimed_run_id: run_id.clone(),
                runs,
                latest_run_id: Some(run_id),
                messages,
                message_commit_cursors,
                state,
                state_commit_cursors,
                events: lifecycle_audit_records(lifecycle.iter().filter(|event| {
                    event.thread_id.0 == thread_id && event.source_commit_cursor <= store_cursor
                })),
                resume_tickets: Vec::new(),
                thread_version: lifecycle.len() as u64,
                store_cursor,
                next_commit_ordinal: 0,
            },
        ))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_error_contract::RunError>
    {
        Ok(self
            .disposition_by_thread
            .lock()
            .unwrap()
            .get(&(session_id.to_string(), thread_id.to_string()))
            .copied()
            .unwrap_or_default())
    }

    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        if self
            .reject_archive
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(awaken_session_error_contract::RunError::internal(
                "archive disposition commit failed",
            ));
        }
        let key = (session_id.to_string(), thread_id.to_string());
        let lifecycle_cursor = self
            .lifecycle
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.source_commit_cursor)
            .max()
            .unwrap_or_default();
        let root_message_cursor = self
            .root_message_cursors
            .lock()
            .unwrap()
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        let child_message_cursor = self
            .child_message_cursors
            .lock()
            .unwrap()
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        let disposition_cursor = self
            .disposition_commit_cursors
            .lock()
            .unwrap()
            .values()
            .copied()
            .max()
            .unwrap_or_default();
        let source_commit_cursor = [
            lifecycle_cursor,
            root_message_cursor,
            child_message_cursor,
            disposition_cursor,
        ]
        .into_iter()
        .max()
        .unwrap_or_default()
        .checked_add(1)
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::internal(
                "test disposition commit cursor exhausted",
            )
        })?;
        let mut dispositions = self.disposition_by_thread.lock().unwrap();
        let mut disposition_cursors = self.disposition_commit_cursors.lock().unwrap();
        if dispositions.get(&key) != Some(&awaken_agent_contract::ThreadDisposition::Archived) {
            dispositions.insert(
                key.clone(),
                awaken_agent_contract::ThreadDisposition::Archived,
            );
            disposition_cursors.insert(key.clone(), source_commit_cursor);
            self.archive_commits.lock().unwrap().push(key);
        }
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for CoordinatedRuntimeFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<
        awaken_session_execution_contract::OutcomeDrive,
        awaken_session_error_contract::RunError,
    > {
        unreachable!()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for CoordinatedRuntimeFake {
    fn capabilities(&self) -> awaken_session_configuration_contract::AgentCapabilities {
        awaken_session_configuration_contract::AgentCapabilities {
            delegates: vec!["researcher".into()],
            ..Default::default()
        }
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The capability surface visible to one prepared session. Runtimes whose
    /// catalogs are workspace-scoped override this; simple runtimes inherit the
    /// process-wide view for backwards compatibility.
    fn capabilities_for(
        &self,
        _thread: &str,
    ) -> awaken_session_configuration_contract::AgentCapabilities {
        self.capabilities()
    }
}
