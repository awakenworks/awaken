use super::core::{clear_agent_inference_error, set_agent_inference_error};
use super::effect_applicator::{apply_phase_output, apply_phase_output_with_options};
use super::AgentLoopError;
use crate::contracts::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::plugin::phase::effect::PhaseOutput;
use crate::contracts::runtime::plugin::phase::{
    reduce_state_actions, AnyPluginAction, AnyStateAction, CommutativeAction, Phase, StateScope,
    StepContext,
};
use crate::contracts::runtime::tool_call::ToolDescriptor;
use crate::contracts::RunContext;
use crate::contracts::ToolCallContext;
use crate::runtime::control::InferenceError;
use serde_json::Value;
use std::sync::Mutex;
use tirea_state::{DocCell, Patch, PatchExt, TrackedPatch};

// =========================================================================
// Agent-based dispatch (declarative model: ReadOnlyContext → PhaseOutput)
// =========================================================================

/// Build a [`ReadOnlyContext`] from the current step and doc state.
fn build_read_only_context<'a>(
    phase: Phase,
    step: &'a StepContext<'a>,
    doc: &'a DocCell,
) -> ReadOnlyContext<'a> {
    let mut ctx = ReadOnlyContext::new(
        phase,
        step.thread_id(),
        step.messages(),
        step.run_config(),
        doc,
    );
    if let Some(response) = step.response.as_ref() {
        ctx = ctx.with_response(response);
    }
    if let Some(tool) = step.tool.as_ref() {
        ctx = ctx.with_tool_info(&tool.name, &tool.id, Some(&tool.args));
        if let Some(result) = tool.result.as_ref() {
            ctx = ctx.with_tool_result(result);
        }
    }
    // Populate resume_input for BeforeToolExecute.
    if phase == Phase::BeforeToolExecute {
        if let Some(call_id) = step.tool_call_id() {
            if let Ok(Some(resume)) = step.ctx().resume_input_for(call_id) {
                ctx = ctx.with_resume_input(resume);
            }
        }
    }
    ctx
}

/// Dispatch a single phase hook on an [`Agent`].
async fn dispatch_agent_phase<'a>(
    agent: &dyn AgentBehavior,
    phase: Phase,
    ctx: &ReadOnlyContext<'a>,
) -> crate::contracts::runtime::plugin::phase::effect::PhaseOutput {
    match phase {
        Phase::RunStart => agent.run_start(ctx).await,
        Phase::StepStart => agent.step_start(ctx).await,
        Phase::BeforeInference => agent.before_inference(ctx).await,
        Phase::AfterInference => agent.after_inference(ctx).await,
        Phase::BeforeToolExecute => agent.before_tool_execute(ctx).await,
        Phase::AfterToolExecute => agent.after_tool_execute(ctx).await,
        Phase::StepEnd => agent.step_end(ctx).await,
        Phase::RunEnd => agent.run_end(ctx).await,
    }
}

fn validate_owned_state_actions(
    agent: &dyn AgentBehavior,
    phase: Phase,
    step: &StepContext<'_>,
    output: &PhaseOutput,
) -> Result<(), AgentLoopError> {
    if output.state_actions.is_empty() {
        return Ok(());
    }
    let owned_states = agent.owned_states();
    for action in &output.state_actions {
        let Some(state_type_id) = action.state_type_id() else {
            continue;
        };
        if !owned_states.contains(&state_type_id) {
            return Err(AgentLoopError::StateError(format!(
                "behavior '{}' emitted action for unowned state '{}' (declare it in owned_states)",
                agent.id(),
                action.state_type_name()
            )));
        }
        if action.scope() == StateScope::ToolCall {
            if phase != Phase::BeforeToolExecute && phase != Phase::AfterToolExecute {
                return Err(AgentLoopError::StateError(format!(
                    "behavior '{}' emitted ToolCall-scoped action '{}' in invalid phase {:?}",
                    agent.id(),
                    action.state_type_name(),
                    phase
                )));
            }
            if step.tool_call_id().is_none() {
                return Err(AgentLoopError::StateError(format!(
                    "behavior '{}' emitted ToolCall-scoped action '{}' without active tool context",
                    agent.id(),
                    action.state_type_name()
                )));
            }
        }
    }
    Ok(())
}

/// Emit a single phase using the [`Agent`] declarative model.
///
/// Builds a [`ReadOnlyContext`], calls the agent hook, and applies the
/// returned [`PhaseOutput`] to the mutable `StepContext`.
pub(super) async fn emit_agent_phase(
    phase: Phase,
    step: &mut StepContext<'_>,
    agent: &dyn AgentBehavior,
    doc: &DocCell,
) -> Result<(), AgentLoopError> {
    emit_agent_phase_with_options(phase, step, agent, doc, false).await
}

async fn emit_agent_phase_with_options(
    phase: Phase,
    step: &mut StepContext<'_>,
    agent: &dyn AgentBehavior,
    doc: &DocCell,
    defer_commutative_state_actions: bool,
) -> Result<(), AgentLoopError> {
    let ctx = build_read_only_context(phase, step, doc);
    let output = dispatch_agent_phase(agent, phase, &ctx).await;
    let plugin_actions = agent.phase_actions(phase, &ctx).await;
    validate_owned_state_actions(agent, phase, step, &output)?;
    if defer_commutative_state_actions {
        apply_phase_output_with_options(phase, step, output, doc, true)?
    } else {
        apply_phase_output(phase, step, output, doc)?
    }
    let plugin_patches = reduce_behavior_plugin_actions(agent, &doc.snapshot(), plugin_actions)?;
    for patch in plugin_patches {
        step.emit_patch(patch);
    }
    Ok(())
}

// =========================================================================
// Shared helpers
// =========================================================================

fn take_step_pending_patches(step: &mut StepContext<'_>) -> Vec<TrackedPatch> {
    std::mem::take(&mut step.pending_patches)
}

fn take_step_pending_commutative_actions(step: &mut StepContext<'_>) -> Vec<CommutativeAction> {
    std::mem::take(&mut step.pending_commutative_actions)
}

fn merge_tracked_patches(patches: &[TrackedPatch], source: &str) -> Option<TrackedPatch> {
    let mut merged = Patch::new();
    for tracked in patches {
        merged.extend(tracked.patch().clone());
    }
    if merged.is_empty() {
        None
    } else {
        Some(TrackedPatch::new(merged).with_source(source.to_string()))
    }
}

fn reduce_commutative_actions_to_patch(
    base_snapshot: &Value,
    actions: Vec<CommutativeAction>,
    source: &str,
) -> Result<Option<TrackedPatch>, AgentLoopError> {
    if actions.is_empty() {
        return Ok(None);
    }

    let tracked = reduce_state_actions(
        actions
            .into_iter()
            .map(AnyStateAction::Commutative)
            .collect(),
        base_snapshot,
        source,
    )
    .map_err(|e| {
        AgentLoopError::StateError(format!("failed to reduce commutative state actions: {e}"))
    })?;
    Ok(merge_tracked_patches(&tracked, source))
}

pub(super) fn reduce_behavior_plugin_actions(
    behavior: &dyn AgentBehavior,
    base_snapshot: &Value,
    actions: Vec<AnyPluginAction>,
) -> Result<Vec<TrackedPatch>, AgentLoopError> {
    if actions.is_empty() {
        return Ok(Vec::new());
    }

    behavior
        .reduce_plugin_actions(actions, base_snapshot)
        .map_err(AgentLoopError::StateError)
}

fn finalize_step_pending_outputs(
    step: &mut StepContext<'_>,
    base_snapshot: &Value,
    defer_commutative_state_actions: bool,
) -> Result<Vec<TrackedPatch>, AgentLoopError> {
    let mut pending = take_step_pending_patches(step);
    let commutative_actions = take_step_pending_commutative_actions(step);

    if !defer_commutative_state_actions || commutative_actions.is_empty() {
        return Ok(pending);
    }

    if let Some(commutative_patch) =
        reduce_commutative_actions_to_patch(base_snapshot, commutative_actions, "agent_loop")?
    {
        for patch in &pending {
            let conflicts = patch.patch().conflicts_with(commutative_patch.patch());
            if let Some(conflict) = conflicts.first() {
                return Err(AgentLoopError::StateError(format!(
                    "conflicting phase state patches between '{}' and 'commutative_actions' at {}",
                    patch.source.as_deref().unwrap_or("agent"),
                    conflict.path
                )));
            }
        }
        pending.push(commutative_patch);
    }

    Ok(pending)
}

/// Multi-phase block dispatch.
pub(super) async fn run_phase_block<R, Setup, Extract>(
    run_ctx: &RunContext,
    tool_descriptors: &[ToolDescriptor],
    agent: &dyn super::Agent,
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
        emit_agent_phase_with_options(*phase, &mut step, agent.behavior(), &doc, true).await?;
    }
    let ctx_patch = step.ctx().take_patch();
    if !ctx_patch.patch().is_empty() {
        step.emit_patch(ctx_patch);
    }
    let output = extract(&mut step);
    let pending = finalize_step_pending_outputs(&mut step, &doc.snapshot(), true)?;
    Ok((output, pending))
}

/// Single-phase block dispatch (no extract value).
pub(super) async fn emit_phase_block<Setup>(
    phase: Phase,
    run_ctx: &RunContext,
    tool_descriptors: &[ToolDescriptor],
    agent: &dyn super::Agent,
    setup: Setup,
) -> Result<Vec<TrackedPatch>, AgentLoopError>
where
    Setup: FnOnce(&mut StepContext<'_>),
{
    let (_, pending) =
        run_phase_block(run_ctx, tool_descriptors, agent, &[phase], setup, |_| ()).await?;
    Ok(pending)
}

/// Cleanup dispatch (after LLM error).
pub(super) async fn emit_cleanup_phases(
    run_ctx: &mut RunContext,
    tool_descriptors: &[ToolDescriptor],
    agent: &dyn super::Agent,
    error_type: &'static str,
    message: String,
) -> Result<(), AgentLoopError> {
    let state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let set_error_patch = set_agent_inference_error(
        &state,
        InferenceError {
            error_type: error_type.to_string(),
            message,
        },
    )?;
    run_ctx.add_thread_patch(set_error_patch);

    let pending = emit_phase_block(
        Phase::AfterInference,
        run_ctx,
        tool_descriptors,
        agent,
        |_| {},
    )
    .await?;
    run_ctx.add_thread_patches(pending);

    let state = run_ctx
        .snapshot()
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    let clear_error_patch = clear_agent_inference_error(&state)?;
    run_ctx.add_thread_patch(clear_error_patch);

    let pending =
        emit_phase_block(Phase::StepEnd, run_ctx, tool_descriptors, agent, |_| {}).await?;
    run_ctx.add_thread_patches(pending);
    Ok(())
}

/// Run-end phase dispatch.
pub(super) async fn emit_run_end_phase(
    run_ctx: &mut RunContext,
    tool_descriptors: &[ToolDescriptor],
    agent: &dyn super::Agent,
) {
    let pending = {
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
        if let Err(e) =
            emit_agent_phase_with_options(Phase::RunEnd, &mut step, agent.behavior(), &doc, true)
                .await
        {
            tracing::warn!(error = %e, "RunEnd phase validation failed");
        }
        let ctx_patch = step.ctx().take_patch();
        if !ctx_patch.patch().is_empty() {
            step.emit_patch(ctx_patch);
        }
        match finalize_step_pending_outputs(&mut step, &doc.snapshot(), true) {
            Ok(pending) => pending,
            Err(e) => {
                tracing::warn!(error = %e, "RunEnd phase commutative reduce failed");
                Vec::new()
            }
        }
    };
    run_ctx.add_thread_patches(pending);
}

/// Tool-level phase dispatch.
pub(super) async fn emit_tool_phase(
    phase: Phase,
    step: &mut StepContext<'_>,
    agent: Option<&dyn AgentBehavior>,
    doc: &DocCell,
    defer_commutative_state_actions: bool,
) -> Result<(), AgentLoopError> {
    if let Some(agent) = agent {
        if defer_commutative_state_actions {
            emit_agent_phase_with_options(phase, step, agent, doc, true).await
        } else {
            emit_agent_phase(phase, step, agent, doc).await
        }
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
        emit_agent_phase_with_options(*phase, &mut step, behavior, &doc, true).await?;
    }
    let ctx_patch = step.ctx().take_patch();
    if !ctx_patch.patch().is_empty() {
        step.emit_patch(ctx_patch);
    }
    let output = extract(&mut step);
    let pending = finalize_step_pending_outputs(&mut step, &doc.snapshot(), true)?;
    Ok((output, pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::plugin::phase::state_spec::StateSpec;
    use crate::contracts::runtime::plugin::phase::AnyStateAction;
    use crate::contracts::testing::TestFixture;
    use crate::contracts::thread::ToolCall;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::any::TypeId;
    use std::collections::HashSet;
    use tirea_state::{DocCell, PatchSink, Path as TPath, State, TireaResult};

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    struct OwnedDebugState {
        value: bool,
    }

    struct OwnedDebugStateRef;

    impl State for OwnedDebugState {
        type Ref<'a> = OwnedDebugStateRef;
        const PATH: &'static str = "debug.owned";

        fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
            OwnedDebugStateRef
        }

        fn from_value(value: &Value) -> TireaResult<Self> {
            if value.is_null() {
                return Ok(Self::default());
            }
            serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
        }

        fn to_value(&self) -> TireaResult<Value> {
            serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
        }
    }

    impl StateSpec for OwnedDebugState {
        type Action = bool;

        fn reduce(&mut self, action: Self::Action) {
            self.value = action;
        }
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    struct ToolScopedFlagState {
        value: bool,
    }

    struct ToolScopedFlagStateRef;

    impl State for ToolScopedFlagState {
        type Ref<'a> = ToolScopedFlagStateRef;
        const PATH: &'static str = "__tool_call_states.flag";

        fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
            ToolScopedFlagStateRef
        }

        fn from_value(value: &Value) -> TireaResult<Self> {
            if value.is_null() {
                return Ok(Self::default());
            }
            serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
        }

        fn to_value(&self) -> TireaResult<Value> {
            serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
        }
    }

    impl StateSpec for ToolScopedFlagState {
        type Action = bool;
        const SCOPE: StateScope = StateScope::ToolCall;

        fn reduce(&mut self, action: Self::Action) {
            self.value = action;
        }
    }

    struct UnownedActionBehavior;

    #[async_trait]
    impl AgentBehavior for UnownedActionBehavior {
        fn id(&self) -> &str {
            "unowned_action"
        }

        async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
            PhaseOutput::default().with_state_action(AnyStateAction::new::<OwnedDebugState>(true))
        }
    }

    struct OwnedActionBehavior;

    #[async_trait]
    impl AgentBehavior for OwnedActionBehavior {
        fn id(&self) -> &str {
            "owned_action"
        }

        fn owned_states(&self) -> HashSet<TypeId> {
            HashSet::from([TypeId::of::<OwnedDebugState>()])
        }

        async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
            PhaseOutput::default().with_state_action(AnyStateAction::new::<OwnedDebugState>(true))
        }
    }

    struct ToolScopedActionBehavior;

    #[async_trait]
    impl AgentBehavior for ToolScopedActionBehavior {
        fn id(&self) -> &str {
            "tool_scoped_action"
        }

        fn owned_states(&self) -> HashSet<TypeId> {
            HashSet::from([TypeId::of::<ToolScopedFlagState>()])
        }

        async fn run_start(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
            PhaseOutput::default()
                .with_state_action(AnyStateAction::new::<ToolScopedFlagState>(true))
        }

        async fn before_tool_execute(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
            PhaseOutput::default()
                .with_state_action(AnyStateAction::new::<ToolScopedFlagState>(true))
        }
    }

    #[tokio::test]
    async fn emit_agent_phase_rejects_unowned_typed_state_action() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(serde_json::json!({}));

        let err = emit_agent_phase(Phase::RunStart, &mut step, &UnownedActionBehavior, &doc)
            .await
            .expect_err("should reject action for unowned state");

        match err {
            AgentLoopError::StateError(message) => {
                assert!(message.contains("unowned state"));
                assert!(message.contains("OwnedDebugState"));
            }
            other => panic!("expected StateError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_agent_phase_accepts_owned_typed_state_action() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(serde_json::json!({}));

        emit_agent_phase(Phase::RunStart, &mut step, &OwnedActionBehavior, &doc)
            .await
            .expect("owned action should pass validation");

        assert!(!step.pending_patches.is_empty());
    }

    #[tokio::test]
    async fn emit_agent_phase_rejects_tool_scoped_action_without_tool_context() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(serde_json::json!({}));

        let err = emit_agent_phase(Phase::RunStart, &mut step, &ToolScopedActionBehavior, &doc)
            .await
            .expect_err("tool-scoped action should fail outside tool phases");

        match err {
            AgentLoopError::StateError(message) => {
                assert!(message.contains("ToolCall-scoped"));
            }
            other => panic!("expected StateError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn emit_agent_phase_accepts_tool_scoped_action_with_tool_context() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let call = ToolCall::new("call_1", "echo", serde_json::json!({"ok": true}));
        step.tool = Some(crate::contracts::runtime::plugin::phase::ToolContext::new(
            &call,
        ));
        let doc = DocCell::new(serde_json::json!({}));

        emit_agent_phase(
            Phase::BeforeToolExecute,
            &mut step,
            &ToolScopedActionBehavior,
            &doc,
        )
        .await
        .expect("tool-scoped action should pass with active tool context");

        assert!(!step.pending_patches.is_empty());
    }
}
