//! Run input and active agent state key.
//!
//! `AgentProfile` has been merged into `AgentSpec` — use `crate::registry::spec::AgentSpec`
//! for agent configuration and plugin config access. `PluginConfigKey` is also on `AgentSpec`.

// Re-export PluginConfigKey and AgentSpec from their canonical location for convenience.
pub use crate::registry::spec::{AgentSpec, PluginConfigKey};

use crate::contract::identity::RunIdentity;

// ---------------------------------------------------------------------------
// RunInput
// ---------------------------------------------------------------------------

/// Per-run caller input — immutable for the duration of a run.
///
/// Contains user overrides and run identity. Set once at `run_agent_loop` entry.
#[derive(Debug, Clone, Default)]
pub struct RunInput {
    /// Override model for this run.
    pub model_override: Option<String>,
    /// Override max rounds for this run.
    pub max_rounds_override: Option<usize>,
    /// Run identity (thread_id, run_id, etc).
    pub identity: RunIdentity,
}

// ---------------------------------------------------------------------------
// ActiveAgentKey
// ---------------------------------------------------------------------------

/// StateKey for the active agent ID. Handoff writes this.
pub struct ActiveAgentKey;

impl crate::state::StateKey for ActiveAgentKey {
    const KEY: &'static str = "__runtime.active_agent";
    type Value = Option<String>;
    type Update = Option<String>;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_agent_key_apply() {
        use crate::state::StateKey;
        let mut val: Option<String> = None;
        ActiveAgentKey::apply(&mut val, Some("reviewer".into()));
        assert_eq!(val.as_deref(), Some("reviewer"));
        ActiveAgentKey::apply(&mut val, None);
        assert!(val.is_none());
    }
}
