use super::core::{clear_agent_inference_error, set_agent_inference_error};
use super::effect_applicator::apply_phase_output;
use super::AgentLoopError;
use crate::contracts::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::plugin::phase::{
    AfterInferenceContext, AfterToolExecuteContext, BeforeInferenceContext,
    BeforeToolExecuteContext, Phase, RunAction, RunEndContext, RunStartContext,
    StateEffect, StepContext, StepEndContext, StepStartContext,
};
use crate::contracts::runtime::plugin::AgentPlugin;
use crate::contracts::runtime::tool_call::ToolDescriptor;
use crate::contracts::RunContext;
use crate::contracts::ToolCallContext;
use crate::runtime::control::InferenceError;
use std::sync::{Arc, Mutex};
use tirea_state::{DocCell, TrackedPatch};

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhaseMutationSnapshot {
    run_action: Option<RunAction>,
    system_context: Vec<String>,
    session_context: Vec<String>,
    system_reminders: Vec<String>,
    tool_ids: Vec<String>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    tool_blocked: bool,
    tool_pending: bool,
    tool_suspended_target_id: Option<String>,
    tool_has_result: bool,
}

fn phase_mutation_snapshot(step: &StepContext<'_>) -> PhaseMutationSnapshot {
    PhaseMutationSnapshot {
        run_action: step.run_action.clone(),
        system_context: step.system_context.clone(),
        session_context: step.session_context.clone(),
        system_reminders: step.system_reminders.clone(),
        tool_ids: step.tools.iter().map(|t| t.id.clone()).collect(),
        tool_call_id: step.tool.as_ref().map(|t| t.id.clone()),
        tool_name: step.tool.as_ref().map(|t| t.name.clone()),
        tool_blocked: step.tool_blocked(),
        tool_pending: step.tool_pending(),
        tool_suspended_target_id: step.tool.as_ref().and_then(|t| {
            t.suspend_ticket
                .as_ref()
                .map(|ticket| ticket.suspension.id.clone())
        }),
        tool_has_result: step.tool.as_ref().and_then(|t| t.result.as_ref()).is_some(),
    }
}

fn validate_phase_mutation(
    phase: Phase,
    plugin_id: &str,
    before: &PhaseMutationSnapshot,
    after: &PhaseMutationSnapshot,
) -> Result<(), AgentLoopError> {
    fn is_append_only(before: &[String], after: &[String]) -> bool {
        after.len() >= before.len() && after.starts_with(before)
    }

    let policy = phase.policy();

    if before.tool_ids != after.tool_ids && !policy.allow_tool_filter_mutation {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' mutated tool filtering outside BeforeInference ({phase})",
            plugin_id
        )));
    }

    if before.run_action != after.run_action && !policy.allow_run_action_mutation {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' mutated run_action outside BeforeInference/AfterInference ({phase})",
            plugin_id
        )));
    }

    let prompt_context_changed = before.system_context != after.system_context
        || before.session_context != after.session_context;
    if prompt_context_changed && phase != Phase::BeforeInference {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' mutated prompt context outside BeforeInference ({phase})",
            plugin_id
        )));
    }
    if phase == Phase::BeforeInference
        && (!is_append_only(&before.system_context, &after.system_context)
            || !is_append_only(&before.session_context, &after.session_context))
    {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' performed non-append prompt context mutation in BeforeInference ({phase})",
            plugin_id
        )));
    }

    if before.system_reminders != after.system_reminders && phase != Phase::AfterToolExecute {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' mutated system reminders outside AfterToolExecute ({phase})",
            plugin_id
        )));
    }
    if phase == Phase::AfterToolExecute
        && !is_append_only(&before.system_reminders, &after.system_reminders)
    {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' performed non-append reminder mutation in AfterToolExecute ({phase})",
            plugin_id
        )));
    }

    if before.tool_call_id != after.tool_call_id || before.tool_name != after.tool_name {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' mutated tool identity in phase {phase}",
            plugin_id
        )));
    }

    let tool_gate_changed = before.tool_blocked != after.tool_blocked
        || before.tool_pending != after.tool_pending
        || before.tool_suspended_target_id != after.tool_suspended_target_id;
    if tool_gate_changed && !policy.allow_tool_gate_mutation {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' mutated tool gate outside BeforeToolExecute ({phase})",
            plugin_id
        )));
    }
    if after.tool_blocked && after.tool_pending {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' produced invalid tool gate state: blocked and pending are mutually exclusive ({phase})",
            plugin_id
        )));
    }
    if after.tool_pending && after.tool_suspended_target_id.is_none() {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' produced invalid tool gate state: pending without suspend ticket ({phase})",
            plugin_id
        )));
    }

    if before.tool_has_result != after.tool_has_result {
        return Err(AgentLoopError::StateError(format!(
            "plugin '{}' mutated tool result in phase {phase}",
            plugin_id
        )));
    }

    Ok(())
}

// =========================================================================
// Plugin-based dispatch (legacy path)
// =========================================================================

pub(super) async fn emit_phase_checked(
    phase: Phase,
    step: &mut StepContext<'_>,
    plugins: &[Arc<dyn AgentPlugin>],
) -> Result<(), AgentLoopError> {
    async fn dispatch_phase(
        plugin: &Arc<dyn AgentPlugin>,
        phase: Phase,
        step: &mut StepContext<'_>,
    ) {
        match phase {
            Phase::RunStart => {
                let mut ctx = RunStartContext::new(step);
                plugin.run_start(&mut ctx).await;
            }
            Phase::StepStart => {
                let mut ctx = StepStartContext::new(step);
                plugin.step_start(&mut ctx).await;
            }
            Phase::BeforeInference => {
                let mut ctx = BeforeInferenceContext::new(step);
                plugin.before_inference(&mut ctx).await;
            }
            Phase::AfterInference => {
                let mut ctx = AfterInferenceContext::new(step);
                plugin.after_inference(&mut ctx).await;
            }
            Phase::BeforeToolExecute => {
                let mut ctx = BeforeToolExecuteContext::new(step);
                plugin.before_tool_execute(&mut ctx).await;
            }
            Phase::AfterToolExecute => {
                let mut ctx = AfterToolExecuteContext::new(step);
                plugin.after_tool_execute(&mut ctx).await;
            }
            Phase::StepEnd => {
                let mut ctx = StepEndContext::new(step);
                plugin.step_end(&mut ctx).await;
            }
            Phase::RunEnd => {
                let mut ctx = RunEndContext::new(step);
                plugin.run_end(&mut ctx).await;
            }
        }
    }

    for plugin in plugins {
        let before = phase_mutation_snapshot(step);
        dispatch_phase(plugin, phase, step).await;
        let after = phase_mutation_snapshot(step);
        validate_phase_mutation(phase, plugin.id(), &before, &after)?;
    }
    Ok(())
}

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

/// Dispatch a single phase through the config's agent.
///
/// Primary path: declarative Agent model. When the agent is the default
/// [`NoOpBehavior`] and legacy plugins are present, falls back to mutable
/// plugin dispatch for backward compatibility.
pub(super) async fn unified_emit_phase(
    phase: Phase,
    step: &mut StepContext<'_>,
    agent: &dyn super::Agent,
    doc: &DocCell,
) -> Result<(), AgentLoopError> {
    if agent.behavior().id() != "noop" || agent.plugins().is_empty() {
        emit_agent_phase(phase, step, agent.behavior(), doc).await
    } else {
        emit_phase_checked(phase, step, agent.plugins()).await
    }
}

/// Unified multi-phase block dispatch.
pub(super) async fn unified_run_phase_block<R, Setup, Extract>(
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
        unified_emit_phase(*phase, &mut step, agent, &doc).await?;
    }
    let ctx_patch = step.ctx().take_patch();
    if !ctx_patch.patch().is_empty() {
        step.emit_patch(ctx_patch);
    }
    let output = extract(&mut step);
    let pending = take_step_pending_patches(&mut step);
    Ok((output, pending))
}

/// Unified single-phase block dispatch (no extract value).
pub(super) async fn unified_emit_phase_block<Setup>(
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
        unified_run_phase_block(run_ctx, tool_descriptors, agent, &[phase], setup, |_| ()).await?;
    Ok(pending)
}

/// Unified cleanup dispatch (after LLM error).
pub(super) async fn unified_emit_cleanup_phases_and_apply(
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

    let pending = unified_emit_phase_block(
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
        unified_emit_phase_block(Phase::StepEnd, run_ctx, tool_descriptors, agent, |_| {}).await?;
    run_ctx.add_thread_patches(pending);
    Ok(())
}

/// Unified run-end phase dispatch.
pub(super) async fn unified_emit_run_end_phase(
    run_ctx: &mut RunContext,
    tool_descriptors: &[ToolDescriptor],
    agent: &dyn super::Agent,
) {
    let pending = {
        let current_state = match run_ctx.snapshot() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "UnifiedRunEnd: failed to rebuild state");
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
        if let Err(e) = unified_emit_phase(Phase::RunEnd, &mut step, agent, &doc).await {
            tracing::warn!(error = %e, "UnifiedRunEnd phase validation failed");
        }
        let ctx_patch = step.ctx().take_patch();
        if !ctx_patch.patch().is_empty() {
            step.emit_patch(ctx_patch);
        }
        take_step_pending_patches(&mut step)
    };
    run_ctx.add_thread_patches(pending);
}

/// Unified tool-level phase dispatch (for use in ToolPhaseContext).
///
/// Primary path: declarative Agent model. When the agent is the default
/// [`NoOpBehavior`] and legacy plugins are present, falls back to mutable
/// plugin dispatch for backward compatibility.
pub(super) async fn unified_emit_tool_phase(
    phase: Phase,
    step: &mut StepContext<'_>,
    agent: Option<&dyn AgentBehavior>,
    plugins: &[Arc<dyn AgentPlugin>],
    doc: &DocCell,
) -> Result<(), AgentLoopError> {
    match agent {
        Some(agent) if agent.id() != "noop" || plugins.is_empty() => {
            emit_agent_phase(phase, step, agent, doc).await
        }
        _ => emit_phase_checked(phase, step, plugins).await,
    }
}

// =========================================================================
// Legacy plugin-only block helpers (used by tool_exec.rs public APIs)
// =========================================================================

pub(super) async fn run_phase_block<R, Setup, Extract>(
    run_ctx: &RunContext,
    tool_descriptors: &[ToolDescriptor],
    plugins: &[Arc<dyn AgentPlugin>],
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
        "plugin:phase",
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
        emit_phase_checked(*phase, &mut step, plugins).await?;
    }
    let plugin_patch = step.ctx().take_patch();
    if !plugin_patch.patch().is_empty() {
        step.emit_patch(plugin_patch);
    }
    let output = extract(&mut step);
    let pending = take_step_pending_patches(&mut step);
    Ok((output, pending))
}

#[cfg(test)]
pub(super) async fn emit_phase_block<Setup>(
    phase: Phase,
    run_ctx: &RunContext,
    tool_descriptors: &[ToolDescriptor],
    plugins: &[Arc<dyn AgentPlugin>],
    setup: Setup,
) -> Result<Vec<TrackedPatch>, AgentLoopError>
where
    Setup: FnOnce(&mut StepContext<'_>),
{
    let (_, pending) =
        run_phase_block(run_ctx, tool_descriptors, plugins, &[phase], setup, |_| ()).await?;
    Ok(pending)
}

#[cfg(test)]
pub(super) async fn emit_cleanup_phases_and_apply(
    run_ctx: &mut RunContext,
    tool_descriptors: &[ToolDescriptor],
    plugins: &[Arc<dyn AgentPlugin>],
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
        plugins,
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
        emit_phase_block(Phase::StepEnd, run_ctx, tool_descriptors, plugins, |_| {}).await?;
    run_ctx.add_thread_patches(pending);
    Ok(())
}

#[cfg(test)]
pub(super) async fn emit_run_end_phase(
    run_ctx: &mut RunContext,
    tool_descriptors: &[ToolDescriptor],
    plugins: &[Arc<dyn AgentPlugin>],
) {
    let pending = {
        let current_state = match run_ctx.snapshot() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "RunEndPhase(RunEnd): failed to rebuild state");
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
            "plugin:run_end",
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
        if let Err(e) = emit_phase_checked(Phase::RunEnd, &mut step, plugins).await {
            tracing::warn!(error = %e, "RunEndPhase(RunEnd) plugin phase validation failed");
        }
        let plugin_patch = step.ctx().take_patch();
        if !plugin_patch.patch().is_empty() {
            step.emit_patch(plugin_patch);
        }
        take_step_pending_patches(&mut step)
    };
    run_ctx.add_thread_patches(pending);
}
