//! Constructing and running an Agent through the ordinary Run lifecycle.
//!
//! Native delegation (`agent_run`), the goal judge, and skill forks all use this
//! substrate. A delegated Agent receives a first-class child Run identity, the
//! same durable context and capabilities as a directly initiated Agent, and the
//! same delegation executor, so nested delegation is not a special execution
//! path. Auxiliary Agents may deliberately request transient identity and an
//! isolated context because their work is outside the user-visible Run tree.
//!
//! [`run_configured_agent`] is the parameterized form: it resolves the Agent's
//! own `ExecutableAgentSnapshot` (instructions, model, tools) from an [`AgentCatalog`] by
//! id. [`run_agent`] is the thin wrapper that runs the default
//! `assistant` config with a plain string prompt.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::delegation::DelegationOrigin;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_run_ingress::{
    AnyDispatchStore, ClaimedRunCommit, Clock, DispatchWorker, ModelAccessRef, PendingInput,
    RunDispatch, SystemClock,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{DirectRunIngress, RunInput, RunService};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::delegation::RunDelegationService;
use awaken_runtime_contract::llm::{LlmExecutor, ThreadUsage};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::{LocalProvider, LocalSandbox};

use crate::agent_catalog::AgentCatalog;
use crate::config::{build_runtime, latest_assistant_text, server_config};

/// Where an Agent Run's tools execute. A delegated Agent shares the initiating
/// Agent's sandbox by default, or receives a fresh sandbox when placement policy
/// requests isolation. Sandbox placement does not alter Run semantics.
pub(crate) enum AgentRunSandbox<'a> {
    /// Reuse the parent agent's live sandbox — the default (`与主 agent 共用`).
    Shared(&'a LocalSandbox),
    /// Create a fresh, isolated sandbox for this Agent Run via the given provider.
    Fresh(&'a LocalProvider),
}

/// Identity used by an auxiliary Agent Run. Delegated children no longer use a
/// host-specific identity wrapper: they are ordinary Runs carrying a typed
/// `DelegationOrigin` in their activation.
pub(crate) struct AgentRunIdentity<'a> {
    thread: &'a str,
}

/// Runtime capabilities of an Agent regardless of who initiated its Run.
/// A child Run receives the target Agent's own delegation interface and roster;
/// initiation never copies capabilities from the parent Agent.
pub(crate) struct AgentExecution<'a> {
    pub(crate) agent_id: &'a str,
    pub(crate) model_ref: &'a str,
    pub(crate) delegates: &'a HashSet<String>,
    pub(crate) run_delegation: Option<Arc<dyn RunDelegationService>>,
    pub(crate) context: Option<RuntimeRunContext>,
    pub(crate) scheduler: Option<RunScheduler>,
}

/// Durable scheduling dependencies for a child Run. This is host wiring, not a
/// child-specific runtime contract: the same dispatch worker and queue drive root
/// and child Runs; creation is the only difference.
#[derive(Clone)]
pub(crate) struct RunScheduler {
    pub(crate) store: Arc<AnyDispatchStore>,
    /// Unfenced session commit authority. The child worker binds its own
    /// `RunClaim`; inheriting the parent's already-fenced coordinator would nest
    /// two claim guards and deadlock the shared dispatch authority.
    pub(crate) commit: Arc<dyn CommitCoordinator>,
    /// Read side of the same committed authority. Kept separately after trait
    /// erasure so cancellation/recovery can make idempotency decisions from
    /// committed child state after a process restart.
    pub(crate) reader: Arc<dyn ThreadReader>,
    pub(crate) owner: String,
    pub(crate) claimed_commit: Option<Arc<dyn ClaimedRunCommit>>,
    pub(crate) model_access: Option<Arc<ModelAccessResolver>>,
}

type ModelAccessResolver =
    dyn Fn(&RunActivation) -> Result<Option<ModelAccessRef>, String> + Send + Sync;

fn child_dispatch_request(
    activation: RunActivation,
    parent_thread_id: ThreadId,
    model_access: Option<&Arc<ModelAccessResolver>>,
) -> Result<RunDispatch, AgentRunError> {
    let access = model_access
        .map(|resolve| resolve(&activation))
        .transpose()
        .map_err(AgentRunError::Configuration)?
        .flatten();
    let mut request = RunDispatch::new(activation)
        .for_session(parent_thread_id)
        .with_traceparent(awaken_observability::current_traceparent());
    if let Some(access) = access {
        request = request.with_model_access(access);
    }
    Ok(request)
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentRunError {
    #[error("{0}")]
    Configuration(String),
    #[error("{0}")]
    Provisioning(String),
    #[error(transparent)]
    Runtime(awaken_runtime_contract::execution::Error),
}

/// One ordinary lifecycle boundary reached by an Agent Run.
///
/// Delegated and directly admitted Runs use the same `Runtime::execute` /
/// `Runtime::resume` transitions. The parent only observes whether its child
/// ended or is awaiting; the child's committed `ResumeTicket` remains the sole
/// authority for what may resume it.
pub(crate) enum AgentRunBoundary {
    Ended { text: String, usage: ThreadUsage },
    Awaiting,
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

impl<'a> AgentRunIdentity<'a> {
    pub(crate) fn transient(thread: &'a str) -> Self {
        Self { thread }
    }
}

/// Reconnect a foreground parent to a child that another dispatch worker won.
///
/// The durable queue deliberately permits the background pool and a foreground
/// parent to race for the same stable child id. Losing that race is not a Run
/// failure: the winner owns execution and the parent observes the resulting
/// committed boundary. This path is bounded and cancellation-aware so a dead
/// worker cannot leave the parent hanging indefinitely.
async fn await_committed_child_boundary(
    reader: &dyn ThreadReader,
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
/// `run_configured_agent` helper below, this function never auto-approves a
/// child's HITL request.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_agent_until_boundary(
    catalog: &AgentCatalog,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    child_run_id: RunId,
    origin: DelegationOrigin,
    seed: Option<RunInput>,
    resume: Option<ResumeResult>,
    context: RuntimeRunContext,
    run_delegation: Option<Arc<dyn RunDelegationService>>,
    parent_thread_id: ThreadId,
    scheduler: Option<RunScheduler>,
) -> Result<AgentRunBoundary, AgentRunError> {
    let config = catalog
        .resolve(agent_id)
        .ok_or_else(|| AgentRunError::Configuration(format!("unknown agent {agent_id:?}")))?
        .clone();
    let thread = child_run_id.0.clone();
    let created = match &sandbox {
        AgentRunSandbox::Fresh(provider) => Some(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec(&thread))
                .await
                .map_err(|error| AgentRunError::Provisioning(error.to_string()))?,
        ),
        AgentRunSandbox::Shared(_) => None,
    };
    let env: &LocalSandbox = match &sandbox {
        AgentRunSandbox::Shared(shared) => shared,
        AgentRunSandbox::Fresh(_) => created
            .as_ref()
            .expect("a Fresh Agent Run created a sandbox"),
    };
    let mut runtime = build_runtime(llm, env);
    if let Some(service) = run_delegation {
        runtime = runtime.with_run_delegation(service);
    }
    if context.commit.is_none() || context.reader.is_none() {
        return Err(AgentRunError::Configuration(
            "an Agent Run requires commit and history wiring".to_string(),
        ));
    }
    let reader = context.reader.clone().expect("checked above");
    let thread_id = ThreadId(thread.clone());
    if matches!(reader.run_state(&child_run_id), Some(RunState::Ended(_))) {
        return Ok(AgentRunBoundary::Ended {
            text: latest_assistant_text(&reader.committed_messages(&thread_id)),
            usage: usage_from_committed(reader.as_ref(), &thread_id),
        });
    }
    let operation = match (seed, resume) {
        (Some(seed), None) => {
            let (_, mut activation) = runtime.prepare(&config, thread, seed);
            activation.run_id = child_run_id.clone();
            activation.delegation_origin = Some(origin);
            Ok((Some(activation), None))
        }
        (None, Some(result)) => {
            runtime.register_snapshot(config.clone());
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
            Ok((None, Some(ResumeCommand::from_ticket(&ticket, result, 0))))
        }
        _ => {
            return Err(AgentRunError::Configuration(
                "a child Run boundary requires exactly one of seed or resume".to_string(),
            ));
        }
    }?;
    let state = if let Some(scheduler) = scheduler {
        // The foreground child scheduler and the background dispatch pool share
        // one queue, so they must use the same epoch time domain. A synthetic
        // zero here makes every new child lease immediately expired to the pool,
        // which can re-claim it and fence the still-running foreground attempt.
        let now_ms = SystemClock.now_ms();
        let runtime = Arc::new(runtime);
        let mut worker = DispatchWorker::from_parts(
            runtime,
            scheduler.store.clone(),
            scheduler.commit.clone(),
            reader.clone(),
            scheduler.owner,
        )
        .with_context(context.clone());
        if let Some(claimed_commit) = scheduler.claimed_commit {
            worker = worker.with_claimed_commit(claimed_commit);
        }
        match operation {
            (Some(activation), None) => {
                // Stable child identity reconnects to committed truth. Only a
                // genuinely new child is scheduled; retries never create another.
                match reader.run_state(&child_run_id) {
                    Some(state @ (RunState::Awaiting | RunState::Ended(_))) => state,
                    _ => {
                        let request = child_dispatch_request(
                            activation,
                            parent_thread_id.clone(),
                            scheduler.model_access.as_ref(),
                        )?;
                        match worker.start_run(request, now_ms).await.map_err(|error| {
                            AgentRunError::Runtime(
                                awaken_runtime_contract::execution::Error::Execution(
                                    error.to_string(),
                                ),
                            )
                        })? {
                            Some((_, state)) => state,
                            None => {
                                await_committed_child_boundary(
                                    reader.as_ref(),
                                    &child_run_id,
                                    context.cancellation.as_ref(),
                                )
                                .await?
                            }
                        }
                    }
                }
            }
            (None, Some(command)) => {
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
                    result: command.result,
                };
                match worker.resume_run(input, now_ms).await.map_err(|error| {
                    AgentRunError::Runtime(awaken_runtime_contract::execution::Error::Execution(
                        error.to_string(),
                    ))
                })? {
                    Some((_, state)) => state,
                    None => {
                        await_committed_child_boundary(
                            reader.as_ref(),
                            &child_run_id,
                            context.cancellation.as_ref(),
                        )
                        .await?
                    }
                }
            }
            _ => unreachable!("operation construction is exhaustive"),
        }
    } else {
        let runs = DirectRunIngress::new(Arc::new(runtime));
        match operation {
            (Some(activation), None) => runs.start(activation, context).await,
            (None, Some(command)) => runs.resume(command, context).await,
            _ => unreachable!("operation construction is exhaustive"),
        }
        .map_err(AgentRunError::Runtime)?
    };
    match state {
        RunState::Awaiting => Ok(AgentRunBoundary::Awaiting),
        RunState::Ended(_) => Ok(AgentRunBoundary::Ended {
            text: latest_assistant_text(&reader.committed_messages(&thread_id)),
            usage: usage_from_committed(reader.as_ref(), &thread_id),
        }),
        RunState::Running => Err(AgentRunError::Runtime(
            awaken_runtime_contract::execution::Error::Execution(
                "an Agent Run escaped without reaching a settled boundary".to_string(),
            ),
        )),
    }
}

/// Run the agent identified by `agent_id` — its config resolved from `catalog` —
/// on its own `thread`, seeded with `seed`, to completion, returning its last
/// assistant line **and its accumulated token usage**. A child Run receives the
/// parent's resolved Runtime context and delegation executor, so it follows the
/// ordinary Agent lifecycle; auxiliary work may request an isolated context.
/// `cancellation`, when set, is forwarded so cancelling the parent cancels the
/// child Run too.
///
/// The returned [`ThreadUsage`] follows `rollup`: under
/// [`FoldIntoParent`](UsageRollup::FoldIntoParent) it is read from the child Run's own
/// committed thread state so the caller can fold it into a parent thread's total;
/// under [`Isolated`](UsageRollup::Isolated) the return is empty, so an auxiliary Agent's tokens are never
/// counted against the session. Also empty when the model reported no usage.
///
/// Errors preserve their category so delegation can recover a child commit
/// interruption without retrying terminal configuration failures.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_agent(
    catalog: &AgentCatalog,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    thread: &str,
    seed: impl Into<RunInput>,
    extra_tools: Vec<Arc<dyn RawTool>>,
    cancellation: Option<CancellationToken>,
    context: Option<RuntimeRunContext>,
    run_delegation: Option<Arc<dyn RunDelegationService>>,
    run_id: Option<RunId>,
    delegation_origin: Option<DelegationOrigin>,
) -> Result<(String, ThreadUsage), AgentRunError> {
    let config = catalog
        .resolve(agent_id)
        .ok_or_else(|| AgentRunError::Configuration(format!("unknown agent {agent_id:?}")))?
        .clone();
    // Reuse the parent's sandbox by default; a `Fresh` Agent Run gets its own root. The
    // created sandbox (if any) is bound here so the borrow lives for the whole run.
    let created = match &sandbox {
        AgentRunSandbox::Fresh(provider) => Some(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec(thread))
                .await
                .map_err(|error| AgentRunError::Provisioning(error.to_string()))?,
        ),
        AgentRunSandbox::Shared(_) => None,
    };
    let env: &LocalSandbox = match &sandbox {
        AgentRunSandbox::Shared(shared) => shared,
        AgentRunSandbox::Fresh(_) => created
            .as_ref()
            .expect("a Fresh Agent Run created a sandbox"),
    };
    let mut runtime = build_runtime(llm, env);
    // Tools the caller provisions on top of the sandbox's own (e.g. a
    // persistent write_memory scoped outside the ephemeral sandbox).
    for tool in extra_tools {
        runtime = runtime.with_tool(tool);
    }
    if let Some(service) = run_delegation {
        runtime = runtime.with_run_delegation(service);
    }
    let mut ctx = context.unwrap_or_else(|| {
        let commit = Arc::new(MemoryCommitCoordinator::new());
        RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit)
    });
    if let Some(token) = cancellation {
        ctx = ctx.with_cancellation(token);
    }
    if ctx.commit.is_none() || ctx.reader.is_none() {
        return Err(AgentRunError::Configuration(
            "an Agent Run requires commit and history wiring".to_string(),
        ));
    }
    let reader = ctx.reader.clone().expect("checked above");
    let thread_id = ThreadId(thread.to_string());
    match run_id {
        Some(run_id) => match delegation_origin {
            Some(origin) => {
                runtime
                    .run_delegated_to_completion(&config, run_id, thread, seed, ctx, origin, |_| {
                        ResumeResult::allow()
                    })
                    .await
            }
            None => {
                runtime
                    .run_to_completion_with_id(&config, run_id, thread, seed, ctx, |_| {
                        ResumeResult::allow()
                    })
                    .await
            }
        },
        None => {
            runtime
                .run_to_completion(&config, thread, seed, ctx, |_| ResumeResult::allow())
                .await
        }
    }
    .map_err(AgentRunError::Runtime)?;
    let text = latest_assistant_text(&reader.committed_messages(&thread_id));
    let usage = usage_from_committed(reader.as_ref(), &thread_id);
    Ok((text, usage))
}

/// Read a finished Agent Run's accumulated [`ThreadUsage`] out of committed
/// thread state. The run loop writes the running cumulative under
/// `THREAD_USAGE_STATE_KEY` each step, so the last `Set` is the whole tally
/// (mirrors `SharedHost::thread_usage`).
fn usage_from_committed(reader: &dyn ThreadReader, thread_id: &ThreadId) -> ThreadUsage {
    ThreadUsage::from_committed_state(&reader.committed_state(thread_id))
}

/// Run a default `assistant` Agent with `input` to completion and return its last
/// assistant line. Thin wrapper over [`run_configured_agent`]: it
/// builds a one-entry catalog holding the shared `assistant` config (no skills,
/// ADR-0036). Callers that need a differently-configured agent resolve it from a
/// richer catalog through [`run_configured_agent`] instead.
pub(crate) async fn run_agent(
    llm: Arc<dyn LlmExecutor>,
    execution: AgentExecution<'_>,
    sandbox: AgentRunSandbox<'_>,
    identity: AgentRunIdentity<'_>,
    input: impl Into<RunInput>,
    cancellation: Option<CancellationToken>,
) -> Result<(String, ThreadUsage), AgentRunError> {
    // This compatibility wrapper does not offer skills (ADR-0036).
    let config = server_config(
        execution.agent_id,
        execution.model_ref,
        &HashSet::new(),
        execution.delegates,
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let catalog = AgentCatalog::new().with_agent(config);
    run_configured_agent(
        &catalog,
        sandbox,
        llm,
        execution.agent_id,
        identity.thread,
        input,
        Vec::new(),
        cancellation,
        execution.context,
        execution.run_delegation,
        None,
        None,
    )
    .await
}

/// One-boundary counterpart of [`run_agent`] for a first-class delegated Run.
/// The target Agent receives the same config it would receive when admitted as a
/// root Run; only the supplied identity records that a parent created it.
pub(crate) async fn run_agent_until_boundary(
    llm: Arc<dyn LlmExecutor>,
    execution: AgentExecution<'_>,
    sandbox: AgentRunSandbox<'_>,
    request: ChildRunRequest,
) -> Result<AgentRunBoundary, AgentRunError> {
    let config = server_config(
        execution.agent_id,
        execution.model_ref,
        &HashSet::new(),
        execution.delegates,
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let catalog = AgentCatalog::new().with_agent(config);
    run_configured_agent_until_boundary(
        &catalog,
        sandbox,
        llm,
        execution.agent_id,
        request.run_id,
        request.origin,
        request.seed,
        request.resume,
        execution.context.unwrap_or_else(|| {
            let commit = Arc::new(MemoryCommitCoordinator::new());
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit)
        }),
        execution.run_delegation,
        request.parent_thread_id,
        execution.scheduler,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_agent_contract::agent::awaiting::ResumeTicket;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, Result as LlmResult,
    };
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

    /// A model that replies with the leading system instruction it was given, so a
    /// test can prove the sub-run resolved that agent's own config.
    struct InstructionEchoModel;

    struct EventuallySettledReader {
        reads: AtomicUsize,
    }

    impl ThreadReader for EventuallySettledReader {
        fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
            Vec::new()
        }

        fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
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

        impl ThreadReader for RunningReader {
            fn committed_messages(&self, _thread_id: &ThreadId) -> Vec<Message> {
                Vec::new()
            }

            fn resume_ticket(&self, _run_id: &RunId) -> Option<ResumeTicket> {
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

    #[async_trait::async_trait]
    impl LlmExecutor for InstructionEchoModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let system = request
                .messages
                .iter()
                .find(|m| m.role == Role::System)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            Ok(ChatResponse {
                output: AssistantOutput::text(system),
                usage: None,
                stop_reason: None,
            })
        }
    }

    fn agent(id: &str, instructions: &str) -> ExecutableAgentSnapshot {
        ExecutableAgentSnapshot::builder(id)
            .instructions(instructions)
            .model(ModelBinding::new("default", "stub", "default"))
            .max_steps(4)
            .build()
    }

    fn user(text: &str) -> Message {
        Message {
            id: MessageId("u1".into()),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    #[test]
    fn child_dispatch_persists_admission_pinned_model_access() {
        let config = agent("child", "child");
        let runtime = awaken_runtime::Runtime::new();
        let (_, activation) = runtime.prepare(
            &config,
            "child-thread".to_string(),
            RunInput::from(vec![user("go")]),
        );
        let expected = ModelAccessRef::candidate_set([
            (
                "primary".to_string(),
                ModelAccessRef::exact_credential("cred-a", "provider-a@1", "route-a@1"),
            ),
            (
                "fallback".to_string(),
                ModelAccessRef::exact_credential("cred-b", "provider-b@2", "route-b@3"),
            ),
        ])
        .expect("candidate access");
        let resolver: Arc<ModelAccessResolver> = {
            let expected = expected.clone();
            Arc::new(move |_| Ok(Some(expected.clone())))
        };

        let request = child_dispatch_request(
            activation,
            ThreadId("parent-thread".to_string()),
            Some(&resolver),
        )
        .expect("build child dispatch");

        assert_eq!(request.model_access, Some(expected));
        assert_eq!(request.session_thread_id.unwrap().0, "parent-thread");
    }

    #[tokio::test]
    async fn resolves_agent_config_by_id() {
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new()
            .with_agent(agent("memory-extractor", "MEMORY INSTRUCTIONS"))
            .with_agent(agent("judge", "JUDGE INSTRUCTIONS"));
        let llm = Arc::new(InstructionEchoModel);

        let (mem, _) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            llm.clone(),
            "memory-extractor",
            "t-mem",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(mem, "MEMORY INSTRUCTIONS");

        let (judge, _) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            llm.clone(),
            "judge",
            "t-judge",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(judge, "JUDGE INSTRUCTIONS");
    }

    /// A model that reports fixed token usage per inference, so a test can prove the
    /// sub-run's usage is read back out of its isolated commit store (rather than
    /// discarded with it).
    struct UsageModel;

    #[async_trait::async_trait]
    impl LlmExecutor for UsageModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: Some(awaken_runtime_contract::llm::TokenUsage {
                    prompt_tokens: 13,
                    completion_tokens: 5,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn agent_run_returns_its_accumulated_usage() {
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-usage-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));

        let (_text, usage) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(UsageModel),
            "worker",
            "t-usage",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // The sub-run's model reported 13/5 on its one step; that must survive the
        // committed Agent Run state as the returned tally.
        let total = usage.total();
        assert_eq!(total.prompt_tokens, 13);
        assert_eq!(total.completion_tokens, 5);
        assert_eq!(
            usage
                .by_model
                .get("stub")
                .copied()
                .unwrap_or_default()
                .prompt_tokens,
            13
        );
    }

    #[tokio::test]
    async fn unknown_agent_is_an_error() {
        let base = std::env::temp_dir().join("awaken-subrun-test-unknown");
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let err = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(InstructionEchoModel),
            "nope",
            "t",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown agent"), "got: {err}");
    }

    #[tokio::test]
    async fn a_shared_agent_run_reuses_the_parent_sandbox_while_fresh_makes_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let parent_base = tmp.path().join("parent");
        let fresh_base = tmp.path().join("fresh");
        // The parent agent's live sandbox (root = parent_base/main).
        let parent = LocalProvider::new(&parent_base)
            .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("main"))
            .await
            .unwrap();
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let fresh_provider = LocalProvider::new(&fresh_base);

        // Shared: the sub-run runs on the parent's sandbox — no new root is created
        // under the fresh provider's base (`默认共用`).
        run_configured_agent(
            &catalog,
            AgentRunSandbox::Shared(&parent),
            Arc::new(InstructionEchoModel),
            "assistant",
            "sub-a",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            !fresh_base.join("sub-a").exists(),
            "a shared sub-run must not create its own sandbox root"
        );

        // Fresh: the sub-run creates its own isolated root under the provider base.
        run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&fresh_provider),
            Arc::new(InstructionEchoModel),
            "assistant",
            "sub-b",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            fresh_base.join("sub-b").exists(),
            "a fresh sub-run creates its own isolated sandbox root"
        );
    }

    /// A delegated child reaches the same typed HITL boundary as a root Run. The
    /// parent-facing coordinator resumes it with a `ResumeResult`; no child-only
    /// continuation language or automatic permission bypass exists.
    #[tokio::test]
    async fn durable_child_permission_recovers_and_duplicate_resume_is_idempotent() {
        struct PermissionModel;

        #[async_trait::async_trait]
        impl LlmExecutor for PermissionModel {
            async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
                let output = if request.messages.iter().any(|m| m.role == Role::Tool) {
                    AssistantOutput::text("child done")
                } else {
                    AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
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
            .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("parent"))
            .await
            .unwrap();
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let dispatch_path = tmp.path().join("child-dispatch.db");
        let dispatch_path = dispatch_path.to_string_lossy().to_string();
        let scheduler = RunScheduler {
            store: Arc::new(
                AnyDispatchStore::open_sqlite(&dispatch_path).expect("child dispatch store"),
            ),
            commit: commit.clone(),
            reader: commit.clone(),
            owner: "replacement-worker".to_string(),
            claimed_commit: None,
            model_access: None,
        };
        let context = || {
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone())
        };
        let child_run_id = RunId("child-run-1".into());
        let origin = DelegationOrigin::root_for_agent(
            RunId("parent-run-1".into()),
            "delegate-call-1",
            "parent-agent",
        );
        let no_delegates = HashSet::new();
        let execution = |scheduler: RunScheduler| AgentExecution {
            agent_id: "worker",
            model_ref: "default",
            delegates: &no_delegates,
            run_delegation: None,
            context: Some(context()),
            scheduler: Some(scheduler),
        };

        let first = run_agent_until_boundary(
            Arc::new(PermissionModel),
            execution(scheduler),
            AgentRunSandbox::Shared(&sandbox),
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
            ticket.reason,
            awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission
        );
        assert_eq!(ticket.call_id.as_deref(), Some("child-write"));
        assert_eq!(ticket.delegation_origin.as_ref(), Some(&origin));

        // A replacement process reopens the durable queue and rebuilds every live
        // runtime object from committed truth; no child handle crosses this seam.
        let replacement = RunScheduler {
            store: Arc::new(
                AnyDispatchStore::open_sqlite(&dispatch_path).expect("reopen child dispatch"),
            ),
            commit: commit.clone(),
            reader: commit.clone(),
            owner: "replacement-worker-2".to_string(),
            claimed_commit: None,
            model_access: None,
        };

        let recovered_boundary = run_agent_until_boundary(
            Arc::new(PermissionModel),
            execution(replacement.clone()),
            AgentRunSandbox::Shared(&sandbox),
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
            execution(replacement),
            AgentRunSandbox::Shared(&sandbox),
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
            execution(RunScheduler {
                store: Arc::new(
                    AnyDispatchStore::open_sqlite(&dispatch_path)
                        .expect("reopen terminal child dispatch"),
                ),
                commit: commit.clone(),
                reader: commit.clone(),
                owner: "replacement-worker-3".to_string(),
                claimed_commit: None,
                model_access: None,
            }),
            AgentRunSandbox::Shared(&sandbox),
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
