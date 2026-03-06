use super::AgentLoopError;
use crate::contracts::runtime::behavior::{build_read_only_context_from_step, AgentBehavior};
use crate::contracts::runtime::inference::{InferenceError, LLMResponse};
use crate::contracts::runtime::phase::{
    ActionSet, AfterInferenceAction, AfterToolExecuteAction, BeforeInferenceAction,
    BeforeToolExecuteAction, LifecycleAction, Phase, StepContext,
};
use crate::contracts::runtime::run::RunAction;
use crate::contracts::runtime::state::{reduce_state_actions, ScopeContext};
use crate::contracts::runtime::tool_call::ToolDescriptor;
use crate::contracts::RunContext;
use crate::contracts::ToolCallContext;
use std::sync::Mutex;
use tirea_state::{DocCell, TrackedPatch};

// =========================================================================
// Action application — typed dispatch, no dynamic dispatch
// =========================================================================

fn apply_lifecycle_actions(step: &mut StepContext<'_>, actions: ActionSet<LifecycleAction>) {
    for action in actions {
        match action {
            LifecycleAction::State(sa) => step.emit_state_action(sa),
        }
    }
}

fn apply_before_inference_actions(
    step: &mut StepContext<'_>,
    actions: ActionSet<BeforeInferenceAction>,
) {
    for action in actions {
        match action {
            BeforeInferenceAction::AddSystemContext(text) => {
                step.inference.system_context.push(text);
            }
            BeforeInferenceAction::AddSessionContext(text) => {
                step.inference.session_context.push(text);
            }
            BeforeInferenceAction::ExcludeTool(id) => {
                step.inference.tools.retain(|t| t.id != id);
            }
            BeforeInferenceAction::IncludeOnlyTools(ids) => {
                step.inference.tools.retain(|t| ids.contains(&t.id));
            }
            BeforeInferenceAction::AddRequestTransform(transform) => {
                step.inference.request_transforms.push(transform);
            }
            BeforeInferenceAction::Terminate(reason) => {
                step.flow.run_action = Some(RunAction::Terminate(reason));
            }
            BeforeInferenceAction::State(sa) => step.emit_state_action(sa),
        }
    }
}

fn apply_after_inference_actions(
    step: &mut StepContext<'_>,
    actions: ActionSet<AfterInferenceAction>,
) {
    for action in actions {
        match action {
            AfterInferenceAction::Terminate(reason) => {
                step.flow.run_action = Some(RunAction::Terminate(reason));
            }
            AfterInferenceAction::State(sa) => step.emit_state_action(sa),
        }
    }
}

fn apply_before_tool_actions(
    step: &mut StepContext<'_>,
    actions: ActionSet<BeforeToolExecuteAction>,
) {
    for action in actions {
        match action {
            BeforeToolExecuteAction::Block(reason) => {
                if let Some(gate) = step.gate.as_mut() {
                    gate.blocked = true;
                    gate.block_reason = Some(reason);
                    gate.pending = false;
                    gate.suspend_ticket = None;
                }
            }
            BeforeToolExecuteAction::Suspend(ticket) => {
                if let Some(gate) = step.gate.as_mut() {
                    gate.blocked = false;
                    gate.block_reason = None;
                    gate.pending = true;
                    gate.suspend_ticket = Some(ticket);
                }
            }
            BeforeToolExecuteAction::SetToolResult(result) => {
                if let Some(gate) = step.gate.as_mut() {
                    gate.result = Some(result);
                }
            }
            BeforeToolExecuteAction::State(sa) => step.emit_state_action(sa),
        }
    }
}

fn apply_after_tool_actions(
    step: &mut StepContext<'_>,
    actions: ActionSet<AfterToolExecuteAction>,
) {
    for action in actions {
        match action {
            AfterToolExecuteAction::AddSystemReminder(text) => {
                step.messaging.reminders.push(text);
            }
            AfterToolExecuteAction::AddUserMessage(text) => {
                step.messaging.user_messages.push(text);
            }
            AfterToolExecuteAction::State(sa) => step.emit_state_action(sa),
        }
    }
}

// =========================================================================
// State reduction helper
// =========================================================================

async fn reduce_and_emit(
    step: &mut StepContext<'_>,
    phase: Phase,
    doc: &DocCell,
) -> Result<(), AgentLoopError> {
    let state_actions = std::mem::take(&mut step.pending_state_actions);
    if state_actions.is_empty() {
        return Ok(());
    }
    // Capture serialized forms before reduce consumes the actions.
    for action in &state_actions {
        if let Some(sa) = action.to_serialized_action() {
            step.emit_serialized_action(sa);
        }
    }
    let scope_ctx = match phase {
        Phase::BeforeToolExecute | Phase::AfterToolExecute => step
            .tool_call_id()
            .map(ScopeContext::for_call)
            .unwrap_or_else(ScopeContext::run),
        _ => ScopeContext::run(),
    };
    let patches = reduce_state_actions(state_actions, &doc.snapshot(), "agent:phase", &scope_ctx)
        .map_err(|e| {
        AgentLoopError::StateError(format!("failed to reduce pending state actions: {e}"))
    })?;
    for p in patches {
        step.emit_patch(p);
    }
    Ok(())
}

// =========================================================================
// Phase dispatch
// =========================================================================

/// Emit a single phase using the [`AgentBehavior`] declarative model.
pub(super) async fn emit_agent_phase(
    phase: Phase,
    step: &mut StepContext<'_>,
    agent: &dyn AgentBehavior,
    doc: &DocCell,
) -> Result<(), AgentLoopError> {
    let ctx = build_read_only_context_from_step(phase, step, doc);
    match phase {
        Phase::RunStart => {
            let actions = agent.run_start(&ctx).await;
            apply_lifecycle_actions(step, actions);
        }
        Phase::StepStart => {
            let actions = agent.step_start(&ctx).await;
            apply_lifecycle_actions(step, actions);
        }
        Phase::BeforeInference => {
            let actions = agent.before_inference(&ctx).await;
            apply_before_inference_actions(step, actions);
        }
        Phase::AfterInference => {
            let actions = agent.after_inference(&ctx).await;
            apply_after_inference_actions(step, actions);
        }
        Phase::BeforeToolExecute => {
            let actions = agent.before_tool_execute(&ctx).await;
            apply_before_tool_actions(step, actions);
        }
        Phase::AfterToolExecute => {
            let actions = agent.after_tool_execute(&ctx).await;
            apply_after_tool_actions(step, actions);
        }
        Phase::StepEnd => {
            let actions = agent.step_end(&ctx).await;
            apply_lifecycle_actions(step, actions);
        }
        Phase::RunEnd => {
            let actions = agent.run_end(&ctx).await;
            apply_lifecycle_actions(step, actions);
        }
    }
    reduce_and_emit(step, phase, doc).await
}

// =========================================================================
// Shared helpers
// =========================================================================

fn take_step_pending_patches(step: &mut StepContext<'_>) -> Vec<TrackedPatch> {
    std::mem::take(&mut step.pending_patches)
}

fn take_step_pending_serialized_actions(
    step: &mut StepContext<'_>,
) -> Vec<tirea_contract::SerializedAction> {
    step.take_pending_serialized_actions()
}

/// Multi-phase block dispatch.
pub(super) async fn run_phase_block<R, Setup, Extract>(
    run_ctx: &RunContext,
    tool_descriptors: &[ToolDescriptor],
    agent: &dyn super::Agent,
    phases: &[Phase],
    setup: Setup,
    extract: Extract,
) -> Result<(R, Vec<TrackedPatch>, Vec<tirea_contract::SerializedAction>), AgentLoopError>
where
    Setup: FnOnce(&mut StepContext<'_>),
    Extract: FnOnce(&mut StepContext<'_>) -> R,
{
    let current_state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let doc = DocCell::new(current_state);
    let ops = Mutex::new(Vec::new());
    let pending_messages = Mutex::new(Vec::new());
    let tool_call_ctx = ToolCallContext::new(
        &doc,
        &ops,
        "phase",
        "agent:phase",
        &run_ctx.run_config,
        &pending_messages,
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
    );
    let mut step = StepContext::new(
        tool_call_ctx,
        run_ctx.thread_id(),
        run_ctx.messages(),
        tool_descriptors.to_vec(),
    );
    setup(&mut step);
    for phase in phases {
        emit_agent_phase(*phase, &mut step, agent.behavior(), &doc).await?;
    }
    let ctx_patch = step.ctx().take_patch();
    if !ctx_patch.patch().is_empty() {
        step.emit_patch(ctx_patch);
    }
    let output = extract(&mut step);
    let pending = take_step_pending_patches(&mut step);
    let actions = take_step_pending_serialized_actions(&mut step);
    Ok((output, pending, actions))
}

/// Single-phase block dispatch (no extract value).
pub(super) async fn emit_phase_block<Setup>(
    phase: Phase,
    run_ctx: &RunContext,
    tool_descriptors: &[ToolDescriptor],
    agent: &dyn super::Agent,
    setup: Setup,
) -> Result<(Vec<TrackedPatch>, Vec<tirea_contract::SerializedAction>), AgentLoopError>
where
    Setup: FnOnce(&mut StepContext<'_>),
{
    let (_, pending, actions) =
        run_phase_block(run_ctx, tool_descriptors, agent, &[phase], setup, |_| ()).await?;
    Ok((pending, actions))
}

/// Cleanup dispatch (after LLM error).
pub(super) async fn emit_cleanup_phases(
    run_ctx: &mut RunContext,
    tool_descriptors: &[ToolDescriptor],
    agent: &dyn super::Agent,
    error_type: &'static str,
    message: String,
) -> Result<(), AgentLoopError> {
    let error = InferenceError {
        error_type: error_type.to_string(),
        message,
    };

    let (pending, actions) = emit_phase_block(
        Phase::AfterInference,
        run_ctx,
        tool_descriptors,
        agent,
        |step| {
            step.llm_response = Some(LLMResponse::error(error));
        },
    )
    .await?;
    run_ctx.add_thread_patches(pending);
    run_ctx.add_serialized_actions(actions);

    let (pending, actions) =
        emit_phase_block(Phase::StepEnd, run_ctx, tool_descriptors, agent, |_| {}).await?;
    run_ctx.add_thread_patches(pending);
    run_ctx.add_serialized_actions(actions);
    Ok(())
}

/// Run-end phase dispatch.
pub(super) async fn emit_run_end_phase(
    run_ctx: &mut RunContext,
    tool_descriptors: &[ToolDescriptor],
    agent: &dyn super::Agent,
) {
    let (pending, actions) = {
        let current_state = match run_ctx.snapshot() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "RunEnd: failed to rebuild state");
                return;
            }
        };
        let doc = DocCell::new(current_state);
        let ops = Mutex::new(Vec::new());
        let pending_messages = Mutex::new(Vec::new());
        let tool_call_ctx = ToolCallContext::new(
            &doc,
            &ops,
            "phase",
            "agent:run_end",
            &run_ctx.run_config,
            &pending_messages,
            tirea_contract::runtime::activity::NoOpActivityManager::arc(),
        );
        let mut step = StepContext::new(
            tool_call_ctx,
            run_ctx.thread_id(),
            run_ctx.messages(),
            tool_descriptors.to_vec(),
        );
        if let Err(e) = emit_agent_phase(Phase::RunEnd, &mut step, agent.behavior(), &doc).await {
            tracing::warn!(error = %e, "RunEnd phase validation failed");
        }
        let ctx_patch = step.ctx().take_patch();
        if !ctx_patch.patch().is_empty() {
            step.emit_patch(ctx_patch);
        }
        (
            take_step_pending_patches(&mut step),
            take_step_pending_serialized_actions(&mut step),
        )
    };
    run_ctx.add_thread_patches(pending);
    run_ctx.add_serialized_actions(actions);
}

/// Tool-level phase dispatch.
pub(super) async fn emit_tool_phase(
    phase: Phase,
    step: &mut StepContext<'_>,
    agent: Option<&dyn AgentBehavior>,
    doc: &DocCell,
) -> Result<(), AgentLoopError> {
    if let Some(agent) = agent {
        emit_agent_phase(phase, step, agent, doc).await
    } else {
        Ok(())
    }
}

// =========================================================================
// Behavior-only block helper (used by tool_exec.rs public APIs)
// =========================================================================

pub(super) async fn behavior_run_phase_block<R, Setup, Extract>(
    run_ctx: &RunContext,
    tool_descriptors: &[ToolDescriptor],
    behavior: &dyn AgentBehavior,
    phases: &[Phase],
    setup: Setup,
    extract: Extract,
) -> Result<(R, Vec<TrackedPatch>), AgentLoopError>
where
    Setup: FnOnce(&mut StepContext<'_>),
    Extract: FnOnce(&mut StepContext<'_>) -> R,
{
    let current_state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let doc = DocCell::new(current_state);
    let ops = Mutex::new(Vec::new());
    let pending_messages = Mutex::new(Vec::new());
    let tool_call_ctx = ToolCallContext::new(
        &doc,
        &ops,
        "phase",
        "behavior:phase",
        &run_ctx.run_config,
        &pending_messages,
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
    );
    let mut step = StepContext::new(
        tool_call_ctx,
        run_ctx.thread_id(),
        run_ctx.messages(),
        tool_descriptors.to_vec(),
    );
    setup(&mut step);
    for phase in phases {
        emit_agent_phase(*phase, &mut step, behavior, &doc).await?;
    }
    let ctx_patch = step.ctx().take_patch();
    if !ctx_patch.patch().is_empty() {
        step.emit_patch(ctx_patch);
    }
    let output = extract(&mut step);
    let pending = take_step_pending_patches(&mut step);
    // Note: behavior_run_phase_block is only used in tool_exec.rs public APIs
    // where RunContext is not available. Serialized actions are dropped here.
    let _actions = take_step_pending_serialized_actions(&mut step);
    Ok((output, pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::behavior::{NoOpBehavior, ReadOnlyContext};
    use crate::contracts::testing::TestFixture;
    use async_trait::async_trait;
    use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};
    use tirea_state::DocCell;

    struct TestActionBehavior;

    #[async_trait]
    impl AgentBehavior for TestActionBehavior {
        fn id(&self) -> &str {
            "test_action"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::AddSystemContext(
                "injected by action".into(),
            ))
        }
    }

    #[tokio::test]
    async fn emit_agent_phase_validates_and_applies_actions() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(serde_json::json!({}));

        emit_agent_phase(Phase::BeforeInference, &mut step, &TestActionBehavior, &doc)
            .await
            .expect("actions should be applied");

        assert_eq!(step.inference.system_context, vec!["injected by action"]);
    }

    #[tokio::test]
    async fn emit_agent_phase_noop_behavior_succeeds() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(serde_json::json!({}));

        emit_agent_phase(Phase::RunStart, &mut step, &NoOpBehavior, &doc)
            .await
            .expect("noop behavior should succeed");
    }
}
