use super::core::{clear_agent_inference_error, set_agent_inference_error};
use super::effect_applicator::apply_phase_output;
use super::AgentLoopError;
use crate::contracts::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::plugin::phase::{Phase, StateEffect, StepContext};
use crate::contracts::runtime::tool_call::ToolDescriptor;
use crate::contracts::RunContext;
use crate::contracts::ToolCallContext;
use crate::runtime::control::InferenceError;
use std::sync::Mutex;
use tirea_state::{DocCell, TrackedPatch};




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
                // Safety: leak to 'a. The resume data lives in the DocCell which
                // outlives this phase dispatch. We box-leak here because the
                // ToolCallResume is deserialized on-the-fly (not borrowed from doc).
                let resume = Box::leak(Box::new(resume));
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
    let ctx = build_read_only_context(phase, step, doc);
    let output = dispatch_agent_phase(agent, phase, &ctx).await;
    apply_phase_output(phase, step, output, doc)
}

// =========================================================================
// Shared helpers
// =========================================================================

fn take_step_pending_patches(step: &mut StepContext<'_>) -> Vec<TrackedPatch> {
    let mut pending = std::mem::take(&mut step.pending_patches);
    for effect in std::mem::take(&mut step.state_effects) {
        match effect {
            StateEffect::Patch(patch) => pending.push(patch),
        }
    }
    pending
}

// =========================================================================
// Agent-driven dispatch — dispatches through Agent (primary path)
// =========================================================================

/// Dispatch a single phase through the agent's behavior.
pub(super) async fn emit_phase(
    phase: Phase,
    step: &mut StepContext<'_>,
    agent: &dyn super::Agent,
    doc: &DocCell,
) -> Result<(), AgentLoopError> {
    emit_agent_phase(phase, step, agent.behavior(), doc).await
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
        emit_phase(*phase, &mut step, agent, &doc).await?;
    }
    let ctx_patch = step.ctx().take_patch();
    if !ctx_patch.patch().is_empty() {
        step.emit_patch(ctx_patch);
    }
    let output = extract(&mut step);
    let pending = take_step_pending_patches(&mut step);
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
        if let Err(e) = emit_phase(Phase::RunEnd, &mut step, agent, &doc).await {
            tracing::warn!(error = %e, "RunEnd phase validation failed");
        }
        let ctx_patch = step.ctx().take_patch();
        if !ctx_patch.patch().is_empty() {
            step.emit_patch(ctx_patch);
        }
        take_step_pending_patches(&mut step)
    };
    run_ctx.add_thread_patches(pending);
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
    Ok((output, pending))
}
