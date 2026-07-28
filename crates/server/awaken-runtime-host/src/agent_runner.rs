//! Constructing and running an Agent through the ordinary Run lifecycle.
//!
//! Native delegation (`agent_run`) and skill forks use this
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
    AnyDispatchStore, ClaimedRunCommit, Clock, DispatchWorker, PendingInput, RunDispatch,
    SystemClock,
};
use awaken_runtime::RunInput;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::delegation::RunDelegationService;
use awaken_runtime_contract::execution::{AttemptExecutorRegistry, RunAttemptExecutor};
use awaken_runtime_contract::llm::{LlmExecutor, ThreadUsage};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::LocalProvider;
#[cfg(test)]
use awaken_sandbox_local::LocalSandbox;

use crate::agent_catalog::AgentCatalog;
use crate::config::{build_runtime, latest_assistant_text, server_config};

/// Where an Agent Run's tools execute. A delegated Agent shares the initiating
/// Agent's sandbox by default, or receives a fresh sandbox when placement policy
/// requests isolation. Sandbox placement does not alter Run semantics.
pub(crate) enum AgentRunSandbox<'a> {
    /// Reuse the parent agent's live sandbox — the default (`与主 agent 共用`).
    Shared(&'a crate::session_environment::SessionEnvironment),
    #[cfg(test)]
    SharedLocal(&'a LocalSandbox),
    /// Create a fresh, isolated sandbox for this Agent Run via the given provider.
    Fresh(&'a LocalProvider),
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
    #[cfg(test)]
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
    pub(crate) recovery_projection: Option<Arc<awaken_run_ingress::RecoveryProjection>>,
    /// Parent Session resource authority inherited by a child dispatch. The child
    /// keeps its own Run/thread lifecycle while executing in the parent's Session
    /// environment, so a replacement worker must install the same frozen manifest.
    pub(crate) session_resources: Option<awaken_protocol_managed::SessionResourceManifest>,
}

/// Execution-edge adapters already installed by the host composition root.
///
/// A delegated child selects only through its immutable `backend_ref`; this
/// value carries implementations and credential-realization evidence, never a
/// second target directory or backend-selection rule.
#[derive(Clone, Default)]
pub(crate) struct ChildExecutionAdapters {
    pub(crate) remote: Option<Arc<dyn RunAttemptExecutor>>,
    pub(crate) remote_credentials: awaken_runtime_contract::CredentialRealizationCapabilities,
}

struct ChildCredentialRecorder {
    ownership: Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>,
    bindings: Vec<awaken_runtime_contract::AttemptCredentialBinding>,
}

#[async_trait::async_trait]
impl awaken_runtime_contract::CredentialRealizationRecorder for ChildCredentialRecorder {
    async fn record(
        &self,
        receipt: awaken_runtime_contract::CredentialRealizationReceipt,
    ) -> Result<(), awaken_runtime_contract::CredentialRealizationRecordError> {
        self.ownership.verify_current().await.map_err(|error| {
            awaken_runtime_contract::CredentialRealizationRecordError(error.to_string())
        })?;
        awaken_runtime_contract::verify_credential_realization_receipt(&self.bindings, &receipt)
            .map_err(|error| {
                awaken_runtime_contract::CredentialRealizationRecordError(error.to_string())
            })
    }
}

fn child_attempt_executor(
    runtime: Arc<awaken_runtime::Runtime>,
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
    adapters: &ChildExecutionAdapters,
) -> Result<Arc<dyn RunAttemptExecutor>, AgentRunError> {
    let mut registry = AttemptExecutorRegistry::new();
    registry
        .register_native(runtime)
        .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    for candidate in snapshot.resolved_spec.candidate_bindings() {
        if matches!(
            awaken_runtime_contract::resolved::Backend::from_ref(&candidate.backend_ref),
            awaken_runtime_contract::resolved::Backend::Remote { .. }
        ) && !registry.supports(&candidate.backend_ref)
        {
            let remote = adapters.remote.clone().ok_or_else(|| {
                AgentRunError::Configuration(format!(
                    "delegate publication requires unavailable remote backend {:?}",
                    candidate.backend_ref
                ))
            })?;
            registry
                .register(candidate.backend_ref.clone(), remote)
                .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
        }
    }
    Ok(Arc::new(registry))
}

fn bind_direct_child_credentials(
    activation: &RunActivation,
    mut context: RuntimeRunContext,
    adapters: &ChildExecutionAdapters,
) -> Result<RuntimeRunContext, AgentRunError> {
    let candidates = activation
        .snapshot
        .resolved_spec
        .execution_candidates(activation.model_ref_override.as_deref())
        .into_iter()
        .filter(|candidate| {
            matches!(
                candidate.provisioning,
                awaken_runtime_contract::resolved::ModelProvisioning::Remote { .. }
            )
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(context);
    }
    let ownership = context.ownership.clone().ok_or_else(|| {
        AgentRunError::Configuration(
            "direct delegated remote execution requires parent attempt ownership".to_string(),
        )
    })?;
    let epoch = LOCAL_CHILD_ATTEMPT_EPOCH
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .max(1);
    let now_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default();
    let holder = awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
        .inference_holder;
    let bindings = awaken_runtime_contract::compile_candidate_credential_bindings(
        &candidates,
        Some(&holder),
        &adapters.remote_credentials,
        epoch,
        now_unix_ms,
    )
    .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    if !bindings.is_empty() {
        context = context.with_credential_realization(
            awaken_runtime_contract::AttemptCredentialRealization::new(
                bindings.clone(),
                Arc::new(ChildCredentialRecorder {
                    ownership,
                    bindings,
                }),
            ),
        );
    }
    Ok(context)
}

static LOCAL_CHILD_ATTEMPT_EPOCH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

fn child_dispatch_request(
    activation: RunActivation,
    parent_thread_id: ThreadId,
    session_resources: Option<awaken_protocol_managed::SessionResourceManifest>,
) -> Result<RunDispatch, AgentRunError> {
    let placement = crate::host::remote_worker_placement(
        &activation.snapshot.resolved_spec,
        session_resources.as_ref(),
        false,
    );
    let mut request = RunDispatch::new(activation)
        .for_session(parent_thread_id)
        .with_traceparent(awaken_observability::current_traceparent())
        .with_placement(placement);
    if let Some(resources) = session_resources {
        let envelope =
            crate::provisioning::encode_session_resource_envelope(&resources).map_err(|error| {
                AgentRunError::Configuration(format!(
                    "serialize child Session resource manifest: {error}"
                ))
            })?;
        request = request
            .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                awaken_tenancy::ScopeId::from(resources.workspace_id.clone()),
            ))
            .with_session_resources(envelope);
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
    config: &awaken_runtime_contract::snapshot::ExecutableAgentSnapshot,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    child_run_id: RunId,
    origin: DelegationOrigin,
    seed: Option<RunInput>,
    resume: Option<ResumeResult>,
    context: RuntimeRunContext,
    run_delegation: Option<Arc<dyn RunDelegationService>>,
    parent_thread_id: ThreadId,
    scheduler: Option<RunScheduler>,
    adapters: ChildExecutionAdapters,
) -> Result<AgentRunBoundary, AgentRunError> {
    let config = config.clone();
    let thread = child_run_id.0.clone();
    let created = match &sandbox {
        AgentRunSandbox::Fresh(provider) => Some(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec(&thread))
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
            let mut activation = RunActivation::new(
                child_run_id.clone(),
                thread_id.clone(),
                config.clone(),
                Vec::new(),
            );
            activation.delegation_origin = Some(origin);
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
    let state = if let Some(scheduler) = scheduler {
        // The foreground child scheduler and the background dispatch pool share
        // one queue, so they must use the same epoch time domain. A synthetic
        // zero here makes every new child lease immediately expired to the pool,
        // which can re-claim it and fence the still-running foreground attempt.
        let now_ms = SystemClock.now_ms();
        let runtime = Arc::new(runtime);
        let attempt_executor = child_attempt_executor(runtime.clone(), &config, &adapters)?;
        let mut worker = DispatchWorker::from_parts(
            runtime,
            scheduler.store.clone(),
            scheduler.commit.clone(),
            reader.clone(),
            scheduler.owner,
        )
        .with_context(context.clone())
        .with_local_credential_capabilities(adapters.remote_credentials.clone());
        worker.install_attempt_executor(attempt_executor);
        if let Some(claimed_commit) = scheduler.claimed_commit {
            worker = worker.with_claimed_commit(claimed_commit);
        }
        if let Some(projection) = scheduler.recovery_projection {
            worker = worker.with_recovery_projection(projection);
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
                            scheduler.session_resources.clone(),
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
        let runtime = Arc::new(runtime);
        let attempt_executor = child_attempt_executor(runtime, &config, &adapters)?;
        match operation {
            (Some(activation), None) => {
                let context = bind_direct_child_credentials(&activation, context, &adapters)?;
                attempt_executor.execute(activation, context).await
            }
            (Some(activation), Some(command)) => {
                let context = bind_direct_child_credentials(&activation, context, &adapters)?;
                attempt_executor.resume(activation, command, context).await
            }
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
/// This helper is intentionally auxiliary-only. Delegated children use
/// [`run_configured_agent_until_boundary`] and the ordinary durable dispatch path;
/// retaining optional child identity/origin arguments here would recreate the
/// retired second child-execution path.
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
) -> Result<(String, ThreadUsage), AgentRunError> {
    run_configured_agent_inner(
        catalog,
        sandbox,
        llm,
        agent_id,
        thread,
        seed.into(),
        extra_tools,
        cancellation,
        context,
        run_delegation,
        None,
    )
    .await
}

/// Execute one recoverable auxiliary Agent using a caller-owned stable Run id.
///
/// This is still the ordinary Runtime lifecycle and commit boundary. The helper
/// only removes random identity from housekeeping retries; it carries no parent
/// delegation origin and cannot create a delegated child relationship.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_agent_with_id(
    catalog: &AgentCatalog,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    thread: &str,
    run_id: RunId,
    seed: impl Into<RunInput>,
    extra_tools: Vec<Arc<dyn RawTool>>,
    context: RuntimeRunContext,
) -> Result<(String, ThreadUsage), AgentRunError> {
    run_configured_agent_inner(
        catalog,
        sandbox,
        llm,
        agent_id,
        thread,
        seed.into(),
        extra_tools,
        None,
        Some(context),
        None,
        Some(run_id),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_configured_agent_inner(
    catalog: &AgentCatalog,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    thread: &str,
    seed: RunInput,
    extra_tools: Vec<Arc<dyn RawTool>>,
    cancellation: Option<CancellationToken>,
    context: Option<RuntimeRunContext>,
    run_delegation: Option<Arc<dyn RunDelegationService>>,
    stable_run_id: Option<RunId>,
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
    let state = match stable_run_id {
        Some(run_id) => {
            runtime
                .run_to_completion_with_id(&config, run_id, thread, seed, ctx, |_| {
                    ResumeResult::allow()
                })
                .await
        }
        None => {
            runtime
                .run_to_completion(&config, thread, seed, ctx, |_| ResumeResult::allow())
                .await
        }
    }
    .map_err(AgentRunError::Runtime)?;
    if let RunState::Ended(cause) = &state
        && !matches!(
            cause,
            awaken_agent_contract::agent::run::EndCause::NaturalEnd
                | awaken_agent_contract::agent::run::EndCause::MaxSteps
        )
    {
        return Err(AgentRunError::Runtime(
            awaken_runtime_contract::execution::Error::Execution(format!(
                "auxiliary Agent ended unsuccessfully: {cause:?}"
            )),
        ));
    }
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
    thread: &str,
    input: impl Into<RunInput>,
    cancellation: Option<CancellationToken>,
) -> Result<(String, ThreadUsage), AgentRunError> {
    // Auxiliary Agents deliberately do not inherit user-visible Skills.
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
        thread,
        input,
        Vec::new(),
        cancellation,
        execution.context,
        execution.run_delegation,
    )
    .await
}

/// One-boundary counterpart of [`run_agent`] for a first-class delegated Run.
/// The target Agent receives the same config it would receive when admitted as a
/// root Run; only the supplied identity records that a parent created it.
#[cfg(test)]
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
    run_configured_agent_until_boundary(
        &config,
        sandbox,
        llm,
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
        ChildExecutionAdapters::default(),
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
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
    use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

    fn published_model(
        model: &str,
        credential: &str,
        provider: &str,
        route: &str,
    ) -> ResolvedModelCandidate {
        ResolvedModelCandidate::provider(
            ModelBinding::new(provider, model, "native"),
            provider,
            route,
            "workspace-a",
            Some(awaken_runtime_contract::CredentialAccess::new(
                awaken_runtime_contract::CredentialRef {
                    id: credential.into(),
                    revision: 0,
                },
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
            )),
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "test".into(),
                api_dialect: String::new(),
                base_url: "https://example.invalid".into(),
                upstream_model: model.into(),
            },
        )
    }

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
    fn child_dispatch_reuses_publication_pinned_model_candidates() {
        let mut config = agent("child", "child");
        let primary = published_model("primary", "cred-a", "provider-a@1", "route-a@1");
        let fallback = published_model("fallback", "cred-b", "provider-b@2", "route-b@3");
        config.resolved_spec.model_binding = primary.clone();
        config.resolved_spec.model_candidates = vec![fallback.clone()];
        let runtime = awaken_runtime::Runtime::new();
        let (_, activation) = runtime.prepare(
            &config,
            "child-thread".to_string(),
            RunInput::from(vec![user("go")]),
        );

        let manifest = awaken_protocol_managed::SessionResourceManifest::new(
            "workspace-a",
            awaken_protocol_managed::ResolvedSessionResources {
                inputs: Vec::new(),
                skills: Some(vec![awaken_protocol_managed::ResolvedSkillBinding {
                    skill_id: "skill-a".into(),
                    version: 3,
                    bundle_sha256: "sha256:skill-a-v3".into(),
                }]),
            },
        );
        let request = child_dispatch_request(
            activation,
            ThreadId("parent-thread".to_string()),
            Some(manifest.clone()),
        )
        .expect("build child dispatch");

        assert_eq!(
            request.activation.snapshot.resolved_spec.model_binding,
            primary
        );
        assert_eq!(
            request.activation.snapshot.resolved_spec.model_candidates,
            vec![fallback]
        );
        assert_eq!(request.session_thread_id.unwrap().0, "parent-thread");
        let carried = crate::provisioning::decode_session_resource_envelope(
            request
                .session_resources
                .as_ref()
                .expect("resource envelope"),
        )
        .expect("decode resource envelope");
        assert_eq!(carried, manifest);
        assert!(
            request
                .placement
                .required_capabilities
                .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY)
        );
    }

    #[test]
    fn child_dispatch_always_carries_backend_placement_without_resources() {
        // Cause/effect graph:
        // C1 the child publication pins a Remote backend; C2 the child has no
        // Session resources. E1 admission still requires the A2A executor
        // capability; E2 no resource capability is invented.
        //
        // Decision rule P1: C1+C2 => E1+E2. The resource-present/provider case
        // is rule P2 in `child_dispatch_reuses_publication_pinned_model_candidates`.
        // Together they prevent optional resource staging from controlling
        // backend or credential placement.
        let mut config = agent("remote-child", "remote child");
        config.resolved_spec.model_binding = ResolvedModelCandidate::remote(
            ModelBinding::new("remote", "", "a2a:https://agent.example"),
            awaken_tenancy::ScopeId::from("default"),
            None,
            "sha256:card",
        );
        let runtime = awaken_runtime::Runtime::new();
        let (_, activation) = runtime.prepare(
            &config,
            "remote-child-thread".to_string(),
            RunInput::from(vec![user("go")]),
        );

        let request =
            child_dispatch_request(activation, ThreadId("parent-thread".to_string()), None)
                .expect("build remote child dispatch");

        assert!(
            request
                .placement
                .required_capabilities
                .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY)
        );
        assert!(
            !request
                .placement
                .required_capabilities
                .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY)
        );
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

    struct CountingModel(AtomicUsize);

    #[async_trait::async_trait]
    impl LlmExecutor for CountingModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                output: AssistantOutput::text("stable"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    struct RejectedModel;

    #[async_trait::async_trait]
    impl LlmExecutor for RejectedModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Err(awaken_runtime_contract::llm::Error::Binding(
                "candidate is outside the frozen set".into(),
            ))
        }
    }

    #[tokio::test]
    async fn auxiliary_terminal_failure_is_not_projected_as_empty_success() {
        // Cause graph / decision table:
        // C1=terminal cause is NaturalEnd/MaxSteps; C2=terminal cause is a fault.
        // | Rule | C1 | C2 | result                                      |
        // | R1   | T  | F  | return reply/usage                          |
        // | R2   | F  | T  | return error; caller must retry/fail closed |
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let context = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit);

        let error = run_configured_agent_with_id(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(RejectedModel),
            "worker",
            "aux/rejected",
            RunId("aux/rejected/run".into()),
            vec![user("go")],
            Vec::new(),
            context,
        )
        .await
        .expect_err("R2: a terminal inference fault is not an empty successful result");

        assert!(error.to_string().contains("ended unsuccessfully"));
        assert!(error.to_string().contains("binding_rejected"));
    }

    #[tokio::test]
    async fn stable_auxiliary_run_reuses_committed_terminal_truth_without_reinference() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));
        let model = Arc::new(CountingModel(AtomicUsize::new(0)));
        let commit = Arc::new(MemoryCommitCoordinator::new());
        let context = || {
            RuntimeRunContext::new()
                .with_commit(commit.clone())
                .with_reader(commit.clone())
        };
        let run_id = RunId("aux/stable/run".into());

        for input in ["first", "ignored retry input"] {
            let (reply, _) = run_configured_agent_with_id(
                &catalog,
                AgentRunSandbox::Fresh(&provider),
                model.clone(),
                "worker",
                "aux/stable",
                run_id.clone(),
                vec![user(input)],
                Vec::new(),
                context(),
            )
            .await
            .unwrap();
            assert_eq!(reply, "stable");
        }

        assert_eq!(model.0.load(Ordering::SeqCst), 1);
        assert!(matches!(
            commit.run_state(&run_id),
            Some(RunState::Ended(_))
        ));
        assert_eq!(
            commit
                .committed_messages(&ThreadId("aux/stable".into()))
                .iter()
                .filter(|message| message.role == Role::Assistant)
                .count(),
            1
        );
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
            AgentRunSandbox::SharedLocal(&parent),
            Arc::new(InstructionEchoModel),
            "assistant",
            "sub-a",
            vec![user("go")],
            Vec::new(),
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
            recovery_projection: None,
            session_resources: None,
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
            recovery_projection: None,
            session_resources: None,
        };

        let recovered_boundary = run_agent_until_boundary(
            Arc::new(PermissionModel),
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
            execution(RunScheduler {
                store: Arc::new(
                    AnyDispatchStore::open_sqlite(&dispatch_path)
                        .expect("reopen terminal child dispatch"),
                ),
                commit: commit.clone(),
                reader: commit.clone(),
                owner: "replacement-worker-3".to_string(),
                claimed_commit: None,
                recovery_projection: None,
                session_resources: None,
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
