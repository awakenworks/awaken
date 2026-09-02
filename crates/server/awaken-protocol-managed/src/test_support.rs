//! Explicit test-only startup for the split Environment owners.

use std::sync::Arc;

use awaken_environment_execution_application::{
    CoordinatorEnvironmentRegistrar, EnvironmentExecutionApplication,
};
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrar;
use awaken_runtime_contract::tool_batch::ToolBatch;

use crate::types::tunnel::{Tunnel, TunnelCertificate, TunnelToken};
use crate::{
    EnvironmentAuthoringState, ManagedTunnelApplication, ManagedTunnelApplicationError,
    ManagedTunnelScope,
};

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
    projection: &awaken_session_contract::FrozenSessionProjection,
    mode: &awaken_session_contract::SessionProjectionInstallMode,
) -> Result<Option<awaken_session_contract::SessionInit>, awaken_session_contract::RunError> {
    if thread.is_empty()
        || projection.workspace_id.is_empty()
        || projection.baseline.agent_id.is_empty()
    {
        return Err(awaken_session_contract::RunError::bad_request(
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
    effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    authorization: &awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
) -> Result<awaken_session_contract::SessionCleanupPreparation, awaken_session_contract::RunError> {
    authorization
        .verify_for(effect)
        .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
    let provider_prepared_effect_fence = effect
        .sandbox_effect_fence()
        .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
    awaken_session_contract::SessionCleanupPreparation::try_new(
        effect,
        provider_prepared_effect_fence,
        Vec::new(),
    )
    .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))
}

/// Complete the physical half of a protocol test Runtime from the one typed
/// aggregate command. The receipt constructor remains the canonical identity
/// owner; this helper owns no cleanup state or compatibility behavior.
#[must_use]
pub fn complete_terminal_cleanup_disposal(
    effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
) -> awaken_session_contract::SessionCleanupDisposalReceipt {
    awaken_session_contract::SessionCleanupDisposalReceipt::new(&effect.command)
}

#[must_use]
pub fn environment_components() -> (
    Arc<EnvironmentAuthoringState>,
    Arc<EnvironmentExecutionApplication>,
) {
    let work: Arc<dyn awaken_session_contract::work_queue::WorkQueue> =
        Arc::new(awaken_work_store::InMemoryWorkQueue::new());
    let catalog =
        Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
    catalog
        .install_seed(awaken_environment_application::default_environment_registration())
        .expect("install built-in local Environment");
    let registrar: Arc<dyn ExecutableEnvironmentRegistrar> =
        Arc::new(CoordinatorEnvironmentRegistrar::new(
            Arc::new(
                awaken_executable_environment_catalog::LocalExecutableEnvironmentRegistrar::new(
                    catalog.clone(),
                ),
            ),
            work.clone(),
        ));
    let policies =
        Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default());
    let application = Arc::new(awaken_environment_application::EnvironmentApplication::new(
        Arc::new(awaken_env_store::InMemoryEnvRegistry::new()),
        registrar,
        Some(policies.clone()),
    ));
    (
        Arc::new(EnvironmentAuthoringState::new(application, policies)),
        Arc::new(EnvironmentExecutionApplication::new(work, catalog)),
    )
}

/// Shared integration-test Runtime for the canonical asynchronous coordination
/// path. It exposes one real child Thread and appends one ordinary child Run per
/// accepted primary Run; no `DelegatedRun` hint or child-named store participates.
#[derive(Clone)]
pub struct CoordinatedRuntimeFake {
    runs: std::sync::Arc<std::sync::atomic::AtomicU64>,
    reserved_user_runs: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, awaken_session_contract::AdmitSessionRun>,
        >,
    >,
    user_run_states: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, awaken_agent_contract::agent::run::RunState>,
        >,
    >,
    user_run_outcomes: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, awaken_session_contract::StepOutcome>>,
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
impl awaken_session_contract::SessionThreadLiveSubscription for FakeLiveSubscription {
    async fn recv(
        &mut self,
    ) -> Result<
        Option<awaken_agent_contract::stream::event::Observation>,
        awaken_session_contract::RunError,
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
            hold_next_user_run_activation: Default::default(),
            user_run_activation_entered: Default::default(),
            user_run_activation_release: Default::default(),
            live: std::sync::Arc::new(tokio::sync::broadcast::channel(32).0),
            live_subscribed: Default::default(),
        }
    }
}

impl CoordinatedRuntimeFake {
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
        let source_commit_cursor = run_ordinal.saturating_sub(1) * 4 + 3;
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

    pub fn complete_deferred_child(&self, text: &str) {
        use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
        use awaken_agent_contract::agent::run::{EndCause, RunState};
        let run_ordinal = self.runs.load(std::sync::atomic::Ordering::SeqCst);
        let base = run_ordinal.saturating_sub(1) * 4;
        let (run_id, step) = self.current_child_response();
        {
            let mut messages = self.child_messages.lock().unwrap();
            let mut cursors = self.child_message_cursors.lock().unwrap();
            messages.push(Message::text(
                MessageId::assistant(&run_id, step),
                Role::Assistant,
                text,
            ));
            cursors.push(base + 4);
        }
        self.lifecycle
            .lock()
            .unwrap()
            .push(awaken_agent_contract::RunLifecycleEvent {
                cursor: awaken_agent_contract::RunLifecycleCursor(base + 4),
                source_commit_cursor: base + 4,
                thread_id: awaken_agent_contract::agent::thread::Id(Self::CHILD_THREAD_ID.into()),
                run_id,
                kind: awaken_agent_contract::RunLifecycleEventKind::Completed,
                state: RunState::Ended(EndCause::NaturalEnd),
                await_reason: None,
            });
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRuntime for CoordinatedRuntimeFake {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_contract::RunError> {
        complete_session_projection_init(thread, &projection, &mode)?;
        Ok(())
    }

    async fn reserve_session_run(
        &self,
        command: awaken_session_contract::AdmitSessionRun,
    ) -> Result<awaken_session_contract::SessionRunReservation, awaken_session_contract::RunError>
    {
        if self
            .user_run_states
            .lock()
            .unwrap()
            .contains_key(&command.run_id.0)
        {
            return Ok(awaken_session_contract::SessionRunReservation::Completed);
        }
        let mut reservations = self.reserved_user_runs.lock().unwrap();
        if let Some(existing) = reservations.get(&command.run_id.0) {
            if existing != &command {
                return Err(awaken_session_contract::RunError::bad_request(
                    "test reservation replay changed its canonical User Run",
                ));
            }
            return Ok(awaken_session_contract::SessionRunReservation::AlreadyReserved);
        }
        reservations.insert(command.run_id.0.clone(), command);
        Ok(awaken_session_contract::SessionRunReservation::Reserved)
    }

    async fn activate_session_run(
        &self,
        delivery: awaken_session_contract::SessionRunDelivery,
    ) -> Result<awaken_session_contract::SessionRunActivation, awaken_session_contract::RunError>
    {
        if self
            .user_run_states
            .lock()
            .unwrap()
            .contains_key(&delivery.run_id.0)
        {
            return Ok(awaken_session_contract::SessionRunActivation::Completed);
        }
        let command = self
            .reserved_user_runs
            .lock()
            .unwrap()
            .get(&delivery.run_id.0)
            .cloned()
            .ok_or_else(|| {
                awaken_session_contract::RunError::bad_request(
                    "test activation has no matching User Run reservation",
                )
            })?;
        if command.session_id != delivery.session_id {
            return Err(awaken_session_contract::RunError::bad_request(
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
                cursor: awaken_agent_contract::RunLifecycleCursor(base + 1),
                source_commit_cursor: base + 1,
                thread_id: awaken_agent_contract::agent::thread::Id(command.session_id.clone()),
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
        let outcome = Box::pin(<Self as awaken_session_contract::SessionRuntime>::run(
            self,
            &command.agent_id,
            &command.session_id,
            command
                .messages
                .last()
                .map(|message| message.content.clone())
                .unwrap_or_default(),
        ))
        .await?;
        let root_terminal_cursor = if deferred { base + 3 } else { base + 4 };
        self.lifecycle
            .lock()
            .unwrap()
            .push(awaken_agent_contract::RunLifecycleEvent {
                cursor: awaken_agent_contract::RunLifecycleCursor(root_terminal_cursor),
                source_commit_cursor: root_terminal_cursor,
                thread_id: awaken_agent_contract::agent::thread::Id(command.session_id),
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
        Ok(awaken_session_contract::SessionRunActivation::Activated)
    }

    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_contract::AdmittedSessionRun,
        _input_message_ids: Vec<String>,
        _sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        let run_id = admission.run_id().clone();
        match admission {
            awaken_session_contract::AdmittedSessionRun::Reserved(delivery)
            | awaken_session_contract::AdmittedSessionRun::AlreadyReserved(delivery) => {
                let _ = self.activate_session_run(delivery).await?;
            }
            awaken_session_contract::AdmittedSessionRun::AlreadyActivated(_)
            | awaken_session_contract::AdmittedSessionRun::Completed { .. } => {}
            awaken_session_contract::AdmittedSessionRun::RecoveryClaimed { .. } => {
                return Err(awaken_session_contract::RunError::unavailable(
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
                awaken_session_contract::RunError::unavailable(
                    "test committed Session Run outcome is unavailable",
                )
            })
    }

    async fn session_run_state(
        &self,
        _session_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::agent::run::RunState>,
        awaken_session_contract::RunError,
    > {
        Ok(self.user_run_states.lock().unwrap().get(&run_id.0).cloned())
    }

    async fn run(
        &self,
        _agent: &str,
        thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
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
                    cursor: awaken_agent_contract::RunLifecycleCursor(base + 1),
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
                        name: awaken_ext_builtin_tools::SEND_MESSAGE.into(),
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
            cursor: awaken_agent_contract::RunLifecycleCursor(base + 2),
            source_commit_cursor: base + 2,
            thread_id: awaken_agent_contract::agent::thread::Id(Self::CHILD_THREAD_ID.into()),
            run_id: run_id.clone(),
            kind: awaken_agent_contract::RunLifecycleEventKind::Running,
            state: RunState::Running,
            await_reason: None,
        };
        self.lifecycle.lock().unwrap().push(running);
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
                    cursor: awaken_agent_contract::RunLifecycleCursor(base + 3),
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
                    cursor: awaken_agent_contract::RunLifecycleCursor(root_terminal_cursor),
                    source_commit_cursor: root_terminal_cursor,
                    thread_id: awaken_agent_contract::agent::thread::Id(thread.into()),
                    run_id: root_run_id,
                    kind: awaken_agent_contract::RunLifecycleEventKind::Completed,
                    state: RunState::Ended(EndCause::NaturalEnd),
                    await_reason: None,
                });
        }
        Ok(awaken_session_contract::StepOutcome::ended(
            delta,
            EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        unreachable!()
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        unreachable!()
    }

    async fn interrupt(&self, thread: &str) -> Result<(), awaken_session_contract::RunError> {
        let target = format!("primary:{thread}");
        self.interrupts.lock().unwrap().push(target.clone());
        if self.rejected_interrupt.lock().unwrap().as_ref() == Some(&target) {
            return Err(awaken_session_contract::RunError::unavailable(
                "injected primary interrupt failure",
            ));
        }
        Ok(())
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &str,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_contract::RunError> {
        let target = format!("child:{session_id}:{}", child_thread_id.0);
        self.interrupts.lock().unwrap().push(target.clone());
        if self.rejected_interrupt.lock().unwrap().as_ref() == Some(&target) {
            return Err(awaken_session_contract::RunError::unavailable(
                "injected child interrupt failure",
            ));
        }
        Ok(())
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, awaken_session_contract::RunError> {
        unreachable!()
    }

    async fn committed_messages(
        &self,
        _thread: &str,
    ) -> Result<
        Vec<awaken_agent_contract::agent::message::Message>,
        awaken_session_contract::RunError,
    > {
        Ok(self.root_messages.lock().unwrap().clone())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_contract::RunError,
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
        let (state, state_commit_cursors) = {
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
        Ok(Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                thread_id: awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                claimed_run_id: run_id.clone(),
                runs,
                latest_run_id: Some(run_id),
                messages,
                message_commit_cursors,
                state,
                state_commit_cursors,
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: lifecycle.len() as u64,
                store_cursor,
                next_commit_ordinal: 0,
            },
        ))
    }

    async fn committed_run_lifecycle(
        &self,
        _thread: &str,
        cursor: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_agent_contract::RunLifecyclePage, awaken_session_contract::RunError> {
        let events = self
            .lifecycle
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.cursor > cursor)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        Ok(awaken_agent_contract::RunLifecyclePage {
            next_cursor: events.last().map_or(cursor, |event| event.cursor),
            events,
        })
    }

    async fn coordinated_threads(
        &self,
        session_id: &str,
    ) -> Result<
        Vec<awaken_session_contract::CoordinatedThreadLink>,
        awaken_session_contract::RunError,
    > {
        let run_count = self.runs.load(std::sync::atomic::Ordering::SeqCst);
        let operation_id = self.coordination_operation_id.lock().unwrap().clone();
        Ok(operation_id
            .map(
                |created_by_operation_id| awaken_session_contract::CoordinatedThreadLink {
                    session_id: session_id.into(),
                    thread_id: awaken_agent_contract::agent::thread::Id(
                        Self::CHILD_THREAD_ID.into(),
                    ),
                    target: awaken_session_contract::CoordinatedThreadTarget::Agent {
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
        session_id: &str,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_contract::SessionThreadLiveSubscription>>,
        awaken_session_contract::RunError,
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

    async fn session_thread_disposition(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_contract::RunError> {
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
        session_id: &str,
        thread_id: &str,
    ) -> Result<(), awaken_session_contract::RunError> {
        if self
            .reject_archive
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(awaken_session_contract::RunError::internal(
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
            awaken_session_contract::RunError::internal("test disposition commit cursor exhausted")
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

    async fn session_usage(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_contract::SessionUsage, awaken_session_contract::RunError> {
        Ok(awaken_session_contract::SessionUsage {
            input_tokens: 10,
            ..Default::default()
        })
    }

    async fn session_thread_usage(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<awaken_session_contract::SessionUsage, awaken_session_contract::RunError> {
        Ok(awaken_session_contract::SessionUsage {
            input_tokens: 5,
            ..Default::default()
        })
    }

    fn capabilities(&self) -> awaken_session_contract::AgentCapabilities {
        awaken_session_contract::AgentCapabilities {
            delegates: vec!["researcher".into()],
            ..Default::default()
        }
    }

    fn model(&self) -> String {
        "test-model".into()
    }
}
