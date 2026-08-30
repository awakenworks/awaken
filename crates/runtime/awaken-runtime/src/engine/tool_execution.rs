//! Tool authorization, execution selection, and resumed-result folding.

use super::*;
use std::sync::Arc;

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
        output.content = vec![ContentBlock::text(
            spiller
                .spill(run_id, &output.call_id, text)
                .await
                .map_err(|error| Error::Execution(error.to_string()))?,
        )];
    }
    Ok(output)
}

/// Build the audit draft for one gated tool call (ADR-0030). The decision label
/// is the permission-relevant view of the gate outcome.
pub(super) fn permission_audit(call: &ToolCall, outcome: &GateOutcome) -> EventDraft {
    RunEvent::PermissionDecided {
        tool_id: call.tool_id.clone(),
        call_id: call.call_id.clone(),
        decision: outcome.decision_label().to_string(),
    }
    .into()
}

/// Handle the reserved client-side `tool_search` meta-tool (ADR-0053). It searches
/// the same combined static/dynamic catalog used to build the model request, reveals
/// every match by canonical id, and returns Claude-compatible `tool_reference`
/// blocks. Other providers deterministically project those references as text.
pub(super) fn execute_tool_search(
    presentation: &ToolPresentation,
    descriptors: &[ToolDescriptor],
    call: &ToolCall,
    discovery: &mut ToolDiscoveryState,
) -> ToolOutput {
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
        },
    )
    .with_delegation_origin(delegation_origin.cloned())
    .with_data_subject(
        context
            .capture
            .subject
            .as_ref()
            .map(|subject| subject.0.clone()),
    )
}

/// Turn a resumed result into the tool/user message(s) and any staged state.
/// An `allow` decision executes the pending tool now; a `deny` feeds a blocked
/// result; a `ToolResult`/`Input` is used directly.
pub(super) struct ResumeExecutionContext<'a> {
    pub runtime: &'a Runtime,
    pub resolved: &'a ResolvedRun,
    pub env: &'a ResolvedExecutionEnv,
    pub context: &'a RuntimeRunContext,
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
    } = execution;
    // The pending call, when the ticket carries one, so a tool-outcome hook can
    // advance a machine on the replayed result exactly like a first-time call.
    let pending_call = |output: &ToolOutput| {
        ticket.pending_tool().map(|pending| ToolCall {
            call_id: output.call_id.clone(),
            tool_id: pending.tool_id.clone(),
            arguments: pending.arguments.clone(),
        })
    };
    match result {
        ResumeResult::Continue => Ok((Vec::new(), Vec::new(), None)),
        ResumeResult::ToolResult(output) => {
            let call_id = ticket.call_id().ok_or_else(|| {
                Error::Execution("tool result resumed a ticket without a tool call".to_string())
            })?;
            let output = spill_tool_output(context, run_id, output).await?;
            let call = pending_call(&output);
            let (messages, state) =
                fold_resume_tool_output(env, run_id, call_id, call, &output, store).await;
            Ok((messages, state, Some(output)))
        }
        ResumeResult::Permission(decision) => {
            let (call_id, pending) = ticket.tool_call().ok_or_else(|| {
                Error::Execution("permission resumed a ticket without a pending tool".to_string())
            })?;
            if matches!(decision, PermissionDecision::Allow { .. }) {
                let call = ToolCall {
                    call_id: call_id.to_string(),
                    tool_id: pending.tool_id.clone(),
                    arguments: pending.arguments.clone(),
                };
                let operation_id = format!("tool-resume:{}:{}", run_id.0, call.call_id);
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
                let output = spill_tool_output(context, run_id, output).await?;
                let (messages, state) =
                    fold_resume_tool_output(env, run_id, call_id, Some(call), &output, store).await;
                return Ok((messages, state, Some(output)));
            }
            let reason = match decision {
                PermissionDecision::Allow { .. } => "no pending operation".to_string(),
                PermissionDecision::Deny { reason } => {
                    reason.unwrap_or_else(|| "denied".to_string())
                }
            };
            let output = ToolOutput::error(call_id, format!("blocked: {reason}"));
            let output = spill_tool_output(context, run_id, output).await?;
            let messages = vec![tool_result_message_from(
                call_id,
                &output.content,
                output.is_error,
            )];
            Ok((messages, Vec::new(), Some(output)))
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

/// Consult the gate chain: the host permission gate first, then any
/// plugin-contributed gates in dependency order. A call runs only if every gate
/// allows it; the first non-`Allow` outcome wins, and the host permission gate is
/// absolute (a plugin gate can further restrict but never widen it, G21). An
/// absent host gate allows (used only in tests). Gates read the run's state.
pub(crate) async fn gate_decision(
    runtime: &Runtime,
    call: &ToolCall,
    env: &ResolvedExecutionEnv,
    state: &Store,
    context: &RuntimeRunContext,
) -> GateOutcome {
    if context.tool_capability_narrowing
        == awaken_runtime_contract::permission::ToolCapabilityNarrowing::DenyAll
    {
        return GateOutcome::Block {
            reason: "tools are disabled for this Run".into(),
        };
    }
    let ctx = ToolCall {
        tool_id: call.tool_id.clone(),
        call_id: call.call_id.clone(),
        arguments: call.arguments.clone(),
    };
    if let Some(narrowing) = &context.tool_permission_policy {
        let outcome = narrowing.evaluate(&ctx).await.into_gate_outcome();
        if !matches!(outcome, GateOutcome::Allow) {
            return outcome;
        }
    }
    let host = match runtime.gate() {
        Some(gate) => gate.gate(&ctx, state).await,
        None => GateOutcome::Allow,
    };
    if !matches!(host, GateOutcome::Allow) {
        return host;
    }
    for gate in env.tool_gates() {
        let outcome = gate.gate(&ctx, state).await;
        if !matches!(outcome, GateOutcome::Allow) {
            return outcome;
        }
    }
    GateOutcome::Allow
}

/// Fold a resumed tool result into `(messages, state)` off a throwaway store copy
/// (the resume paths return staged effects rather than mutating the live store).
/// The result message is minted from `call_id`; when a `call` is present (the
/// replayed call the hooks advance on) its output state is applied to a `work`
/// clone and the `AfterTool` reactions/reminders are folded in. The one home for
/// the resume arms' outcome folding (`ToolResult` and an allowed `Decision`).
pub(super) async fn fold_resume_tool_output(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    call_id: &str,
    call: Option<ToolCall>,
    output: &ToolOutput,
    store: &Store,
) -> (Vec<Message>, Vec<StateCommand>) {
    let mut state = output.state.clone();
    let mut messages = vec![tool_result_message_from(
        call_id,
        &output.content,
        output.is_error,
    )];
    if let Some(call) = call {
        let mut work = store.clone();
        for command in &output.state {
            work.apply(command);
        }
        let (reactions, reminders) =
            collect_tool_reactions(env, run_id, 0, &call, output, &work).await;
        state.extend(reactions);
        messages.extend(reminders);
    }
    (messages, state)
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
) -> (Vec<StateCommand>, Vec<Message>) {
    let mut work = state.clone();
    let mut commands: Vec<StateCommand> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    let ctx = PhaseContext {
        run_id: run_id.clone(),
        step,
        kind: PhaseKind::AfterTool(AfterToolContext {
            call: call.clone(),
            output: output.clone(),
        }),
    };
    for hook in env.hooks_for(PhaseHookPoint::AfterTool) {
        let reaction = hook.on_phase(&ctx, &[], &work).await;
        for command in &reaction.state {
            work.apply(command);
            commands.push(command.clone());
        }
        messages.extend(reaction.messages);
    }
    (commands, messages)
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
    let missing_sandbox = MissingSandboxExecutor;
    let executor: &dyn ToolExecutor = match local.execution_target(&call.tool_id) {
        Some(ToolExecutionTarget::Sandbox) => {
            context.tool_executor.as_deref().unwrap_or(&missing_sandbox)
        }
        Some(ToolExecutionTarget::Brain) | None => &local,
    };
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
    let started = std::time::Instant::now();
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
            ToolExecutorAction::Detached(DetachedToolAction::Start) => executor
                .start_task(call)
                .await
                .map(ToolExecutorOutcome::Started),
            ToolExecutorAction::Detached(DetachedToolAction::Poll(task)) => executor
                .poll_task(call, task)
                .await
                .map(ToolExecutorOutcome::Polled),
            ToolExecutorAction::Detached(DetachedToolAction::Cancel(task)) => executor
                .cancel_task(call, task)
                .await
                .map(ToolExecutorOutcome::Polled),
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
    let mut outcome = match std::panic::AssertUnwindSafe(invocation)
        .catch_unwind()
        .await
    {
        Ok(outcome) => outcome,
        Err(_panic) => Err(ToolError::Execution(format!(
            "tool `{}` panicked during execution",
            call.tool_id
        ))),
    };
    if let Ok(ToolExecutorOutcome::Started(ToolTaskStart::Pending(handle))) = &outcome {
        outcome = handle
            .validate()
            .map(|()| ToolExecutorOutcome::Started(ToolTaskStart::Pending(handle.clone())));
    }
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
            outcome = Err(ToolError::Execution(format!(
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
    Ok(outcome)
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
    let output = match execute_tool_action(
        runtime,
        env,
        resolved,
        call,
        context,
        origin,
        state,
        ToolExecutorAction::Invoke,
    )
    .await?
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
            .ok_or_else(|| ToolError::Unknown(call.tool_id.clone()))?;
        if self.stateful_dynamic_tools.contains(&call.tool_id) {
            return Err(ToolError::Execution(format!(
                "stateful tool `{}` cannot be detached because its State commands require its owning Runtime commit path",
                call.tool_id
            )));
        }
        let tool = self
            .tools
            .get(&call.tool_id)
            .ok_or_else(|| ToolError::Unknown(call.tool_id.clone()))?;
        let (capability, concurrency) = match tool.execution_target() {
            ToolExecutionTarget::Brain => (
                tool.recovery_capability(),
                tool.concurrency(&call.arguments),
            ),
            ToolExecutionTarget::Sandbox => {
                let sandbox = self.sandbox.as_deref().ok_or_else(|| {
                    ToolError::Execution(format!(
                        "sandbox executor unavailable for tool `{}`",
                        call.tool_id
                    ))
                })?;
                (
                    sandbox.recovery_capability(&call.tool_id),
                    sandbox.concurrency(&call.tool_id, &call.arguments),
                )
            }
        };
        descriptor
            .recovery_policy
            .validate(capability)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let concurrency = self
            .constraints
            .iter()
            .fold(concurrency, |claim, constraint| {
                claim.narrowed_with(constraint.constrain(call))
            });
        Ok(ToolExecutionFacts {
            recovery: descriptor.recovery_policy.clone(),
            concurrency,
        })
    }
}

impl LocalToolExecutor<'_> {
    fn tool(
        &self,
        tool_id: &str,
    ) -> Option<std::sync::Arc<dyn awaken_runtime_contract::tool::RawTool>> {
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
            None => Err(ToolError::Unknown(call.tool_id.clone())),
        }
    }

    async fn start_task(&self, call: &ToolCall) -> std::result::Result<ToolTaskStart, ToolError> {
        let tool = self.tool(&call.tool_id);
        match tool {
            Some(tool) => tool.start_task(call.clone()).await,
            None => Err(ToolError::Unknown(call.tool_id.clone())),
        }
    }

    async fn poll_task(
        &self,
        call: &ToolCall,
        task: &ToolTaskHandle,
    ) -> std::result::Result<ToolTaskPoll, ToolError> {
        let tool = self.tool(&call.tool_id);
        match tool {
            Some(tool) => tool.poll_task(call, task).await,
            None => Err(ToolError::Unknown(call.tool_id.clone())),
        }
    }

    async fn cancel_task(
        &self,
        call: &ToolCall,
        task: &ToolTaskHandle,
    ) -> std::result::Result<ToolTaskPoll, ToolError> {
        let tool = self.tool(&call.tool_id);
        match tool {
            Some(tool) => tool.cancel_task(call, task).await,
            None => Err(ToolError::Unknown(call.tool_id.clone())),
        }
    }
}

/// Resolve concurrency through the same placement decision as invocation. A
/// missing Sandbox executor and an unknown tool both fail closed to sequential.
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
    match local.execution_target(&call.tool_id) {
        Some(ToolExecutionTarget::Sandbox) => context
            .tool_executor
            .as_deref()
            .map_or_else(ToolConcurrency::default, |executor| {
                executor.concurrency(&call.tool_id, &call.arguments)
            }),
        Some(ToolExecutionTarget::Brain) => local.concurrency(&call.tool_id, &call.arguments),
        None => ToolConcurrency::Serial,
    }
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
        Err(ToolError::Execution(format!(
            "sandbox executor unavailable for tool `{}`",
            call.tool_id
        )))
    }
}
