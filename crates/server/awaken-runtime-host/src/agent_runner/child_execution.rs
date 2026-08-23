//! Child Run boundary orchestration, execution-edge materialization, and durable
//! dispatch construction.

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::delegation::DelegationOrigin;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::Coordinator as CommitCoordinator;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_run_ingress::{
    AnyDispatchStore, ClaimedRunCommit, Clock, DispatchWorker, PendingInput, RunDispatch,
    SystemClock,
};
use awaken_runtime::RunInput;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::delegation::RunDelegationService;
use awaken_runtime_contract::execution::RunAttemptExecutor;
use awaken_runtime_contract::llm::{LlmExecutor, ThreadUsage};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::{RawToolRegistry, ToolExecutor};

use super::{AgentRunError, AgentRunSandbox, usage_from_committed};
use crate::config::{
    build_runtime_with_authorization, configured_web_fetch_executor, effective_tool_authorization,
    latest_assistant_text,
};

/// A child task must not outlive the parent future that owns its delegation
/// boundary. Tokio's bare `JoinHandle` detaches on drop; this guard makes parent
/// cancellation abort the same child execution instead of creating an orphan.
struct AbortOnDropTask<T> {
    handle: tokio::task::JoinHandle<T>,
}

impl<T> AbortOnDropTask<T> {
    fn spawn(future: impl std::future::Future<Output = T> + Send + 'static) -> Self
    where
        T: Send + 'static,
    {
        Self {
            handle: tokio::spawn(future),
        }
    }

    async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        (&mut self.handle).await
    }
}

impl<T> Drop for AbortOnDropTask<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
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
    pub(crate) reader: Arc<dyn CommittedThreadView>,
    pub(crate) owner: String,
    pub(crate) claimed_commit: Option<Arc<dyn ClaimedRunCommit>>,
    pub(crate) recovery_projection: Option<Arc<awaken_run_ingress::RecoveryProjection>>,
    /// Parent Session resource authority inherited by a child dispatch. The child
    /// keeps its own Run/thread lifecycle while executing in the parent's Session
    /// environment, so a replacement worker must install the same frozen manifest.
    pub(crate) session_resources: Option<awaken_session_contract::SessionResourceManifest>,
    /// Attempt-scoped publication lookup inherited by every durable child
    /// admission. A child freezes its own non-root closure from this immutable
    /// source; no execution path reopens the mutable host catalog.
    pub(crate) publication_source:
        Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
}

impl RunScheduler {
    /// The local Session's committed read authority for a child Thread.
    ///
    /// A database-independent Worker exposes only its parent claim's recovery
    /// projection here. That projection cannot contain a child Thread, so remote
    /// children must continue through their own claim-installed projection.
    pub(crate) fn local_child_reader(&self) -> Option<Arc<dyn CommittedThreadView>> {
        self.recovery_projection
            .is_none()
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

pub(super) fn settled_agent_boundary(
    reader: &dyn CommittedThreadView,
    thread_id: &ThreadId,
    state: RunState,
) -> Result<AgentRunBoundary, AgentRunError> {
    match state {
        RunState::Awaiting => Ok(AgentRunBoundary::Awaiting),
        RunState::Ended(
            awaken_agent_contract::agent::run::EndCause::NaturalEnd
            | awaken_agent_contract::agent::run::EndCause::MaxSteps,
        ) => Ok(AgentRunBoundary::Ended {
            text: latest_assistant_text(&reader.committed_messages(thread_id)),
            usage: usage_from_committed(reader, thread_id),
        }),
        RunState::Ended(cause) => Err(AgentRunError::Runtime(
            awaken_runtime_contract::execution::Error::Execution(format!(
                "child Agent ended unsuccessfully: {cause:?}"
            )),
        )),
        RunState::Running => Err(AgentRunError::Runtime(
            awaken_runtime_contract::execution::Error::Execution(
                "an Agent Run escaped without reaching a settled boundary".to_string(),
            ),
        )),
    }
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
/// [`super::run_configured_agent`] helper, this function never auto-approves a child's
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
    let current_tool_executor: Arc<dyn ToolExecutor> = match &sandbox {
        AgentRunSandbox::Shared(environment) => environment.tool_executor(),
        #[cfg(test)]
        AgentRunSandbox::SharedLocal(_) => Arc::new(RawToolRegistry::new(env.runtime_tools())),
        AgentRunSandbox::Fresh(_) => Arc::new(RawToolRegistry::new(env.runtime_tools())),
    };
    let authorization = effective_tool_authorization(
        &config.resolved_spec.plugin_config,
        &[],
        &config.resolved_spec.plugin_config.agent.toolsets,
    );
    let mut runtime = build_runtime_with_authorization(llm, env, &authorization);
    if config
        .resolved_spec
        .plugin_ids
        .iter()
        .any(|id| id == awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID)
    {
        let plugin = adapters.web_search.clone().ok_or_else(|| {
            AgentRunError::Configuration(
                "a child publication selects WebSearch but the Host has no WebSearch adapter"
                    .to_string(),
            )
        })?;
        runtime = runtime.with_plugin(Arc::new(
            (*plugin).clone().with_execution_configuration(
                awaken_ext_builtin_tools::web_search_execution_configuration(
                    &config.resolved_spec.plugin_config.agent.toolsets,
                )
                .map_err(AgentRunError::Configuration)?,
            ),
        ));
    }
    if let Some(service) = run_delegation {
        runtime = runtime.with_run_delegation(service);
    }
    if context.commit.is_none() || context.reader.is_none() {
        return Err(AgentRunError::Configuration(
            "an Agent Run requires commit and history wiring".to_string(),
        ));
    }
    let context = context.with_tool_executor(
        configured_web_fetch_executor(
            Some(current_tool_executor),
            &config.resolved_spec.plugin_config.agent.toolsets,
        )
        .map_err(AgentRunError::Configuration)?
        .expect("an Agent Run sandbox always supplies its current Hand executor"),
    );
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
    let mut boundary_reader = reader.clone();
    let state = if let Some(scheduler) = scheduler {
        // The foreground child scheduler and every later ownership operation
        // retain this one edge clock. The Worker never substitutes another time
        // source after the queue grants the claim.
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let runtime = Arc::new(runtime);
        let attempt_executor = child_attempt_executor(
            runtime.clone(),
            &config,
            &adapters,
            authorization.policy.clone(),
        )?;
        // A database-independent Worker keeps one recovery projection per
        // claimed Run. Reusing the parent's projection lets the child install
        // its snapshot over the still-running parent and fences whichever one
        // commits next. Local authoritative stores need no projection.
        let child_projection =
            isolated_child_recovery_projection(scheduler.recovery_projection.as_ref());
        let worker_reader: Arc<dyn CommittedThreadView> = child_projection
            .as_ref()
            .map_or_else(|| reader.clone(), |projection| projection.clone());
        boundary_reader = worker_reader.clone();
        let mut worker = DispatchWorker::from_parts(
            runtime,
            scheduler.store.clone(),
            scheduler.commit.clone(),
            worker_reader,
            scheduler.owner,
        )
        .with_context(context.clone())
        .with_local_credential_capabilities(adapters.remote_credentials.clone());
        worker.install_attempt_executor(attempt_executor);
        if let Some(claimed_commit) = scheduler.claimed_commit {
            worker = worker.with_claimed_commit(claimed_commit);
        }
        if let Some(projection) = child_projection {
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
                            scheduler.publication_source.as_deref(),
                        )?;
                        match worker
                            .start_run(request, clock.clone())
                            .await
                            .map_err(|error| {
                                AgentRunError::Runtime(
                                    awaken_runtime_contract::execution::Error::Execution(
                                        error.to_string(),
                                    ),
                                )
                            })? {
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
                let resumed = worker
                    .resume_run(input, clock.clone())
                    .await
                    .map_err(|error| {
                        AgentRunError::Runtime(
                            awaken_runtime_contract::execution::Error::Execution(error.to_string()),
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
        }
    } else {
        let runtime = Arc::new(runtime);
        let attempt_executor =
            child_attempt_executor(runtime, &config, &adapters, authorization.policy.clone())?;
        // A Native child is one ordinary Run, but polling that complete Run
        // recursively inside its parent's `agent_run` poll stack can overflow a
        // standard Tokio worker stack in debug and sufficiently deep production
        // compositions. An owned task is only an execution boundary: the same
        // activation, context, executor, commit authority, and cancellation token
        // remain canonical. Abort-on-drop prevents detaching work if the parent
        // attempt is cancelled while awaiting the child boundary.
        AbortOnDropTask::spawn(async move {
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
        })
        .join()
        .await
        .map_err(|error| {
            AgentRunError::Runtime(awaken_runtime_contract::execution::Error::Execution(
                format!("delegated child task failed: {error}"),
            ))
        })??
    };
    settled_agent_boundary(boundary_reader.as_ref(), &thread_id, state)
}

/// Execution-edge adapters already installed by the host process startup.
///
/// A delegated child selects only through its immutable `backend_ref`; this
/// value carries implementations and credential-realization evidence, never a
/// second target directory or backend-selection rule.
#[derive(Clone, Default)]
pub(crate) struct ChildExecutionAdapters {
    pub(crate) acp: Option<ChildAcpExecutorFactory>,
    pub(crate) remote: Option<Arc<dyn RunAttemptExecutor>>,
    pub(crate) remote_credentials: awaken_runtime_contract::CredentialRealizationCapabilities,
    /// The same host-configured plugin instance shape used by a root Native Run.
    /// The child snapshot still decides whether the plugin is selected.
    pub(crate) web_search: Option<Arc<awaken_ext_builtin_tools::WebSearchPlugin>>,
    pub(crate) web_fetch: Option<Arc<awaken_ext_builtin_tools::WebFetchPlugin>>,
}

/// Materializes the already-selected ACP execution edge for a child snapshot.
/// Selection stays in the immutable `backend_ref`; the factory only binds that
/// exact backend to the Session-owned sandbox and permission context.
pub(crate) type ChildAcpExecutorFactory = Arc<
    dyn Fn(
            Backend,
            Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
        ) -> Result<Arc<dyn RunAttemptExecutor>, String>
        + Send
        + Sync,
>;

pub(super) fn isolated_child_recovery_projection(
    parent: Option<&Arc<awaken_run_ingress::RecoveryProjection>>,
) -> Option<Arc<awaken_run_ingress::RecoveryProjection>> {
    parent.map(|_| Arc::new(awaken_run_ingress::RecoveryProjection::new()))
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

pub(crate) fn child_attempt_executor(
    runtime: Arc<awaken_runtime::Runtime>,
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
    adapters: &ChildExecutionAdapters,
    permission: Arc<dyn awaken_runtime_contract::permission::ToolPermissionPolicy>,
) -> Result<Arc<dyn RunAttemptExecutor>, AgentRunError> {
    let backends = snapshot
        .resolved_spec
        .attempt_candidates(None)
        .into_iter()
        .map(|candidate| Backend::from_ref(&candidate.binding().backend_ref))
        .collect::<Vec<_>>();
    let acp = backends
        .iter()
        .find(|backend| matches!(backend, Backend::Acp(_)))
        .cloned()
        .map(|backend| {
            adapters.acp.as_ref().ok_or_else(|| {
                AgentRunError::Configuration(format!(
                    "delegate publication requires unavailable ACP backend {backend:?}"
                ))
            })?(backend, permission.clone())
            .map_err(AgentRunError::Configuration)
        })
        .transpose()?;
    let remote = if backends
        .iter()
        .any(|backend| matches!(backend, Backend::Remote(_)))
    {
        Some(adapters.remote.clone().ok_or_else(|| {
            AgentRunError::Configuration(
                "delegate publication requires an unavailable remote backend".to_string(),
            )
        })?)
    } else {
        None
    };
    Ok(Arc::new(
        crate::run_exec::SessionAttemptExecutor::from_executors(
            runtime,
            acp,
            remote,
            &[&snapshot.resolved_spec],
        ),
    ))
}

pub(super) fn bind_direct_child_credentials(
    activation: &RunActivation,
    mut context: RuntimeRunContext,
    adapters: &ChildExecutionAdapters,
) -> Result<RuntimeRunContext, AgentRunError> {
    let candidates = activation
        .snapshot
        .resolved_spec
        .attempt_candidates(activation.model_ref_override.as_deref())
        .into_iter()
        .filter(|candidate| {
            matches!(
                candidate.provisioning(),
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
    let holder = crate::host::self_hosted_inference_holder(activation)
        .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    let bindings = awaken_runtime_contract::compile_candidate_credential_bindings(
        &candidates,
        holder.as_ref(),
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

pub(crate) fn child_dispatch_request(
    activation: RunActivation,
    parent_thread_id: ThreadId,
    session_resources: Option<awaken_session_contract::SessionResourceManifest>,
    publication_source: Option<&dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
) -> Result<RunDispatch, AgentRunError> {
    let workspace = session_resources
        .as_ref()
        .map_or("default", |resources| resources.workspace_id.as_str());
    let child_publications = crate::agent_catalog::freeze_run_publications(
        &activation.snapshot,
        publication_source,
        workspace,
    )
    .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    let inference_holder = crate::host::self_hosted_inference_holder(&activation)
        .map_err(|error| AgentRunError::Configuration(error.to_string()))?;
    let placement = crate::host::remote_worker_placement(
        &activation.snapshot.resolved_spec,
        None,
        session_resources.as_ref(),
        false,
    );
    let mut request = RunDispatch::new(activation)
        .for_session(parent_thread_id)
        .with_traceparent(awaken_observability::current_traceparent())
        .with_agent_publications(child_publications)
        .with_placement(placement);
    if let Some(holder) = inference_holder {
        request = request.with_inference_plaintext_holder(holder);
    }
    if let Some(resources) = session_resources {
        let envelope = awaken_run_ingress::SessionResourceEnvelope::from_manifest(&resources)
            .map_err(|error| {
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
