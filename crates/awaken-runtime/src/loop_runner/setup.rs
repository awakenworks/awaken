//! Run setup: resolve agent, trim history, detect resume.

use std::sync::Arc;

use crate::agent::config::AgentConfig;
use crate::phase::{ExecutionEnv, PhaseRuntime};
use crate::registry::{AgentResolver, ResolvedAgent};
use crate::state::MutationBatch;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::message::Message;

use super::AgentLoopError;
use super::resume::detect_and_replay_resume;

/// All resolved state needed before the main loop begins.
pub(super) struct PreparedRun {
    pub agent: AgentConfig,
    pub env: ExecutionEnv,
    pub messages: Vec<Arc<Message>>,
}

/// Resolve the agent, trim compaction history, and replay any suspended tool calls.
pub(super) async fn prepare_run(
    resolver: &dyn AgentResolver,
    runtime: &PhaseRuntime,
    initial_agent_id: &str,
    initial_messages: Vec<Message>,
    run_identity: &RunIdentity,
) -> Result<PreparedRun, AgentLoopError> {
    let store = runtime.store();
    let mut messages: Vec<Arc<Message>> = initial_messages.into_iter().map(Arc::new).collect();

    // Resolve initial agent
    let ResolvedAgent { config: agent, env } = resolver
        .resolve(initial_agent_id)
        .map_err(AgentLoopError::RuntimeError)?;

    // Install plugin state keys into the store so persistence and commit can find them.
    if !env.key_registrations.is_empty() {
        store
            .register_keys(&env.key_registrations)
            .map_err(AgentLoopError::PhaseError)?;
    }

    // Activate plugins for the initial agent.
    if let Some(ref spec) = env.agent_spec {
        let mut activate_patch = MutationBatch::new();
        for plugin in &env.plugins {
            plugin
                .on_activate(spec, &mut activate_patch)
                .map_err(AgentLoopError::PhaseError)?;
        }
        if !activate_patch.is_empty() {
            store
                .commit(activate_patch)
                .map_err(AgentLoopError::PhaseError)?;
        }
    }

    // Trim to latest compaction boundary — skip already-summarized history
    if agent.context_policy().is_some() {
        crate::context::trim_to_compaction_boundary(&mut messages);
    }

    // State-driven resume detection: replay any Resuming tool calls.
    detect_and_replay_resume(&agent, store, run_identity, &mut messages).await?;

    Ok(PreparedRun {
        agent,
        env,
        messages,
    })
}
