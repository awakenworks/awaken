//! Tool authorization, execution selection, and resumed-result folding.

use super::*;

/// Apply the Session's one model-visible tool-output policy before the result is
/// persisted in a ToolBatch or appended to the transcript. Keeping this beside
/// the loop lets fresh, recovered, and resumed paths share it without teaching
/// individual tools about sandbox storage.
pub(super) async fn spill_tool_output(
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

/// Handle a call to the reserved `tool_open` meta-tool (ADR-0053): mark the requested
/// tool opened for the rest of the run so its full schema is sent next step. The `name`
/// is reverse-mapped to its canonical id; an unknown or non-deferred name is reported
/// back to the model rather than silently accepted.
pub(super) fn open_deferred_tool(
    presentation: &ToolPresentation,
    call: &ToolCall,
    opened: &mut std::collections::BTreeSet<String>,
) -> ToolOutput {
    let name = call
        .arguments
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let canonical = presentation.resolve(name);
    if presentation.is_deferred(canonical) {
        opened.insert(canonical.to_string());
        ToolOutput::ok(
            &call.call_id,
            format!("Tool `{name}` is now available; call it directly on your next step."),
        )
    } else {
        ToolOutput::error(&call.call_id, format!("no deferred tool named `{name}`"))
    }
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
pub(super) async fn resume_into_messages(
    runtime: &Runtime,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    ticket: &ResumeTicket,
    result: ResumeResult,
    store: &Store,
    context: &RuntimeRunContext,
) -> Result<(Vec<Message>, Vec<StateCommand>, Option<ToolOutput>)> {
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
                    &call,
                    context,
                    run_id,
                    &ticket.thread_id,
                    operation_id,
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
pub(super) async fn gate_decision(
    runtime: &Runtime,
    call: &ToolCall,
    env: &ResolvedExecutionEnv,
    state: &Store,
    context: &RuntimeRunContext,
) -> GateOutcome {
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
pub(super) async fn collect_tool_reactions(
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
pub(super) async fn execute_tool(
    runtime: &Runtime,
    env: Option<&ResolvedExecutionEnv>,
    call: &ToolCall,
    context: &RuntimeRunContext,
    run_id: &RunId,
    thread_id: &ThreadId,
    operation_id: String,
) -> Result<ToolOutput> {
    let span = tracing::Span::current();
    // Resolve placement per tool. A placed Hand must never capture Brain tools
    // such as MCP or Skills, while a Sandbox tool must never silently execute in
    // the Brain when placement is unavailable.
    let local = LocalToolExecutor { runtime, env };
    let missing_sandbox = MissingSandboxExecutor;
    let executor: &dyn ToolExecutor = match local.execution_target(&call.tool_id) {
        Some(ToolExecutionTarget::Sandbox) => {
            context.tool_executor.as_deref().unwrap_or(&missing_sandbox)
        }
        Some(ToolExecutionTarget::Brain) | None => &local,
    };
    verify_attempt_ownership(context.ownership.as_deref()).await?;
    let started = std::time::Instant::now();
    // Fault isolation at the SPI boundary: a `RawTool` (MCP / plugin / skill — often
    // third-party) that PANICS must not take down the run. Catch the unwind here, at
    // the single execute-tool confluence, and map it to a model-visible error just like
    // an `Err` — so unknown/invalid-args/execution/panic all fail closed identically.
    use futures_util::FutureExt;
    let invocation = with_tool_operation_context(
        ToolOperationContext {
            run_id: Some(run_id.clone()),
            thread_id: Some(thread_id.clone()),
            operation_id,
            call_id: Some(call.call_id.clone()),
            execution_scope: context.execution_scope.clone(),
        },
        executor.invoke(call),
    );
    let output = match std::panic::AssertUnwindSafe(invocation)
        .catch_unwind()
        .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => ToolOutput::error(&call.call_id, err.to_string()),
        Err(_panic) => ToolOutput::error(
            &call.call_id,
            format!("tool `{}` panicked during execution", call.tool_id),
        ),
    };
    // OTel: a tool that returned an error (unknown tool, invocation failure, or a
    // model-visible error result) marks the span ERROR with a `gen_ai`-shaped type.
    if output.is_error {
        span.record("error.type", "tool_error");
        span.record("otel.status_code", "ERROR");
    }
    // Structure-only metric at the tool chokepoint (#2): tool id + outcome class +
    // latency. The tool id is a declared identifier, never content.
    runtime.metrics().record_tool(
        &call.tool_id,
        if output.is_error { "error" } else { "ok" },
        started.elapsed(),
    );
    Ok(output)
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

    async fn invoke(&self, call: &ToolCall) -> std::result::Result<ToolOutput, ToolError> {
        let tool = self.tool(&call.tool_id);
        match tool {
            Some(tool) => tool.invoke(call.clone()).await,
            None => Err(ToolError::Unknown(call.tool_id.clone())),
        }
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
