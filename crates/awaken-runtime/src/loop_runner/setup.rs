//! Run setup: resolve agent, trim history, detect resume.

use std::sync::Arc;

use crate::phase::PhaseRuntime;
use crate::registry::{AgentResolver, ResolvedAgent};
use crate::state::MutationBatch;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::message::Message;

use super::AgentLoopError;

/// All resolved state needed before the main loop begins.
pub(super) struct PreparedRun {
    pub agent: ResolvedAgent,
    pub messages: Vec<Arc<Message>>,
}

/// Resolve the agent, trim compaction history, and replay any suspended tool calls.
pub(super) async fn prepare_run(
    resolver: &dyn AgentResolver,
    runtime: &PhaseRuntime,
    initial_agent_id: &str,
    initial_messages: Vec<Message>,
    _run_identity: &RunIdentity,
) -> Result<PreparedRun, AgentLoopError> {
    let store = runtime.store();
    let mut messages: Vec<Arc<Message>> = initial_messages.into_iter().map(Arc::new).collect();

    // Resolve initial agent
    let agent = resolver
        .resolve(initial_agent_id)
        .map_err(AgentLoopError::RuntimeError)?;

    // Install plugin state keys into the store so persistence and commit can find them.
    if !agent.env.key_registrations.is_empty() {
        store
            .register_keys(&agent.env.key_registrations)
            .map_err(AgentLoopError::PhaseError)?;
    }

    // Activate plugins for the initial agent.
    {
        let mut activate_patch = MutationBatch::new();
        for plugin in &agent.env.plugins {
            plugin
                .on_activate(&agent.spec, &mut activate_patch)
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

    Ok(PreparedRun { agent, messages })
}
