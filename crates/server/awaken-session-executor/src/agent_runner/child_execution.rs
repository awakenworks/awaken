// @formal entrypoint delegation.independent_child_scheduling_lease
//! Child Run boundary orchestration.
//!
//! Execution-edge materialization lives in [`super::child_attempt`] and durable
//! dispatch construction in [`super::child_dispatch`]; this module owns the
//! boundary itself: owned child tasks, durable scheduling wiring, and driving a
//! configured Agent to exactly one settled Run boundary.

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::delegation::DelegationOrigin;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_dispatch_contract::PendingInput;
use awaken_dispatch_erasure::DispatchBinding;
use awaken_runtime::RunInput;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::delegation::RunDelegationService;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_time::WallClock;
use awaken_tool_contract::tool::{RawToolRegistry, ToolExecutor};

use super::child_attempt::{
    ChildExecutionAdapters, bind_direct_child_credentials, child_attempt_executor,
    configure_child_native_runtime,
};
use super::child_dispatch::child_dispatch_request;
use super::{AgentRunBoundary, AgentRunError, AgentRunSandbox, settled_agent_boundary};
use crate::config::{build_runtime_with_authorization, effective_tool_authorization};

/// A child task must not outlive the parent future that owns its delegation
/// boundary. Tokio's bare `JoinHandle` detaches on drop; this guard makes parent
/// cancellation abort the same child execution instead of creating an orphan.
pub(crate) struct AbortOnDropTask<T> {
    handle: tokio::task::JoinHandle<T>,
}

impl<T> AbortOnDropTask<T> {
    pub(crate) fn spawn(future: impl std::future::Future<Output = T> + Send + 'static) -> Self
    where
        T: Send + 'static,
    {
        Self {
            handle: tokio::spawn(future),
        }
    }

    pub(crate) async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        (&mut self.handle).await
    }
}

impl<T> Drop for AbortOnDropTask<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(test)]
mod owned_task_tests {
    use super::*;

    #[tokio::test]
    async fn dropping_one_child_task_aborts_only_its_observation() {
        // T1 two owned children have entered; T2 A's owner is dropped; T3 B
        // returns. => A is aborted (not detached), only A latches uncertainty,
        // and B's original output is joined. No abort is treated as stop proof.
        struct Dropped(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let a = awaken_runtime_contract::runtime_context::AttemptQuiescence::default();
        let b = awaken_runtime_contract::runtime_context::AttemptQuiescence::default();
        let entered = Arc::new(tokio::sync::Notify::new());
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let owned_a = a.clone();
        let entered_a = entered.clone();
        let task_a = AbortOnDropTask::spawn(async move {
            owned_a
                .observe_return(async move {
                    let _dropped = Dropped(Some(dropped_tx));
                    entered_a.notify_one();
                    std::future::pending::<()>().await;
                })
                .await;
        });
        entered.notified().await;
        let owned_b = b.clone();
        let task_b =
            AbortOnDropTask::spawn(async move { owned_b.observe_return(async { 42 }).await });
        drop(task_a);
        tokio::time::timeout(Duration::from_secs(2), dropped_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(!a.can_acknowledge_return(), "T2 unknown A");
        assert_eq!(task_b.join().await.unwrap(), 42, "T3 original result");
        assert!(b.can_acknowledge_return(), "T2/T3 independent B");
    }
}

/// Durable scheduling dependencies for a child Run. This is host wiring, not a
/// child-specific runtime contract: the same dispatch worker and queue drive root
/// and child Runs; creation is the only difference.
#[derive(Clone)]
pub(crate) struct RunScheduler {
    /// The Host's clock: the foreground child scheduler and every later
    /// ownership operation retain this one edge clock.
    pub(crate) clock: Arc<dyn WallClock>,
    pub(crate) binding: DispatchBinding,
    /// Unfenced session commit authority. The child worker binds its own
    /// `RunClaim`; inheriting the parent's already-fenced coordinator would nest
    /// two claim guards and deadlock the shared dispatch authority.
    pub(crate) commit: Arc<dyn CommitCoordinator>,
    /// Read side of the same committed authority. Kept separately after trait
    /// erasure so cancellation/recovery can make idempotency decisions from
    /// committed child state after a process restart.
    pub(crate) reader: Arc<dyn CommittedThreadView>,
    pub(crate) owner: String,
    /// Parent Session resource authority inherited by a child dispatch. The child
    /// keeps its own Run/thread lifecycle while executing in the parent's Session
    /// environment, so a replacement worker must install the same frozen manifest.
    pub(crate) session_resources: Option<crate::session_resource::SessionResourceManifest>,
    /// Attempt-scoped publication lookup inherited by every durable child
    /// admission. A child freezes its own non-root closure from this immutable
    /// source; no execution path reopens the mutable host catalog.
    pub(crate) publication_source:
        Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
    /// Rebind the existing Session model admission port to the child Worker's
    /// own durable claim. The parent attempt's gate cannot authorize this Run.
    pub(crate) claimed_model_request_gate:
        Option<awaken_dispatch_execution::ClaimedModelRequestGateFactory>,
}

impl RunScheduler {
    /// The local Session's committed read authority for a child Thread.
    ///
    /// A database-independent Worker exposes only its parent claim's recovery
    /// projection here. That projection cannot contain a child Thread, so remote
    /// children must continue through their own claim-installed projection.
    pub(crate) fn local_child_reader(&self) -> Option<Arc<dyn CommittedThreadView>> {
        matches!(
            self.binding.placement(),
            awaken_dispatch_erasure::DispatchTopology::Local(_)
        )
        .then(|| self.reader.clone())
    }
}

/// Parent-mediated creation or continuation of one ordinary child Run. Grouping
/// relationship identity and boundary input prevents call sites from swapping
/// adjacent ids or optional payloads.
pub(crate) struct ChildRunRequest {
    pub(crate) run_id: RunId,
    pub(crate) origin: DelegationOrigin,
    pub(crate) seed: Option<RunInput>,
    pub(crate) resume: Option<ResumeResult>,
    pub(crate) parent_thread_id: ThreadId,
}

impl AgentRunError {
    /// A failed commit leaves the stable child Run recoverable. Resolution,
    /// provisioning, state-conflict, and ordinary execution errors are terminal
    /// configuration/business failures and must not loop forever.
    pub(crate) const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Runtime(awaken_runtime_contract::execution::Error::Commit(_))
        )
    }
}

/// Reconnect a foreground parent to a child that another dispatch worker won.
///
/// The durable queue deliberately permits the background pool and a foreground
/// parent to race for the same stable child id. Losing that race is not a Run
/// failure: the winner owns execution and the parent observes the resulting
/// committed boundary. This path is bounded and cancellation-aware so a dead
/// worker cannot leave the parent hanging indefinitely.
pub(super) async fn await_committed_child_boundary(
    reader: &dyn CommittedThreadView,
    child_run_id: &RunId,
    cancellation: Option<&CancellationToken>,
) -> Result<RunState, AgentRunError> {
    const SETTLE_TIMEOUT: Duration = Duration::from_secs(60);
    const POLL_INTERVAL: Duration = Duration::from_millis(10);

    let settled = async {
        loop {
            if let Some(state @ (RunState::Awaiting | RunState::Ended(_))) =
                reader.run_state(child_run_id)
            {
                return Ok(state);
            }

            if let Some(cancellation) = cancellation {
                tokio::select! {
                    () = cancellation.cancelled() => {
                        return Err(AgentRunError::Runtime(
                            awaken_runtime_contract::execution::Error::Execution(format!(
                                "waiting for child Run {:?} was cancelled",
                                child_run_id.0
                            )),
                        ));
                    }
                    () = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            } else {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    };

    tokio::time::timeout(SETTLE_TIMEOUT, settled)
        .await
        .map_err(|_| {
            AgentRunError::Runtime(awaken_runtime_contract::execution::Error::Execution(
                format!(
                    "child Run {:?} was claimed but did not reach a committed boundary",
                    child_run_id.0
                ),
            ))
        })?
}

/// Drive a configured Agent to exactly one settled Run boundary.
///
/// This is the substrate used by parent-mediated child Runs. Starting and
/// resuming both rebuild the same Agent configuration and ordinary Runtime. A
/// restart therefore needs no live child handle: the executable snapshot and
/// `ResumeTicket` are recovered from committed truth. Unlike the auxiliary
/// [`super::root_execution::run_configured_agent`] helper, this function never auto-approves a child's
/// HITL request.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_agent_until_boundary(
    config: &awaken_runtime_contract::snapshot::ExecutableAgentSnapshot,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    child_run_id: RunId,
    origin: DelegationOrigin,
    seed: Option<RunInput>,
    resume: Option<ResumeResult>,
    mut context: RuntimeRunContext,
    run_delegation: Option<Arc<dyn RunDelegationService>>,
    parent_thread_id: ThreadId,
    scheduler: Option<RunScheduler>,
    adapters: ChildExecutionAdapters,
) -> Result<AgentRunBoundary, AgentRunError> {
    let config = config.clone();
    if resume.is_some()
        && let Some(ticket) = scheduler
            .as_ref()
            .and_then(RunScheduler::local_child_reader)
            .or_else(|| context.reader.clone())
            .and_then(|reader| reader.resume_ticket(&child_run_id))
    {
        context.inherited_toolsets = ticket.inherited_toolsets;
    }
    let thread = child_run_id.0.clone();
    super::child_attempt::validate_child_tool_configuration(
        &config,
        context.inherited_toolsets.as_ref(),
    )?;
    let clock = adapters.clock.clone().ok_or_else(|| {
        AgentRunError::Runtime(awaken_runtime::ConfigurationError::MissingClock.into())
    })?;
    let created = match &sandbox {
        AgentRunSandbox::Fresh(provider) => Some(
            provider
                .create_rooted_sandbox(&crate::sandbox_projection::agent_run_sandbox_spec(&thread))
                .await
                .map_err(|error| AgentRunError::Provisioning(error.to_string()))?,
        ),
        AgentRunSandbox::Shared(_) => None,
        #[cfg(test)]
        AgentRunSandbox::SharedLocal(_) => None,
    };
    let env: &dyn crate::config::RuntimeToolSource = match &sandbox {
        AgentRunSandbox::Shared(shared) => *shared,
        #[cfg(test)]
        AgentRunSandbox::SharedLocal(shared) => *shared,
        AgentRunSandbox::Fresh(_) => created
            .as_ref()
            .expect("a Fresh Agent Run created a sandbox"),
    };
    let current_tool_executor: Arc<dyn ToolExecutor> = match &sandbox {
        AgentRunSandbox::Shared(environment) => environment.agent_tools().tool_executor(),
        #[cfg(test)]
        AgentRunSandbox::SharedLocal(_) => Arc::new(RawToolRegistry::new(env.runtime_tools())),
        AgentRunSandbox::Fresh(_) => Arc::new(RawToolRegistry::new(env.runtime_tools())),
    };
    let authorization = effective_tool_authorization(
        &config.resolved_spec.plugin_config,
        &[],
        &config.resolved_spec.plugin_config.agent.toolsets,
    );
    let runtime = build_runtime_with_authorization(
        llm,
        clock,
        env,
        &authorization,
        adapters.background_supervisor.clone(),
    );
    let mut runtime = configure_child_native_runtime(
        runtime,
        &config,
        context.inherited_toolsets.as_ref(),
        &adapters,
    )?;
    if let Some(service) = run_delegation {
        runtime = runtime.with_run_delegation(service);
    }
    if context.commit.is_none() || context.reader.is_none() {
        return Err(AgentRunError::Configuration(
            "an Agent Run requires commit and history wiring".to_string(),
        ));
    }
    let context = context.with_tool_executor(current_tool_executor);
    let parent_reader = context.reader.clone().expect("checked above");
    // A root dispatch attempt reads through a claim-scoped projection containing
    // only the parent Thread. A local durable child instead reads the complete
    // Session commit authority already carried by its scheduler. Remote children
    // keep the parent projection out of their lifecycle and install a distinct
    // child projection below when their own claim is acquired.
    let reader = scheduler
        .as_ref()
        .and_then(RunScheduler::local_child_reader)
        .unwrap_or(parent_reader);
    let thread_id = ThreadId(thread.clone());
    if let Some(state @ RunState::Ended(_)) = reader.run_state(&child_run_id) {
        return settled_agent_boundary(reader.as_ref(), &thread_id, state);
    }
    let operation = match (seed, resume) {
        (Some(seed), None) => {
            let (_, mut activation) = runtime.prepare(&config, thread, seed);
            activation.run_id = child_run_id.clone();
            activation.delegation_origin = Some(origin);
            activation = activation.with_inherited_toolsets(context.inherited_toolsets.clone());
            Ok((Some(activation), None))
        }
        (None, Some(result)) => {
            let ticket = reader.resume_ticket(&child_run_id).ok_or_else(|| {
                AgentRunError::Configuration(format!(
                    "child Run {:?} is not awaiting",
                    child_run_id.0
                ))
            })?;
            if ticket.delegation_origin.as_ref() != Some(&origin) {
                return Err(AgentRunError::Configuration(
                    "child Run origin does not match its parent relationship".to_string(),
                ));
            }
            let mut activation = RunActivation::new(
                child_run_id.clone(),
                thread_id.clone(),
                config.clone(),
                Vec::new(),
            );
            activation.delegation_origin = Some(origin);
            activation = activation.with_inherited_toolsets(context.inherited_toolsets.clone());
            Ok((
                Some(activation),
                Some(ResumeCommand::from_ticket(&ticket, result, 0)),
            ))
        }
        _ => {
            return Err(AgentRunError::Configuration(
                "a child Run boundary requires exactly one of seed or resume".to_string(),
            ));
        }
    }?;
    // Both scheduling modes execute on the same owned task boundary. Polling
    // a complete child Worker inside its parent can exhaust the default stack;
    // dropping the parent must abort that execution instead of detaching it.
    // Queue claims, committed readers and cancellation authority stay unchanged.
    AbortOnDropTask::spawn(async move {
        let mut boundary_reader = reader.clone();
        let state = if let Some(scheduler) = scheduler {
            // The foreground child scheduler and every later ownership operation
            // retain this one edge clock. The Worker never substitutes another time
            // source after the queue grants the claim.
            let clock: Arc<dyn WallClock> = scheduler.clock.clone();
            let runtime = Arc::new(runtime);
            let attempt_executor = child_attempt_executor(
                runtime.clone(),
                &config,
                &adapters,
                authorization.policy.clone(),
            )?;
            // The sole child assembly owns the local/remote reader choice and
            // installs the child's exact claim authority before either start or
            // resume enters the durable queue.
            let (worker, worker_reader) = super::ChildWorkerFactory::parent_execution(
                runtime,
                &scheduler,
                reader.clone(),
                context.clone(),
                adapters.credential_capabilities.clone(),
                attempt_executor,
            );
            boundary_reader = worker_reader.clone();
            Ok::<_, AgentRunError>(match operation {
                (Some(activation), None) => {
                    // Stable child identity reconnects to committed truth. Only a
                    // genuinely new child is scheduled; retries never create another.
                    match reader.run_state(&child_run_id) {
                        Some(state @ (RunState::Awaiting | RunState::Ended(_))) => state,
                        _ => {
                            let request = child_dispatch_request(
                                activation,
                                parent_thread_id.clone(),
                                scheduler.session_resources.clone(),
                                scheduler.publication_source.as_deref(),
                            )?;
                            match worker.start_run(request, clock.clone()).await.map_err(
                                |error| {
                                    AgentRunError::Runtime(
                                        awaken_runtime_contract::execution::Error::Execution(
                                            error.to_string(),
                                        ),
                                    )
                                },
                            )? {
                                Some((_, state)) => state,
                                None => {
                                    await_committed_child_boundary(
                                        boundary_reader.as_ref(),
                                        &child_run_id,
                                        context.cancellation.as_ref(),
                                    )
                                    .await?
                                }
                            }
                        }
                    }
                }
                (Some(_), Some(command)) => {
                    let ticket = reader.resume_ticket(&child_run_id).ok_or_else(|| {
                        AgentRunError::Configuration(format!(
                            "child Run {:?} is not awaiting",
                            child_run_id.0
                        ))
                    })?;
                    let input = PendingInput {
                        message_id: format!(
                            "child-resume:{}:{}",
                            child_run_id.0, ticket.correlation_id
                        ),
                        run_id: child_run_id.clone(),
                        thread_id: ticket.thread_id.clone(),
                        correlation_id: ticket.correlation_id,
                        available_at_ms: None,
                        context_messages: Vec::new(),
                        result: command.result,
                    };
                    let resumed =
                        worker
                            .resume_run(input, clock.clone())
                            .await
                            .map_err(|error| {
                                AgentRunError::Runtime(
                                    awaken_runtime_contract::execution::Error::Execution(
                                        error.to_string(),
                                    ),
                                )
                            })?;
                    match resumed {
                        Some((_, state)) => state,
                        None => {
                            await_committed_child_boundary(
                                boundary_reader.as_ref(),
                                &child_run_id,
                                context.cancellation.as_ref(),
                            )
                            .await?
                        }
                    }
                }
                _ => unreachable!("operation construction is exhaustive"),
            })
        } else {
            let runtime = Arc::new(runtime);
            let attempt_executor =
                child_attempt_executor(runtime, &config, &adapters, authorization.policy.clone())?;
            match operation {
                (Some(activation), None) => {
                    let context = bind_direct_child_credentials(&activation, context, &adapters)?;
                    attempt_executor
                        .execute(activation, context)
                        .await
                        .map_err(AgentRunError::Runtime)
                }
                (Some(activation), Some(command)) => {
                    let context = bind_direct_child_credentials(&activation, context, &adapters)?;
                    attempt_executor
                        .resume(activation, command, context)
                        .await
                        .map_err(AgentRunError::Runtime)
                }
                _ => unreachable!("operation construction is exhaustive"),
            }
        }?;
        settled_agent_boundary(boundary_reader.as_ref(), &thread_id, state)
    })
    .join()
    .await
    .map_err(|error| {
        AgentRunError::Runtime(awaken_runtime_contract::execution::Error::Execution(
            format!("delegated child task failed: {error}"),
        ))
    })?
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::awaiting::ResumeTicket;
    use awaken_agent_contract::agent::delegation::DelegationOrigin;
    use awaken_agent_contract::agent::message::{Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Record as RunRecord, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    // Lifecycle and recovery fixtures: no deadline, so wall time.
    use awaken_runtime_contract::CancellationToken;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
    };
    use awaken_runtime_contract::resume::ResumeResult;
    use awaken_runtime_contract::runtime_context::RuntimeRunContext;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
    use awaken_sandbox_local::LocalProvider;
    use awaken_store_inmem::MemoryCommitCoordinator;
    use awaken_tool_contract::tool::{RawToolRegistry, ToolExecutor};

    use super::{
        ChildExecutionAdapters, ChildRunRequest, RunScheduler, await_committed_child_boundary,
        run_configured_agent_until_boundary,
    };
    use crate::agent_catalog::AgentCatalog;
    use crate::agent_runner::AgentRunBoundary;
    use crate::agent_runner::fixtures::user;
    use crate::agent_runner::root_execution::{
        AgentExecution, AgentRunSandbox, run_agent_until_boundary, run_configured_agent,
    };
    use crate::config::server_config;

    #[tokio::test]
    async fn child_without_host_clock_refuses_before_fresh_provider_effect() {
        // Clock composition table C1: configured child + commit/history but no
        // existing Host clock -> typed MissingClock and zero provider/model I/O.
        // C2 a supplied clock uses the ordinary current-executor cases below;
        // their store, scheduler and Runtime all share that same fixture Arc.
        struct MustNotInfer;
        #[async_trait::async_trait]
        impl LlmExecutor for MustNotInfer {
            async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
                panic!("C1 missing clock must reject before model I/O");
            }
        }
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("must-not-materialize");
        let provider = LocalProvider::new(&root);
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(123));
        let commit = Arc::new(MemoryCommitCoordinator::new(clock));
        let config = crate::agent_runner::fixtures::agent("child-clock", "clock");
        let result = run_configured_agent_until_boundary(
            &config,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(MustNotInfer),
            RunId("child-clock".into()),
            DelegationOrigin::root_for_agent(RunId("parent".into()), "call", "parent"),
            Some(vec![user("go")].into()),
            None,
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit),
            None,
            ThreadId("parent".into()),
            None,
            ChildExecutionAdapters::default(),
        )
        .await;
        match result {
            Err(crate::agent_runner::AgentRunError::Runtime(error)) => {
                assert_eq!(error.code(), "missing_clock", "C1 typed refusal");
            }
            _ => panic!("C1 must preserve the existing typed configuration failure"),
        }
        assert!(!root.exists(), "C1 zero Fresh provider effect");
    }

    struct EventuallySettledReader {
        reads: AtomicUsize,
    }

    impl CommittedThreadView for EventuallySettledReader {
        fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
            Vec::new()
        }

        fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
            None
        }

        fn open_wait_for_thread(&self, _thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
            None
        }

        fn run(&self, _run_id: &RunId) -> Option<RunRecord> {
            None
        }

        fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
            None
        }

        fn run_state(&self, _run_id: &RunId) -> Option<RunState> {
            if self.reads.fetch_add(1, Ordering::SeqCst) < 2 {
                Some(RunState::Running)
            } else {
                Some(RunState::Awaiting)
            }
        }
    }

    #[tokio::test]
    async fn losing_a_child_claim_waits_for_the_winners_committed_boundary() {
        let reader = EventuallySettledReader {
            reads: AtomicUsize::new(0),
        };
        let state = await_committed_child_boundary(&reader, &RunId("child-race".to_string()), None)
            .await
            .expect("the competing worker settles the child");

        assert_eq!(state, RunState::Awaiting);
        assert!(reader.reads.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn waiting_for_a_competing_child_claim_observes_parent_cancellation() {
        struct RunningReader;

        impl CommittedThreadView for RunningReader {
            fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
                Vec::new()
            }

            fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
                None
            }

            fn open_wait_for_thread(&self, _thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
                None
            }

            fn run(&self, _run_id: &RunId) -> Option<RunRecord> {
                None
            }

            fn latest_run(&self, _thread_id: &ThreadId) -> Option<RunRecord> {
                None
            }

            fn run_state(&self, _run_id: &RunId) -> Option<RunState> {
                Some(RunState::Running)
            }
        }

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = await_committed_child_boundary(
            &RunningReader,
            &RunId("child-cancelled".to_string()),
            Some(&cancellation),
        )
        .await
        .expect_err("a cancelled parent stops waiting for its child");

        assert!(error.to_string().contains("was cancelled"));
    }

    #[tokio::test]
    async fn child_runs_bind_the_current_sandbox_and_selected_web_plugin() {
        // Causes: C1 execution enters the boundary or auxiliary child path; C2
        // placement is Shared, SharedLocal, or Fresh; C3 the child WebFetch
        // configuration is absent, valid, malformed, or incompatible with a
        // provider-server realization; C4 the parent context carries an executor
        // for a different sandbox; C5 the frozen child selects the WebFetch
        // plugin; C6 the Host supplies its configured WebFetch adapter. Effects:
        // E1 a successful child writes only through
        // its current placement executor; E2 the parent's executor is not
        // inherited; E3 selected malformed child configuration
        // fails before model/tool effects; E4 a selected WebFetch plugin resolves
        // from the Host adapter; E5 a missing selected adapter fails before
        // inference; E6 an incompatible provider-server policy fails through the
        // same configured plugin before inference.
        // Constraints: K1 the current sandbox is the sole child Hand authority;
        // K2 WebFetch policy belongs to the configured plugin, never the Hand;
        // K3 direct and recovered Native children use one plugin composition owner.
        //
        // | Rule | Child path | Placement | Fetch selected/adapter | Effect |
        // | R1 | boundary | Shared | no/either | E1+E2 current Shared executor |
        // | R2 | auxiliary | SharedLocal | no/either, inert policy | E1+E2 current executor |
        // | R3 | auxiliary | Fresh | no/either | E1+E2 fresh executor/root |
        // | R4 | boundary | SharedLocal | yes/yes, malformed policy | E3 fail closed |
        // | R5 | boundary | Shared | yes/yes | E1+E4 configured plugin |
        // | R6 | boundary | Shared | yes/no | E5 fail closed |
        // | R7 | boundary | Shared | provider-server/restrictive policy | E6 capability-bound before inference |
        use awaken_agent_contract::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };

        struct SandboxWriteModel {
            file_name: String,
            inferences: Arc<AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl LlmExecutor for SandboxWriteModel {
            async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
                self.inferences.fetch_add(1, Ordering::SeqCst);
                let output = if request
                    .messages
                    .iter()
                    .any(|message| message.role == Role::Tool)
                {
                    AssistantOutput::text("child done")
                } else {
                    AssistantOutput::from_tool_calls(vec![awaken_tool_contract::tool::ToolCall {
                        call_id: format!("write-{}", self.file_name),
                        tool_id: "write".into(),
                        arguments: serde_json::json!({
                            "file_path": self.file_name,
                            "content": "current sandbox"
                        }),
                    }])
                };
                Ok(ChatResponse {
                    output,
                    usage: None,
                    stop_reason: None,
                })
            }
        }

        fn child_config(configuration: Option<serde_json::Value>) -> ExecutableAgentSnapshot {
            let allow = ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAllow,
            };
            let mut overrides = vec![ToolPolicyOverride::new("write", allow)];
            if let Some(configuration) = configuration {
                overrides.push(ToolPolicyOverride::with_optional_configuration(
                    "web_fetch",
                    allow,
                    Some(configuration),
                ));
            }
            let selected_web_fetch = overrides.iter().any(|item| item.name == "web_fetch");
            let plugin_ids = if selected_web_fetch {
                vec![awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string()]
            } else {
                Vec::new()
            };
            let plugin_config = if selected_web_fetch {
                std::collections::BTreeMap::from([(
                    awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string(),
                    awaken_ext_builtin_tools::WebFetchPlugin::default_config(),
                )])
            } else {
                std::collections::BTreeMap::new()
            };
            let mut config = server_config(
                "assistant",
                "stub",
                &HashSet::new(),
                &HashSet::new(),
                &plugin_ids,
                &plugin_config,
                &[],
                awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
            )
            .expect("literal model reference");
            config.resolved_spec.plugin_config.agent.toolsets = vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides,
            }];
            config
                .recompute_fingerprint()
                .expect("test child configuration remains coherent");
            config
        }

        fn select_web_fetch(mut config: ExecutableAgentSnapshot) -> ExecutableAgentSnapshot {
            config
                .resolved_spec
                .plugin_ids
                .push(awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string());
            config
                .recompute_fingerprint()
                .expect("selected child WebFetch configuration remains coherent");
            config
        }

        let tmp = tempfile::tempdir().unwrap();
        let parent = LocalProvider::new(tmp.path().join("parent"))
            .create_sandbox(&crate::sandbox_projection::agent_run_sandbox_spec(
                "policy-owner",
            ))
            .await
            .unwrap();
        let parent_path = parent.workspace_path().to_path_buf();
        let parent_executor: Arc<dyn ToolExecutor> =
            Arc::new(RawToolRegistry::new(parent.rooted_tools()));
        let clock = awaken_time::system_clock();
        let context = || {
            let commit = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit)
                .with_tool_executor(parent_executor.clone())
                .with_inherited_toolsets(Some(
                    awaken_agent_contract::toolset::InheritedToolsets::with_caller(
                        Vec::new(),
                        None,
                    ),
                ))
        };
        let model = |file_name: &str, inferences: Arc<AtomicUsize>| {
            Arc::new(SandboxWriteModel {
                file_name: file_name.to_string(),
                inferences,
            }) as Arc<dyn LlmExecutor>
        };
        let web_adapters = || ChildExecutionAdapters {
            clock: Some(clock.clone()),
            web_fetch: Some(Arc::new(awaken_ext_builtin_tools::WebFetchPlugin::new(
                awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
                None,
            ))),
            ..Default::default()
        };

        let shared_sandbox = LocalProvider::new(tmp.path().join("shared"))
            .create_sandbox(&crate::sandbox_projection::agent_run_sandbox_spec(
                "current",
            ))
            .await
            .unwrap();
        let shared_path = shared_sandbox.workspace_path().to_path_buf();
        let shared_environment =
            awaken_session_environment::SandboxEnvironment::workdir(shared_sandbox)
                .into_session_environment();
        let shared_inferences = Arc::new(AtomicUsize::new(0));
        let shared_config = child_config(None);
        let shared = run_configured_agent_until_boundary(
            &shared_config,
            AgentRunSandbox::Shared(&shared_environment),
            model("shared.txt", shared_inferences.clone()),
            RunId("shared-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-shared".into()),
                "call-shared",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            ChildExecutionAdapters {
                clock: Some(clock.clone()),
                ..Default::default()
            },
        )
        .await
        .expect("R1 child reaches a terminal boundary");
        assert!(matches!(shared, AgentRunBoundary::Ended { .. }), "R1/E1");
        assert!(shared_path.join("shared.txt").is_file(), "R1/E1");
        assert_eq!(shared_inferences.load(Ordering::SeqCst), 2, "R1/E1");

        let shared_local = LocalProvider::new(tmp.path().join("shared-local"))
            .create_sandbox(&crate::sandbox_projection::agent_run_sandbox_spec(
                "current",
            ))
            .await
            .unwrap();
        let shared_local_path = shared_local.workspace_path().to_path_buf();
        let valid = serde_json::json!({"type":"web_fetch", "max_content_tokens":3});
        let shared_local_config = child_config(Some(valid));
        let shared_local_inferences = Arc::new(AtomicUsize::new(0));
        run_configured_agent_until_boundary(
            &shared_local_config,
            AgentRunSandbox::SharedLocal(&shared_local),
            model("shared-local.txt", shared_local_inferences.clone()),
            RunId("shared-local-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-shared-local".into()),
                "call-shared-local",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            web_adapters(),
        )
        .await
        .expect("R2 child uses its configured plugin and current Hand executor");
        assert!(
            shared_local_path.join("shared-local.txt").is_file(),
            "R2/E1"
        );
        assert_eq!(shared_local_inferences.load(Ordering::SeqCst), 2, "R2/E1");

        let fresh_base = tmp.path().join("fresh");
        let fresh_provider = LocalProvider::new(&fresh_base);
        let plain_catalog = AgentCatalog::new().with_agent(child_config(None));
        let fresh_inferences = Arc::new(AtomicUsize::new(0));
        run_configured_agent(
            &plain_catalog,
            AgentRunSandbox::Fresh(&fresh_provider),
            model("fresh.txt", fresh_inferences.clone()),
            clock.clone(),
            "assistant",
            "fresh-current",
            vec![user("write")],
            Vec::new(),
            None,
            Some(context()),
            None,
        )
        .await
        .expect("R3 child creates and uses its fresh executor");
        assert!(
            fresh_base.join("fresh-current/fresh.txt").is_file(),
            "R3/E1"
        );
        assert_eq!(fresh_inferences.load(Ordering::SeqCst), 2, "R3/E1");

        let selected_config = select_web_fetch(child_config(None));
        let selected_inferences = Arc::new(AtomicUsize::new(0));
        let selected = run_configured_agent_until_boundary(
            &selected_config,
            AgentRunSandbox::Shared(&shared_environment),
            model("selected-web-fetch.txt", selected_inferences.clone()),
            RunId("selected-web-fetch-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-selected-web-fetch".into()),
                "call-selected-web-fetch",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            ChildExecutionAdapters {
                clock: Some(clock.clone()),
                web_fetch: Some(Arc::new(awaken_ext_builtin_tools::WebFetchPlugin::new(
                    awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
                    None,
                ))),
                ..Default::default()
            },
        )
        .await
        .expect("R5 selected WebFetch resolves through the Host adapter");
        assert!(matches!(selected, AgentRunBoundary::Ended { .. }), "R5/E4");
        assert!(
            shared_path.join("selected-web-fetch.txt").is_file(),
            "R5/E1"
        );
        assert_eq!(selected_inferences.load(Ordering::SeqCst), 2, "R5/E4");

        let missing_inferences = Arc::new(AtomicUsize::new(0));
        let missing = match run_configured_agent_until_boundary(
            &selected_config,
            AgentRunSandbox::Shared(&shared_environment),
            model("missing-web-fetch.txt", missing_inferences.clone()),
            RunId("missing-web-fetch-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-missing-web-fetch".into()),
                "call-missing-web-fetch",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            ChildExecutionAdapters {
                clock: Some(clock.clone()),
                ..Default::default()
            },
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("R6 selected WebFetch without its adapter must fail closed"),
        };
        assert!(
            missing.to_string().contains("no WebFetch adapter"),
            "R6/E5: {missing}"
        );
        assert_eq!(missing_inferences.load(Ordering::SeqCst), 0, "R6/E5");
        assert!(!shared_path.join("missing-web-fetch.txt").exists(), "R6/E5");

        let mut provider_server_config = select_web_fetch(child_config(Some(
            serde_json::json!({"type": "web_fetch", "max_content_tokens": 3}),
        )));
        provider_server_config.resolved_spec.plugin_config.insert(
            awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string(),
            serde_json::json!({"provider_id": "openrouter", "options": {}}),
        );
        provider_server_config
            .recompute_fingerprint()
            .expect("provider-server child configuration remains coherent");
        let provider_server_inferences = Arc::new(AtomicUsize::new(0));
        let provider_server = match run_configured_agent_until_boundary(
            &provider_server_config,
            AgentRunSandbox::Shared(&shared_environment),
            model(
                "provider-server-web-fetch.txt",
                provider_server_inferences.clone(),
            ),
            RunId("provider-server-web-fetch-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-provider-server-web-fetch".into()),
                "call-provider-server-web-fetch",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            ChildExecutionAdapters {
                clock: Some(clock.clone()),
                web_fetch: Some(Arc::new(awaken_ext_builtin_tools::WebFetchPlugin::new(
                    awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
                    None,
                ))),
                ..Default::default()
            },
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("R7 provider-server child policy must fail closed"),
        };
        assert!(
            provider_server
                .to_string()
                .contains("Error(CapabilityBound)"),
            "R7/E6: {provider_server}"
        );
        assert_eq!(
            provider_server_inferences.load(Ordering::SeqCst),
            0,
            "R7/E6"
        );
        assert!(
            !shared_path.join("provider-server-web-fetch.txt").exists(),
            "R7/E6"
        );

        let malformed_sandbox = LocalProvider::new(tmp.path().join("malformed"))
            .create_sandbox(&crate::sandbox_projection::agent_run_sandbox_spec(
                "current",
            ))
            .await
            .unwrap();
        let malformed_path = malformed_sandbox.workspace_path().to_path_buf();
        let malformed_inferences = Arc::new(AtomicUsize::new(0));
        let malformed_config = select_web_fetch(child_config(Some(serde_json::json!({
            "type": "web_fetch",
            "unexpected": true
        }))));
        let error = match run_configured_agent_until_boundary(
            &malformed_config,
            AgentRunSandbox::SharedLocal(&malformed_sandbox),
            model("malformed.txt", malformed_inferences.clone()),
            RunId("malformed-child".into()),
            DelegationOrigin::root_for_agent(
                RunId("parent-malformed".into()),
                "call-malformed",
                "parent",
            ),
            Some(vec![user("write")].into()),
            None,
            context(),
            None,
            ThreadId("parent-thread".into()),
            None,
            web_adapters(),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("R4 malformed child configuration must fail closed"),
        };
        assert!(
            error
                .to_string()
                .starts_with("unexpected [invalid_web_configuration_shape]:"),
            "R4/E3: {error}"
        );
        assert_eq!(malformed_inferences.load(Ordering::SeqCst), 0, "R4/E3");
        assert!(!malformed_path.join("malformed.txt").exists(), "R4/E3");

        for file in [
            "shared.txt",
            "shared-local.txt",
            "fresh.txt",
            "selected-web-fetch.txt",
            "missing-web-fetch.txt",
            "provider-server-web-fetch.txt",
            "malformed.txt",
        ] {
            assert!(
                !parent_path.join(file).exists(),
                "E2 parent leakage: {file}"
            );
        }
    }

    #[tokio::test]
    async fn dropping_parent_aborts_direct_and_durable_child_execution() {
        // C1 direct/durable scheduling; C2 the real child reaches model inference;
        // C3 its parent future is dropped. R1/R2 require the model future's Drop
        // receipt and no successful terminal result. A Notify/oneshot handshake
        // proves the execution seam; no yield-count or sleep guesses entry.
        // This kills detached JoinHandle behavior in either scheduling branch.
        struct Dropped(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        struct Blocked {
            entered: tokio::sync::Notify,
            dropped: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        }
        #[async_trait::async_trait]
        impl LlmExecutor for Blocked {
            async fn infer(&self, _: ChatRequest) -> LlmResult<ChatResponse> {
                let _receipt = Dropped(self.dropped.lock().unwrap().take());
                self.entered.notify_one();
                std::future::pending().await
            }
        }
        for durable in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let provider = LocalProvider::new(tmp.path());
            let sandbox = provider
                .create_sandbox(&crate::sandbox_projection::agent_run_sandbox_spec(
                    "cancel-parent",
                ))
                .await
                .unwrap();
            let clock = awaken_time::system_clock();
            let commit = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
            let scheduler = if durable {
                Some(RunScheduler {
                    clock: clock.clone(),
                    binding: awaken_dispatch_erasure::DispatchBinding::local(Arc::new(
                        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                            .await
                            .unwrap(),
                    )),
                    commit: commit.clone(),
                    reader: commit.clone(),
                    owner: "child-worker".into(),
                    session_resources: None,
                    publication_source: None,
                    claimed_model_request_gate: None,
                })
            } else {
                None
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let model = Arc::new(Blocked {
                entered: tokio::sync::Notify::new(),
                dropped: std::sync::Mutex::new(Some(tx)),
            });
            let delegates = HashSet::new();
            let execution = AgentExecution {
                agent_id: "worker",
                model_ref: "default",
                delegates: &delegates,
                run_delegation: None,
                context: Some(
                    RuntimeRunContext::new()
                        .with_commit(commit.clone())
                        .with_reader(commit.clone()),
                ),
                scheduler,
                toolsets: &[],
            };
            let run_id = RunId("cancel-child".into());
            let mut parent = Box::pin(run_agent_until_boundary(
                model.clone(),
                clock.clone(),
                execution,
                AgentRunSandbox::SharedLocal(&sandbox),
                ChildRunRequest {
                    run_id: run_id.clone(),
                    origin: DelegationOrigin::root_for_agent(
                        RunId("parent".into()),
                        "call",
                        "parent",
                    ),
                    seed: Some(vec![user("block")].into()),
                    resume: None,
                    parent_thread_id: ThreadId("parent-thread".into()),
                },
            ));
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::select! { _ = model.entered.notified() => {}, _ = &mut parent => panic!("C2 must remain in inference") }
            }).await.expect("C2 real model entered");
            drop(parent);
            tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .expect("R1/R2 child was not detached")
                .unwrap();
            assert!(
                !matches!(
                    commit.run_state(&run_id),
                    Some(RunState::Ended(EndCause::NaturalEnd))
                ),
                "no fabricated successful completion"
            );
        }
    }

    /// A delegated child reaches the same typed HITL boundary as a root Run. The
    /// parent-facing coordinator resumes it with a `ResumeResult`; no child-only
    /// continuation language or automatic permission bypass exists.
    #[tokio::test]
    async fn durable_child_permission_recovers_and_duplicate_resume_is_idempotent() {
        // Causes: C0 the child publication explicitly sets write=always_ask;
        // C1 a local durable child has a Session-wide committed reader;
        // C2 its inherited parent reader contains no child state; C3 the child is
        // new, Awaiting, resumed, or terminal on replay. Effects: E1 every child
        // state/ticket comes from C1; E2 duplicate start reconnects without a
        // second seed; E3 exact resume ends once; E4 terminal replay is a stutter.
        // Constraints: K1 SessionCommit is the sole local committed authority; K2 a
        // parent claim projection cannot own a child Thread; K3 the queue/claim
        // remains the sole execution fence; K4 no second read model is introduced.
        // Decision table: R1=C0+C1+C2+new=>Awaiting; R2=R1+duplicate=>E2;
        // R3=R1+resume=>E3; R4=E3+replay=>E4.
        // C4 default test-thread stack, with no RUST_MIN_STACK override: all
        // four rules exercise the durable owned-task boundary. Restoring direct
        // Worker polling must reproduce stack overflow, not merely an assertion.
        struct PermissionModel;

        #[async_trait::async_trait]
        impl LlmExecutor for PermissionModel {
            async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
                let output = if request.messages.iter().any(|m| m.role == Role::Tool) {
                    AssistantOutput::text("child done")
                } else {
                    AssistantOutput::from_tool_calls(vec![awaken_tool_contract::tool::ToolCall {
                        call_id: "child-write".into(),
                        tool_id: "write".into(),
                        arguments: serde_json::json!({
                            "path": "child.txt",
                            "content": "from child"
                        }),
                    }])
                };
                Ok(ChatResponse {
                    output,
                    usage: None,
                    stop_reason: None,
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let sandbox = provider
            .create_sandbox(&crate::sandbox_projection::agent_run_sandbox_spec("parent"))
            .await
            .unwrap();
        let clock = awaken_time::system_clock();
        let commit = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
        let dispatch_path = tmp.path().join("child-dispatch.db");
        let dispatch_path = dispatch_path.to_string_lossy().to_string();
        let scheduler = RunScheduler {
            clock: clock.clone(),
            binding: awaken_dispatch_erasure::DispatchBinding::local(Arc::new(
                awaken_dispatch::open_sqlite_dispatch_store(&dispatch_path, clock.clone())
                    .await
                    .expect("child dispatch store"),
            )),
            commit: commit.clone(),
            reader: commit.clone(),
            owner: "replacement-worker".to_string(),
            session_resources: None,
            publication_source: None,
            claimed_model_request_gate: None,
        };
        let parent_reader: Arc<dyn CommittedThreadView> =
            Arc::new(MemoryCommitCoordinator::new(clock.clone()));
        let context = || {
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(parent_reader.clone())
        };
        let child_run_id = RunId("child-run-1".into());
        let origin = DelegationOrigin::root_for_agent(
            RunId("parent-run-1".into()),
            "delegate-call-1",
            "parent-agent",
        );
        let no_delegates = HashSet::new();
        let permission_toolsets = vec![crate::config::test_agent_toolset_permission(
            "write",
            awaken_agent_contract::ToolPermissionRequirement::AlwaysAsk,
        )];
        let execution = |scheduler: RunScheduler| AgentExecution {
            agent_id: "worker",
            model_ref: "default",
            delegates: &no_delegates,
            run_delegation: None,
            context: Some(context()),
            scheduler: Some(scheduler),
            toolsets: &permission_toolsets,
        };

        let first = run_agent_until_boundary(
            Arc::new(PermissionModel),
            clock.clone(),
            execution(scheduler),
            AgentRunSandbox::SharedLocal(&sandbox),
            ChildRunRequest {
                run_id: child_run_id.clone(),
                origin: origin.clone(),
                seed: Some(vec![user("write the file")].into()),
                resume: None,
                parent_thread_id: ThreadId("parent-thread".to_string()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(first, AgentRunBoundary::Awaiting));
        let ticket = commit.resume_ticket(&child_run_id).expect("child ticket");
        assert_eq!(
            ticket.reason(),
            awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission
        );
        assert_eq!(ticket.call_id(), Some("child-write"));
        assert_eq!(ticket.delegation_origin.as_ref(), Some(&origin));

        // A replacement process reopens the durable queue and rebuilds every live
        // runtime object from committed truth; no child handle crosses this seam.
        let replacement = RunScheduler {
            clock: clock.clone(),
            binding: awaken_dispatch_erasure::DispatchBinding::local(Arc::new(
                awaken_dispatch::open_sqlite_dispatch_store(&dispatch_path, clock.clone())
                    .await
                    .expect("reopen child dispatch"),
            )),
            commit: commit.clone(),
            reader: commit.clone(),
            owner: "replacement-worker-2".to_string(),
            session_resources: None,
            publication_source: None,
            claimed_model_request_gate: None,
        };

        let recovered_boundary = run_agent_until_boundary(
            Arc::new(PermissionModel),
            clock.clone(),
            execution(replacement.clone()),
            AgentRunSandbox::SharedLocal(&sandbox),
            ChildRunRequest {
                run_id: child_run_id.clone(),
                origin: origin.clone(),
                seed: Some(vec![user("write the file")].into()),
                resume: None,
                parent_thread_id: ThreadId("parent-thread".to_string()),
            },
        )
        .await
        .expect("duplicate start reconnects to the awaiting child");
        assert!(matches!(recovered_boundary, AgentRunBoundary::Awaiting));
        assert_eq!(
            commit
                .committed()
                .messages
                .iter()
                .filter(|message| message.role == Role::User)
                .count(),
            1,
            "recovery start does not duplicate the child seed"
        );

        let second = run_agent_until_boundary(
            Arc::new(PermissionModel),
            clock.clone(),
            execution(replacement),
            AgentRunSandbox::SharedLocal(&sandbox),
            ChildRunRequest {
                run_id: child_run_id.clone(),
                origin: origin.clone(),
                seed: None,
                resume: Some(ResumeResult::allow()),
                parent_thread_id: ThreadId("parent-thread".to_string()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            second,
            AgentRunBoundary::Ended { ref text, .. } if text == "child done"
        ));

        // Simulate a replacement process that did not observe the returned child
        // result before crashing. The child is already terminal and its ticket is
        // gone; replaying the same resume must return committed truth without
        // entering the model or appending a duplicate assistant message.
        let replay = run_agent_until_boundary(
            Arc::new(PermissionModel),
            clock.clone(),
            execution(RunScheduler {
                clock: clock.clone(),
                binding: awaken_dispatch_erasure::DispatchBinding::local(Arc::new(
                    awaken_dispatch::open_sqlite_dispatch_store(&dispatch_path, clock.clone())
                        .await
                        .expect("reopen terminal child dispatch"),
                )),
                commit: commit.clone(),
                reader: commit.clone(),
                owner: "replacement-worker-3".to_string(),
                session_resources: None,
                publication_source: None,
                claimed_model_request_gate: None,
            }),
            AgentRunSandbox::SharedLocal(&sandbox),
            ChildRunRequest {
                run_id: RunId("child-run-1".into()),
                origin: DelegationOrigin::root_for_agent(
                    RunId("parent-run-1".into()),
                    "delegate-call-1",
                    "parent-agent",
                ),
                seed: None,
                resume: Some(ResumeResult::allow()),
                parent_thread_id: ThreadId("parent-thread".to_string()),
            },
        )
        .await
        .expect("terminal child replay is idempotent");
        assert!(matches!(
            replay,
            AgentRunBoundary::Ended { ref text, .. } if text == "child done"
        ));
        assert_eq!(
            commit
                .committed()
                .messages
                .iter()
                .filter(|message| message.text_content() == "child done")
                .count(),
            1,
            "replacement resume does not duplicate the child result"
        );
    }
}
