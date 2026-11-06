// @formal entrypoint model_advisor.never_substitutes_for_primary_admission_or_duplicates_a_binding
// @formal entrypoint permission.runtime_capability_and_verdict_projection_never_widens_authority
// @formal entrypoint session_execution.one_physical_attempt_per_thread
// @formal entrypoint tool.executor_entry_order
//! Tool authorization, execution selection, and resumed-result folding.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_tool_contract::tool::{
    DurableTaskExecutor, ToolTaskContinuation, start_executor_task, start_tool_task,
};
use std::sync::Arc;

/// Private cancellation signal consumed by the existing dispatch disposition
/// boundary. It is not a serialized Run failure or Session admission marker.
#[derive(Debug, thiserror::Error)]
#[error("tool phase cancelled")]
struct ToolPhaseCancelled;

pub(super) fn is_tool_phase_cancelled(error: &Error) -> bool {
    use std::error::Error as _;
    matches!(error, Error::Diagnostic(diagnostic)
        if diagnostic.source().is_some_and(|source| source.is::<ToolPhaseCancelled>()))
}

fn tool_phase_cancelled(context: &RuntimeRunContext) -> Error {
    context.physical_quiescence.mark_uncertain();
    awaken_runtime_contract::diagnostic::RuntimeDiagnostic::new(
        awaken_runtime_contract::diagnostic::RuntimeErrorKind::Unavailable,
        "tool_phase_cancelled",
        ToolPhaseCancelled,
    )
    .into()
}

pub(super) fn check_tool_phase_cancellation(context: &RuntimeRunContext) -> Result<()> {
    if context.is_cancelled() {
        Err(tool_phase_cancelled(context))
    } else {
        Ok(())
    }
}

/// Cancel only an effect/hook/materialization wait. Journal commits deliberately
/// do not enter this wrapper: their enclosing owner drains ACK/error first.
pub(super) async fn cancellable_tool_phase<T>(
    context: &RuntimeRunContext,
    operation: impl std::future::Future<Output = T>,
) -> Result<T> {
    check_tool_phase_cancellation(context)?;
    match &context.cancellation {
        None => Ok(operation.await),
        Some(token) => tokio::select! {
            biased;
            result = operation => Ok(result),
            _ = token.cancelled() => {
                Err(tool_phase_cancelled(context))
            },
        },
    }
}

/// Runtime-owned operations are classified once at the model boundary. The
/// closed enum prevents dispatch, recovery and concurrency policy from growing
/// independent string-matching tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeToolOperation {
    ToolSearch,
}

impl RuntimeToolOperation {
    #[must_use]
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::ToolSearch => awaken_runtime_contract::resolved::TOOL_SEARCH_ID,
        }
    }

    #[must_use]
    pub(crate) fn classify(tool_id: &str) -> Option<Self> {
        [Self::ToolSearch]
            .into_iter()
            .find(|operation| operation.id() == tool_id)
    }
}

/// Apply the Session's one model-visible tool-output policy before the result is
/// persisted in a ToolBatch or appended to the transcript. Keeping this beside
/// the loop lets fresh, recovered, and resumed paths share it without teaching
/// individual tools about sandbox storage.
pub(crate) async fn spill_tool_output(
    context: &RuntimeRunContext,
    run_id: &RunId,
    mut output: ToolOutput,
) -> Result<ToolOutput> {
    let text_only = output
        .content
        .iter()
        .all(|block| matches!(block, ContentBlock::Text { .. }));
    // Empty successful process output is still a fact. Materialize it once at
    // the neutral model-visible boundary so every provider receives a valid,
    // truthful ToolResult and individual tools/adapters do not invent their own
    // placeholders. The error row stays distinct so absence is never presented
    // as success. Structured empty-looking blocks remain untouched.
    if text_only && output.text().trim().is_empty() {
        output.content = vec![ContentBlock::text(if output.is_error {
            "Tool failed without an error message."
        } else {
            "Tool completed successfully without output."
        })];
    }
    if let Some(spiller) = &context.tool_output_spiller
        && text_only
    {
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let text = output.text();
        let spilled = context
            .physical_quiescence
            .observe_return(spiller.spill(run_id, &output.call_id, text))
            .await;
        output.content = vec![ContentBlock::text(match spilled {
            Ok(text) => text,
            Err(ToolError::Indeterminate(_)) => {
                context.physical_quiescence.mark_uncertain();
                return Err(Error::Indeterminate);
            }
            Err(error) => return Err(Error::Execution(error.to_string())),
        })];
    }
    Ok(output)
}

/// Handle the reserved client-side `tool_search` meta-tool (ADR-0053). It searches
/// the same combined static/dynamic catalog used to build the model request, reveals
/// every match by canonical id, and returns Claude-compatible `tool_reference`
/// blocks. Other providers deterministically project those references as text.
pub(super) fn execute_tool_search(
    capability: awaken_runtime_contract::permission::ToolCapabilityNarrowing,
    presentation: &ToolPresentation,
    descriptors: &[ToolDescriptor],
    call: &ToolCall,
    discovery: &mut ToolDiscoveryState,
) -> ToolOutput {
    if let Err(reason) = verify_tool_capability(capability) {
        return ToolOutput::error(&call.call_id, format!("blocked: {reason}"));
    }
    let input = match serde_json::from_value::<ToolSearchInput>(call.arguments.clone()) {
        Ok(input) => input,
        Err(error) => {
            return ToolOutput::error(
                &call.call_id,
                format!("invalid tool_search arguments: {error}"),
            );
        }
    };
    let query = match ToolSearchQuery::parse(&input.query) {
        Ok(query) => query,
        Err(error) => return ToolOutput::error(&call.call_id, error.to_string()),
    };
    if input.max_results.is_some_and(|value| {
        usize::from(value.get()) > presentation.discovery().effective_max_results()
    }) {
        return ToolOutput::error(
            &call.call_id,
            format!(
                "tool_search.max_results exceeds the configured maximum of {}",
                presentation.discovery().effective_max_results()
            ),
        );
    }
    let matches = crate::tool_discovery::search_discoverable(
        presentation,
        descriptors,
        discovery,
        &query,
        input.max_results,
    );
    if matches.is_empty() {
        return ToolOutput::ok(&call.call_id, "No deferred tools matched the query.");
    }
    for found in &matches {
        discovery.reveal(&found.canonical_id, found.descriptor.content_hash());
    }
    ToolOutput::ok_blocks(
        &call.call_id,
        matches
            .into_iter()
            .map(|found| ContentBlock::tool_reference(found.descriptor.id))
            .collect(),
    )
    .with_state(vec![ToolDiscoveryStateKey::write(discovery)])
}

/// Build the committed ticket for an awaiting tool call. Adapter execution
/// references live only in `RunDelegations`, their single durable owner.
pub(super) fn resume_ticket(
    context: &RuntimeRunContext,
    resolved: &ResolvedRun,
    run_id: &RunId,
    delegation_origin: Option<&DelegationOrigin>,
    correlation_id: &str,
    call: &ToolCall,
    reason: ToolAwaitReason,
) -> ResumeTicket {
    ResumeTicket::new(
        correlation_id,
        run_id.clone(),
        ThreadId(String::new()), // filled in finish via the commit thread id
        &resolved.snapshot_id.0,
        &resolved.spec.catalog_fingerprint.0,
        AwaitTarget::ToolCall {
            reason,
            call_id: call.call_id.clone(),
            tool: PendingTool {
                tool_id: call.tool_id.clone(),
                arguments: call.arguments.clone(),
            },
            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
        },
    )
    .with_delegation_origin(delegation_origin.cloned())
    .with_inherited_toolsets(context.inherited_toolsets.clone())
    .with_data_subject(
        context
            .capture
            .subject
            .as_ref()
            .map(|subject| subject.0.clone()),
    )
}

/// Read the pending payload from the aggregate admitted by the common resume
/// entry. Ticket coordinates select it; the ticket is not a second call store.
pub(super) fn pending_tool_call<'a>(
    batch: Option<&'a ToolBatch>,
    ticket: &ResumeTicket,
) -> Option<&'a ToolCall> {
    batch?
        .calls()
        .iter()
        .find(|entry| Some(entry.call.call_id.as_str()) == ticket.call_id())
        .map(|entry| &entry.call)
}

/// Turn a resumed result into the tool/user message(s) and any staged state.
/// An `allow` decision executes the pending tool now; a `deny` feeds a blocked
/// result; a `ToolResult`/`Input` is used directly.
pub(super) struct ResumeExecutionContext<'a> {
    pub runtime: &'a Runtime,
    pub resolved: &'a ResolvedRun,
    pub env: &'a ResolvedExecutionEnv,
    pub context: &'a RuntimeRunContext,
    pub batch: Option<&'a ToolBatch>,
    pub execution_admission: Option<ToolExecutionAdmission<'a>>,
}

pub(super) async fn resume_into_messages(
    execution: ResumeExecutionContext<'_>,
    run_id: &RunId,
    ticket: &ResumeTicket,
    result: ResumeResult,
    store: &Store,
) -> Result<(Vec<Message>, Vec<StateCommand>, Option<ToolOutput>)> {
    let ResumeExecutionContext {
        runtime,
        resolved,
        env,
        context,
        batch,
        execution_admission,
    } = execution;
    // The pending call, when the ticket carries one, so a tool-outcome hook can
    // advance a machine on the replayed result exactly like a first-time call.
    let pending_call = pending_tool_call(batch, ticket);
    match result {
        ResumeResult::Continue => Ok((Vec::new(), Vec::new(), None)),
        ResumeResult::ToolResult(output) => {
            ticket.call_id().ok_or_else(|| {
                Error::Execution("tool result resumed a ticket without a tool call".to_string())
            })?;
            // The caller journals this exact raw value before materialization.
            // Fresh, recovered and resumed calls share dispatch's sole recorder.
            Ok((Vec::new(), Vec::new(), Some(output)))
        }
        ResumeResult::Permission(decision) => {
            let (call_id, _) = ticket.tool_call().ok_or_else(|| {
                Error::Execution("permission resumed a ticket without a pending tool".to_string())
            })?;
            if matches!(decision, PermissionDecision::Allow { .. }) {
                let admission = execution_admission.ok_or_else(|| {
                    Error::Execution(
                        "approved tool resume has no committed executor admission".to_string(),
                    )
                })?;
                if admission.call_id() != call_id {
                    return Err(Error::Execution(
                        "approved tool resume executor admission names another call".to_string(),
                    ));
                }
                let call = pending_call.cloned().ok_or_else(|| {
                    Error::Execution("approved tool resume lost its validated call".into())
                })?;
                let operation_id = admission.operation_id();
                let output = execute_tool(
                    runtime,
                    Some(env),
                    Some(resolved),
                    &call,
                    context,
                    ToolExecutionOrigin {
                        run_id,
                        thread_id: &ticket.thread_id,
                        operation_id,
                    },
                    store,
                )
                .await?;
                return Ok((Vec::new(), Vec::new(), Some(output)));
            }
            let reason = match decision {
                PermissionDecision::Allow { .. } => "no pending operation".to_string(),
                PermissionDecision::Deny { reason } => {
                    reason.unwrap_or_else(|| "denied".to_string())
                }
            };
            let output = ToolOutput::error(call_id, format!("blocked: {reason}"));
            Ok((Vec::new(), Vec::new(), Some(output)))
        }
        ResumeResult::Input(text) => Ok((
            vec![Message::text(
                MessageId::resume_input(ticket.call_id().unwrap_or(&ticket.correlation_id)),
                Role::User,
                text,
            )],
            Vec::new(),
            None,
        )),
    }
}

/// One call's gate result: the outcome that drives execution and, when a real
/// gate decided (a host gate or a per-Run policy is installed), the audit fact
/// that records it (ADR-0030 D4). No gate means no audit.
pub(crate) struct GatedCall {
    pub(crate) outcome: GateOutcome,
    pub(crate) audit: Option<EventDraft>,
}

/// The serializable Run ceiling also bounds operations owned outside the Host
/// permission gate, including client execution and Runtime tool discovery.
pub(super) fn verify_tool_capability(
    capability: awaken_runtime_contract::permission::ToolCapabilityNarrowing,
) -> std::result::Result<(), &'static str> {
    capability.disabled_reason().map_or(Ok(()), Err)
}

/// Check only the Run's current restriction before a new resumed effect. A
/// refusal leaves the original ticket/call untouched, so an older confirmation
/// is neither rewritten nor used to approve a newly introduced restriction.
pub(super) async fn verify_resumed_tool_authority(
    call: &ToolCall,
    context: &RuntimeRunContext,
) -> Result<()> {
    let decision = context
        .physical_quiescence
        .observe_return(
            context
                .tool_capability_narrowing
                .evaluate_run_policy(context.tool_permission_policy.as_deref(), call),
        )
        .await;
    match decision.map(|decision| decision.verdict) {
        None | Some(awaken_runtime_contract::permission::ToolPermissionVerdict::Allow) => Ok(()),
        Some(awaken_runtime_contract::permission::ToolPermissionVerdict::Deny { reason }) => {
            Err(Error::Execution(reason))
        }
        Some(_) => Err(Error::Execution(
            "current Run restriction does not permit resumed tool execution".into(),
        )),
    }
}

/// Consult the gate chain: the host permission gate first, then any
/// plugin-contributed gates in dependency order. A call runs only if every gate
/// allows it; the first non-`Allow` outcome wins, and the host permission gate is
/// absolute (a plugin gate can further restrict but never widen it, G21). An
/// absent host gate allows (used only in tests). Gates read the run's state.
///
/// The authority is the deciding gate's own: a non-`Allow` outcome carries the
/// authority of the gate that produced it, and a final `Allow` carries the
/// outermost authored policy that allowed (the per-Run policy if it was one,
/// else the host gate's), falling back to the host gate's own authority and
/// then the per-Run narrowing's. A later gate can only restrict, so an allow it
/// merely passed through is still the earlier authority's allow. Only when
/// neither a host gate nor a per-Run policy is installed is there no authority
/// for a final `Allow` — and exactly then nothing is audited.
pub(crate) async fn gate_decision(
    runtime: &Runtime,
    call: &ToolCall,
    env: &ResolvedExecutionEnv,
    state: &Store,
    context: &RuntimeRunContext,
) -> Result<GatedCall> {
    // Gates may await independent work. They enter the same participant barrier
    // as executors; cancelling a pending gate cannot masquerade as full return.
    cancellable_tool_phase(
        context,
        context.physical_quiescence.observe_return(async {
            // Audit the decision of a real (policy-backed) gate (ADR-0030).
            let audited = runtime.gate().is_some()
                || context.tool_permission_policy.is_some()
                || !context.tool_capability_narrowing.is_configured();
            let decided = |decision: GateDecision| GatedCall {
                // The decision label is the permission-relevant view of the outcome;
                // who decided, the policy and the reason are the authority the deciding
                // gate stated, never inferred from the label.
                audit: audited.then(|| decision.audit_fact(call).into()),
                outcome: decision.outcome,
            };
            let ctx = call.clone();
            let mut allowed_on: Option<GateAuthority> = None;
            if let Some(ruling) = context
                .tool_capability_narrowing
                .evaluate_run_policy(context.tool_permission_policy.as_deref(), &ctx)
                .await
            {
                let decision = ruling.into_gate_decision();
                if !matches!(decision.outcome, GateOutcome::Allow) {
                    return decided(decision);
                }
                allowed_on = Some(decision.authority);
            }
            if let Some(gate) = runtime.gate() {
                let host = gate.gate(&ctx, state).await;
                if !matches!(host.outcome, GateOutcome::Allow) {
                    return decided(host);
                }
                allowed_on = Some(match allowed_on {
                    Some(policy @ GateAuthority::Policy(_)) => policy,
                    _ => host.authority,
                });
            }
            for gate in env.tool_gates() {
                let decision = gate.gate(&ctx, state).await;
                if !matches!(decision.outcome, GateOutcome::Allow) {
                    return decided(decision);
                }
            }
            match allowed_on {
                Some(authority) => decided(GateDecision {
                    outcome: GateOutcome::Allow,
                    authority,
                }),
                // Neither a host gate nor a per-Run policy is installed: `audited` is
                // false, so there is no decider to name and nothing to record.
                None => GatedCall {
                    outcome: GateOutcome::Allow,
                    audit: None,
                },
            }
        }),
    )
    .await
}

/// Consult the `AfterTool` phase hooks for one executed call (ADR-0055 folds the
/// former `ToolOutcomeHook` into the phase-hook model). `state` must already
/// reflect the tool's own output state; each hook reads the folded state and the
/// executed call/output on `PhaseContext::after_tool`, and may stage further state
/// and reminder messages. Returns the hooks' additional state commands and
/// messages (the tool's own output state is staged by the caller).
pub(crate) async fn collect_tool_reactions(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    step: usize,
    call: &ToolCall,
    output: &ToolOutput,
    state: &Store,
    context: &RuntimeRunContext,
) -> Result<(Vec<StateCommand>, Vec<Message>)> {
    let ctx = PhaseContext {
        run_id: run_id.clone(),
        step,
        kind: PhaseKind::AfterTool(AfterToolContext {
            call: call.clone(),
            output: output.clone(),
        }),
    };
    let reaction =
        super::run_loop::collect_phase_reactions(env, context, &ctx, &[], state, None).await?;
    Ok((reaction.state, reaction.messages))
}

/// Invoke an authorized tool, turning a missing tool or a tool error into a
/// model-visible error result rather than aborting the run. A plugin-contributed
/// dynamic tool (from `env`) takes precedence over the static registry, so an
/// MCP server's live tools resolve; `env` is `None` on the resume path, which
/// only re-runs a statically registered pending tool.
// OTel GenAI tool span: name `execute_tool {tool}`, `SpanKind::Internal`,
// `gen_ai.operation.name = "execute_tool"` with the tool name + call id.
pub(crate) struct ToolExecutionOrigin<'a> {
    pub run_id: &'a RunId,
    pub thread_id: &'a ThreadId,
    pub operation_id: String,
}

pub(crate) enum DetachedToolAction<'a> {
    Start,
    Poll(&'a ToolTaskHandle),
    Cancel(&'a ToolTaskHandle),
}

enum ToolExecutorAction<'a> {
    Invoke,
    Detached(DetachedToolAction<'a>),
}

pub(crate) enum ToolExecutorOutcome {
    Output(ToolOutput),
    Started(ToolTaskStart),
    Polled(ToolTaskPoll),
}

// Keeping every execution coordinate explicit here makes this the one invoke,
// start, poll, and cancel confluence; an argument-wrapper type would duplicate
// RuntimeRunContext and ToolExecutionOrigin solely to appease this lint.
#[allow(clippy::too_many_arguments)]
async fn execute_tool_action(
    runtime: &Runtime,
    env: Option<&ResolvedExecutionEnv>,
    resolved: Option<&ResolvedRun>,
    call: &ToolCall,
    context: &RuntimeRunContext,
    origin: ToolExecutionOrigin<'_>,
    state: &Store,
    action: ToolExecutorAction<'_>,
) -> Result<std::result::Result<ToolExecutorOutcome, ToolError>> {
    let local = LocalToolExecutor { runtime, env };
    let executor = local.routed_executor(context, &call.tool_id);
    let _admission = match context.tool_execution_admission.as_ref() {
        Some(admission) => {
            let claim = env.map_or_else(
                || executor.concurrency(&call.tool_id, &call.arguments),
                |env| tool_concurrency(runtime, env, context, call),
            );
            Some(
                admission
                    .clone()
                    .acquire(claim)
                    .await
                    .map_err(|error| Error::Execution(error.to_string()))?,
            )
        }
        None => None,
    };
    // Admission may have waited behind another Run. Recheck the claim fence at
    // the actual effect boundary rather than trusting ownership from before it.
    verify_attempt_ownership(context.ownership.as_deref()).await?;
    if !context.physical_quiescence.permits_execution() {
        return Err(Error::Indeterminate);
    }
    let started = awaken_time::mono::Stopwatch::start();
    use futures_util::FutureExt;
    let state_bound = env.and_then(|env| env.dynamic_tool_state_bound(&call.tool_id));
    let executor_state = match state_bound {
        Some(bound) => state.project(|key| bound.allows(&key.0)),
        None => Store::new(),
    };
    let operation = async {
        match action {
            ToolExecutorAction::Invoke => {
                executor.invoke(call).await.map(ToolExecutorOutcome::Output)
            }
            ToolExecutorAction::Detached(DetachedToolAction::Start) => {
                start_executor_task(executor, call)
                    .await
                    .map(ToolExecutorOutcome::Started)
            }
            ToolExecutorAction::Detached(DetachedToolAction::Poll(task)) => {
                ToolTaskContinuation::Poll
                    .continue_through(executor, call, task)
                    .await
                    .map(ToolExecutorOutcome::Polled)
            }
            ToolExecutorAction::Detached(DetachedToolAction::Cancel(task)) => {
                ToolTaskContinuation::Cancel
                    .continue_through(executor, call, task)
                    .await
                    .map(ToolExecutorOutcome::Polled)
            }
        }
    };
    let invocation = with_tool_state_context(
        executor_state,
        with_tool_operation_context(
            ToolOperationContext {
                run_id: Some(origin.run_id.clone()),
                thread_id: Some(origin.thread_id.clone()),
                operation_id: origin.operation_id,
                call_id: Some(call.call_id.clone()),
                execution_scope: context.execution_scope.clone(),
            },
            async {
                match resolved {
                    Some(resolved) => {
                        with_tool_execution_facts(
                            Arc::new(CurrentToolExecutionFacts::new(
                                runtime, env, resolved, context,
                            )),
                            operation,
                        )
                        .await
                    }
                    None => operation.await,
                }
            },
        ),
    );
    // Arm only after admission and the ownership recheck. The same observer
    // used by model execution covers invoke/start/poll/cancel, including Future
    // drop and unwind. Catching a panic outside it must not fabricate return.
    let mut outcome =
        match std::panic::AssertUnwindSafe(context.physical_quiescence.observe_return(invocation))
            .catch_unwind()
            .await
        {
            Ok(outcome) => outcome,
            Err(_panic) => Err(ToolError::Indeterminate(format!(
                "tool `{}` panicked during execution",
                call.tool_id
            ))),
        };
    if let Ok(ToolExecutorOutcome::Started(ToolTaskStart::Pending(handle))) = &outcome {
        outcome = handle
            .validate_created()
            .map(|()| ToolExecutorOutcome::Started(ToolTaskStart::Pending(handle.clone())));
    }
    // Include post-creation validation before recording the one monotonic veto.
    // A malformed returned handle still names possibly live external work.
    // No await or result normalization may erase this uncertainty, and replay
    // capability says nothing about physical termination or locality.
    if let Some(bound) = state_bound {
        let invalid_command = match &outcome {
            Ok(ToolExecutorOutcome::Output(output))
            | Ok(ToolExecutorOutcome::Started(ToolTaskStart::Completed(output)))
            | Ok(ToolExecutorOutcome::Polled(ToolTaskPoll::Completed(output))) => output
                .state
                .iter()
                .find(|command| !bound.allows(&command.key.0)),
            _ => None,
        };
        if let Some(command) = invalid_command {
            outcome = Err(ToolError::DefinitiveFailure(format!(
                "tool `{}` attempted state key {:?} outside its plugin capability",
                call.tool_id, command.key.0
            )));
        }
    }
    runtime.metrics().record_tool(
        &call.tool_id,
        if outcome.is_err() { "error" } else { "ok" },
        started.elapsed(),
    );
    tool_executor_result(context, call, outcome)
}

/// Preserve fatal executor evidence at both metadata and invocation boundaries.
/// Ordinary pre-entry errors remain values for the batch's one result recorder.
pub(super) fn tool_executor_result<T>(
    context: &RuntimeRunContext,
    call: &ToolCall,
    outcome: std::result::Result<T, ToolError>,
) -> Result<std::result::Result<T, ToolError>> {
    match outcome {
        Err(ToolError::Indeterminate(_)) => {
            context.physical_quiescence.mark_uncertain();
            Err(Error::Indeterminate)
        }
        Err(ToolError::StateInvalidatedAfterDispatch(detail)) => {
            tracing::error!(tool_id = %call.tool_id, call_id = %call.call_id, %detail,
                "tool effect completed after its executor projection became unavailable");
            Err(Error::StateConflict)
        }
        other => Ok(other),
    }
}

#[tracing::instrument(
    name = "execute_tool",
    skip_all,
    fields(
        otel.name = format_args!("execute_tool {}", call.tool_id),
        otel.kind = "internal",
        gen_ai.operation.name = "execute_tool",
        gen_ai.tool.name = %call.tool_id,
        gen_ai.tool.call.id = %call.call_id,
        error.type = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    )
)]
pub(crate) async fn execute_tool(
    runtime: &Runtime,
    env: Option<&ResolvedExecutionEnv>,
    resolved: Option<&ResolvedRun>,
    call: &ToolCall,
    context: &RuntimeRunContext,
    origin: ToolExecutionOrigin<'_>,
    state: &Store,
) -> Result<ToolOutput> {
    let span = tracing::Span::current();
    let output = match cancellable_tool_phase(
        context,
        execute_tool_action(
            runtime,
            env,
            resolved,
            call,
            context,
            origin,
            state,
            ToolExecutorAction::Invoke,
        ),
    )
    .await??
    {
        Ok(ToolExecutorOutcome::Output(output)) => output,
        Ok(_) => unreachable!("invoke action returns only ToolExecutorOutcome::Output"),
        Err(error) => ToolOutput::error(&call.call_id, error.to_string()),
    };
    // OTel: a tool that returned an error (unknown tool, invocation failure, or a
    // model-visible error result) marks the span ERROR with a `gen_ai`-shaped type.
    if output.is_error {
        span.record("error.type", "tool_error");
        span.record("otel.status_code", "ERROR");
    }
    // Structure-only metric at the tool chokepoint (#2): tool id + outcome class +
    // latency. The tool id is a declared identifier, never content.
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_detached_tool_action(
    runtime: &Runtime,
    env: &ResolvedExecutionEnv,
    resolved: &ResolvedRun,
    call: &ToolCall,
    context: &RuntimeRunContext,
    origin: ToolExecutionOrigin<'_>,
    state: &Store,
    action: DetachedToolAction<'_>,
) -> Result<std::result::Result<ToolExecutorOutcome, ToolError>> {
    execute_tool_action(
        runtime,
        Some(env),
        Some(resolved),
        call,
        context,
        origin,
        state,
        ToolExecutorAction::Detached(action),
    )
    .await
}

/// The in-process `ToolExecutor` (ADR-0044 D1): the degenerate case where the
/// hand runs in the brain's own process. It resolves the call by id against the
/// run's dynamic tools and the runtime registry — the historical lookup — and is
/// used whenever a run wires no remote executor. Borrowing keeps it allocation-
/// free per call; it is invoked directly, never as a stored `dyn`.
struct LocalToolExecutor<'a> {
    runtime: &'a Runtime,
    env: Option<&'a ResolvedExecutionEnv>,
}

struct CurrentToolExecutionFacts {
    descriptors: std::collections::BTreeMap<String, ToolDescriptor>,
    tools: std::collections::BTreeMap<String, Arc<dyn RawTool>>,
    sandbox: Option<Arc<dyn ToolExecutor>>,
    constraints: Vec<Arc<dyn ToolConcurrencyConstraint>>,
    stateful_dynamic_tools: std::collections::BTreeSet<String>,
}

impl CurrentToolExecutionFacts {
    fn new(
        runtime: &Runtime,
        env: Option<&ResolvedExecutionEnv>,
        resolved: &ResolvedRun,
        context: &RuntimeRunContext,
    ) -> Self {
        let dynamic = env.map_or_else(Vec::new, ResolvedExecutionEnv::dynamic_descriptors);
        let descriptors = executable_tool_descriptors(&resolved.spec, &dynamic)
            .into_iter()
            .map(|descriptor| (descriptor.id.clone(), descriptor))
            .collect();
        let mut tools = std::collections::BTreeMap::new();
        for id in resolved
            .spec
            .tool_descriptors
            .iter()
            .map(|descriptor| descriptor.id.as_str())
            .chain(dynamic.iter().map(|descriptor| descriptor.id.as_str()))
        {
            let tool = env
                .and_then(|env| env.dynamic_tool(id))
                .or_else(|| runtime.tool(id).cloned());
            if let Some(tool) = tool {
                tools.insert(id.to_string(), tool);
            }
        }
        let stateful_dynamic_tools = env
            .map(|env| {
                dynamic
                    .iter()
                    .filter(|descriptor| {
                        env.dynamic_tool_state_bound(&descriptor.id)
                            .is_some_and(|bound| !bound.is_deny_all())
                    })
                    .map(|descriptor| descriptor.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            descriptors,
            tools,
            sandbox: context.tool_executor.clone(),
            constraints: env
                .map(|env| env.tool_constraints().to_vec())
                .unwrap_or_default(),
            stateful_dynamic_tools,
        }
    }
}

impl ToolExecutionFactsResolver for CurrentToolExecutionFacts {
    fn resolve(&self, call: &ToolCall) -> std::result::Result<ToolExecutionFacts, ToolError> {
        let descriptor = self
            .descriptors
            .get(&call.tool_id)
            .filter(|descriptor| {
                matches!(descriptor.kind, ToolKind::Regular | ToolKind::DetachedOnly)
            })
            .ok_or_else(|| ToolError::UnknownTool(call.tool_id.clone()))?;
        if self.stateful_dynamic_tools.contains(&call.tool_id) {
            return Err(ToolError::DefinitiveFailure(format!(
                "stateful tool `{}` cannot be detached because its State commands require its owning Runtime commit path",
                call.tool_id
            )));
        }
        let tool = self
            .tools
            .get(&call.tool_id)
            .ok_or_else(|| ToolError::UnknownTool(call.tool_id.clone()))?;
        let (capability, concurrency) = match tool.execution_target() {
            ToolExecutionTarget::Brain => (
                tool.recovery_capability(),
                intrinsic_tool_concurrency(ToolConcurrencySource::Brain(tool.as_ref()), call),
            ),
            ToolExecutionTarget::Sandbox => {
                let sandbox = self.sandbox.as_deref().ok_or_else(|| {
                    ToolError::DefinitiveFailure(format!(
                        "sandbox executor unavailable for tool `{}`",
                        call.tool_id
                    ))
                })?;
                (
                    sandbox.recovery_capability(&call.tool_id),
                    intrinsic_tool_concurrency(ToolConcurrencySource::Sandbox(sandbox), call),
                )
            }
        };
        descriptor
            .recovery_policy
            .validate(capability)
            .map_err(|error| ToolError::DefinitiveFailure(error.to_string()))?;
        let concurrency = constrain_tool_concurrency(concurrency, &self.constraints, call);
        Ok(ToolExecutionFacts {
            recovery: descriptor.recovery_policy.clone(),
            concurrency,
        })
    }
}

impl LocalToolExecutor<'_> {
    fn routed_executor<'a>(
        &'a self,
        context: &'a RuntimeRunContext,
        id: &str,
    ) -> &'a dyn ToolExecutor {
        static MISSING: MissingSandboxExecutor = MissingSandboxExecutor;
        match self.execution_target(id) {
            Some(ToolExecutionTarget::Sandbox) => {
                context.tool_executor.as_deref().unwrap_or(&MISSING)
            }
            Some(ToolExecutionTarget::Brain) | None => self,
        }
    }
    fn tool(
        &self,
        tool_id: &str,
    ) -> Option<std::sync::Arc<dyn awaken_tool_contract::tool::RawTool>> {
        self.env
            .and_then(|env| env.dynamic_tool(tool_id))
            .or_else(|| self.runtime.tool(tool_id).cloned())
    }

    fn execution_target(&self, tool_id: &str) -> Option<ToolExecutionTarget> {
        self.tool(tool_id).map(|tool| tool.execution_target())
    }
}

#[async_trait::async_trait]
impl ToolExecutor for LocalToolExecutor<'_> {
    async fn execution_domain(
        &self,
        call: &ToolCall,
    ) -> std::result::Result<awaken_tool_contract::tool_batch::ToolExecutionDomain, ToolError> {
        Ok(self
            .tool(&call.tool_id)
            .map_or(Default::default(), |tool| tool.execution_domain()))
    }
    fn recovery_capability(&self, tool_id: &str) -> ToolRecoveryCapability {
        self.tool(tool_id)
            .map_or(ToolRecoveryCapability::NonRecoverable, |tool| {
                tool.recovery_capability()
            })
    }

    fn concurrency(&self, tool_id: &str, arguments: &serde_json::Value) -> ToolConcurrency {
        self.tool(tool_id)
            .map_or_else(ToolConcurrency::default, |tool| tool.concurrency(arguments))
    }

    async fn invoke(&self, call: &ToolCall) -> std::result::Result<ToolOutput, ToolError> {
        let tool = self.tool(&call.tool_id);
        match tool {
            Some(tool) => tool.invoke(call.clone()).await,
            None => Err(ToolError::UnknownTool(call.tool_id.clone())),
        }
    }

    fn durable_tasks(&self) -> Option<&dyn DurableTaskExecutor> {
        Some(self)
    }
}

pub(super) async fn prepare_tool_domain(
    runtime: &Runtime,
    env: &ResolvedExecutionEnv,
    context: &RuntimeRunContext,
    call: &ToolCall,
) -> Result<std::result::Result<awaken_tool_contract::tool_batch::ToolExecutionDomain, ToolOutput>>
{
    cancellable_tool_phase(
        context,
        verify_attempt_ownership(context.ownership.as_deref()),
    )
    .await??;
    if !context.physical_quiescence.permits_execution() {
        return Err(Error::Indeterminate);
    }
    let local = LocalToolExecutor {
        runtime,
        env: Some(env),
    };
    let result = cancellable_tool_phase(
        context,
        context.physical_quiescence.observe_return(
            local
                .routed_executor(context, &call.tool_id)
                .execution_domain(call),
        ),
    )
    .await?;
    Ok(tool_executor_result(context, call, result)?
        .map_err(|error| ToolOutput::error(&call.call_id, error.to_string()))
        .and_then(|domain| {
            domain
                .bind_resource_origin(context.resource_origin.as_ref())
                .map_err(|error| ToolOutput::error(&call.call_id, error.to_string()))
        }))
}

/// Consume only the aggregate's actually admitted, uncommitted new entry.
/// Recovery/wait rejection happens before this shell can perform lazy I/O.
pub(super) async fn enter_pending_tool(
    runtime: &Runtime,
    env: &ResolvedExecutionEnv,
    context: &RuntimeRunContext,
    call: &ToolCall,
    entry: awaken_tool_contract::tool_batch::PendingToolExecutionEntry<'_>,
) -> Result<()> {
    verify_resumed_tool_authority(call, context).await?;
    let domain = prepare_tool_domain(runtime, env, context, call)
        .await?
        .map_err(|output| Error::Execution(output.text()))?;
    entry
        .enter(Some(domain))
        .map_err(|error| Error::Execution(error.to_string()))?;
    Ok(())
}

#[async_trait::async_trait]
impl DurableTaskExecutor for LocalToolExecutor<'_> {
    async fn start_task(&self, call: &ToolCall) -> std::result::Result<ToolTaskStart, ToolError> {
        let tool = self.tool(&call.tool_id);
        match tool {
            Some(tool) => start_tool_task(tool.as_ref(), call.clone()).await,
            None => Err(ToolError::UnknownTool(call.tool_id.clone())),
        }
    }

    async fn poll_task(
        &self,
        call: &ToolCall,
        task: &ToolTaskHandle,
    ) -> std::result::Result<ToolTaskPoll, ToolError> {
        let tool = self.tool(&call.tool_id);
        match tool {
            Some(tool) => {
                ToolTaskContinuation::Poll
                    .continue_on(tool.as_ref(), call, task)
                    .await
            }
            None => Err(ToolError::UnknownTool(call.tool_id.clone())),
        }
    }

    async fn cancel_task(
        &self,
        call: &ToolCall,
        task: &ToolTaskHandle,
    ) -> std::result::Result<ToolTaskPoll, ToolError> {
        let tool = self.tool(&call.tool_id);
        match tool {
            Some(tool) => {
                ToolTaskContinuation::Cancel
                    .continue_on(tool.as_ref(), call, task)
                    .await
            }
            None => Err(ToolError::UnknownTool(call.tool_id.clone())),
        }
    }
}

/// Resolve the complete concurrency claim through invocation's placement and
/// the resolved plugins' ordered constraints. An unknown tool stays Serial;
/// an absent Sandbox executor retains its intrinsic default while invocation
/// rejects that missing executor at its existing boundary.
pub(crate) fn tool_concurrency(
    runtime: &Runtime,
    env: &ResolvedExecutionEnv,
    context: &RuntimeRunContext,
    call: &ToolCall,
) -> ToolConcurrency {
    let local = LocalToolExecutor {
        runtime,
        env: Some(env),
    };
    let intrinsic = local
        .tool(&call.tool_id)
        .map_or(ToolConcurrency::Serial, |tool| {
            let source = match tool.execution_target() {
                ToolExecutionTarget::Brain => ToolConcurrencySource::Brain(tool.as_ref()),
                ToolExecutionTarget::Sandbox => match context.tool_executor.as_deref() {
                    Some(executor) => ToolConcurrencySource::Sandbox(executor),
                    None => ToolConcurrencySource::MissingSandbox,
                },
            };
            intrinsic_tool_concurrency(source, call)
        });
    constrain_tool_concurrency(intrinsic, env.tool_constraints(), call)
}

/// The selected source of an intrinsic claim, not execution authority.
/// Missing Sandbox wiring keeps the conservative claim while invocation and
/// immutable-catalog admission retain their existing fail-closed boundaries.
enum ToolConcurrencySource<'a> {
    Brain(&'a dyn RawTool),
    Sandbox(&'a dyn ToolExecutor),
    MissingSandbox,
}

/// Placement's intrinsic claim, shared with the immutable execution catalog.
/// The catalog validates missing Sandbox wiring before using this calculation.
fn intrinsic_tool_concurrency(
    source: ToolConcurrencySource<'_>,
    call: &ToolCall,
) -> ToolConcurrency {
    match source {
        ToolConcurrencySource::Brain(tool) => tool.concurrency(&call.arguments),
        ToolConcurrencySource::Sandbox(executor) => {
            executor.concurrency(&call.tool_id, &call.arguments)
        }
        ToolConcurrencySource::MissingSandbox => ToolConcurrency::default(),
    }
}

/// One ordered composition for ordinary placement and special intrinsic claims.
/// Evaluate every contribution, including after Serial, so a trait callback is
/// observed once in order at this boundary rather than relying on idempotence.
pub(super) fn constrain_tool_concurrency(
    intrinsic: ToolConcurrency,
    constraints: &[Arc<dyn ToolConcurrencyConstraint>],
    call: &ToolCall,
) -> ToolConcurrency {
    constraints.iter().fold(intrinsic, |claim, constraint| {
        claim.narrowed_with(constraint.constrain(call))
    })
}

/// Resolve recovery through the same placement decision as invocation.
pub(crate) fn tool_recovery_capability(
    runtime: &Runtime,
    env: &ResolvedExecutionEnv,
    context: &RuntimeRunContext,
    tool_id: &str,
) -> ToolRecoveryCapability {
    let local = LocalToolExecutor {
        runtime,
        env: Some(env),
    };
    match local.execution_target(tool_id) {
        Some(ToolExecutionTarget::Sandbox) => context
            .tool_executor
            .as_deref()
            .map_or(ToolRecoveryCapability::NonRecoverable, |executor| {
                executor.recovery_capability(tool_id)
            }),
        Some(ToolExecutionTarget::Brain) => local.recovery_capability(tool_id),
        None => ToolRecoveryCapability::NonRecoverable,
    }
}

struct MissingSandboxExecutor;

#[async_trait::async_trait]
impl ToolExecutor for MissingSandboxExecutor {
    async fn invoke(&self, call: &ToolCall) -> std::result::Result<ToolOutput, ToolError> {
        Err(ToolError::DefinitiveFailure(format!(
            "sandbox executor unavailable for tool `{}`",
            call.tool_id
        )))
    }
}
