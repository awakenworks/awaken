// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
//! One context construction sequence under the caller's Session lifecycle guard.
//! Existing-obligation evidence; this is not a whole-constructor refinement proof.
//!
//! Planning, physical acquisition, Runtime assembly and cache installation have
//! owned outputs. Only installation publishes a complete SessionCtx.

mod assembly;
mod execution;
mod extensions;
mod plan;
mod resources;

use crate::HostError;
use crate::host::{SessionCtx, session_ctx::RuntimeContextPurpose};
use std::sync::Arc;

type SemanticSkillTools = (
    Vec<awaken_runtime_contract::resolved::ToolDescriptor>,
    Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
);

struct PreparedContext {
    ctx: Arc<SessionCtx>,
    active_mcp: Vec<awaken_session_mcp::McpGenerationProjection>,
}

pub(super) async fn construct(
    host: &crate::RunAssembly,
    lifecycle: crate::session_realizations::LifecycleHeld<'_>,
    thread: &str,
    agent: Option<&str>,
    request: super::SessionOpenRequest,
    requested_purpose: RuntimeContextPurpose,
) -> Result<Arc<SessionCtx>, HostError> {
    let super::SessionOpenRequest {
        published_snapshot,
        claimed_attempt,
        force_defer_environment,
        recover_revoked_legacy,
    } = request;
    let plan = plan::prepare(
        host,
        thread,
        agent,
        published_snapshot,
        claimed_attempt.as_ref(),
        force_defer_environment,
    )
    .await?;
    let resources = resources::acquire(
        host,
        lifecycle,
        thread,
        plan,
        requested_purpose,
        force_defer_environment,
        recover_revoked_legacy,
    )
    .await?;
    // Assembly's typed extension futures are larger than this stage's output.
    // Keep their storage out of the enclosing construction future: nested polls
    // otherwise exhaust the default stack before Memory recovery can start.
    // The same future runs under the caller's lifecycle guard; only install
    // below publishes the completed context.
    let assembly = assembly::assemble(host, thread, resources, requested_purpose);
    let prepared = Box::pin(assembly).await?;
    install(host, thread, prepared).await
}

async fn install(
    host: &crate::RunAssembly,
    thread: &str,
    prepared: PreparedContext,
) -> Result<Arc<SessionCtx>, HostError> {
    let PreparedContext { ctx, active_mcp } = prepared;
    host.install_mcp_input_handlers(
        &active_mcp,
        ctx.config.resolved_spec.plugin_config.plugins(),
    )?;
    host.installed_sessions
        .install_with(thread, |slot| slot.run.context = Some(ctx.clone()))?;
    // A prior process may have crashed after atomically ending the parent
    // and before delivering its remote child cancellations. Re-entering the
    // session redelivers those idempotent outbox entries.
    let _ = ctx
        .runtime
        .reconcile_delegation_cancellations(&ctx.thread_id, ctx.commit.as_ref())
        .await;
    // Direct ingress has no later settlement edge, so reopening it repairs
    // the generic after-commit observer gap from committed Run truth. A
    // durable Runtime must leave this delivery to its guarded Worker/HTTP
    // settlement owner, including ordinary cold contexts opened without a
    // claimed identity.
    if !ctx.delivery.is_durable()
        && let Some(run) = ctx
            .commit
            .authoritative_latest_run(&ctx.thread_id)
            .await
            .map_err(HostError::internal)?
        && run.state.is_terminal()
    {
        let _ = awaken_runtime_contract::terminal::redeliver_committed_terminal(
            ctx.commit.as_ref(),
            &ctx.terminal_observers,
            &run.id,
            &ctx.thread_id,
        )
        .await;
    }
    Ok(ctx)
}
