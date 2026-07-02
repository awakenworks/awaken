//! A composition-root registry mapping a human agent id to its compiled
//! [`RunnableConfig`].
//!
//! Every locally-runnable agent — the main assistant, native delegates, and the
//! auxiliary agents (memory extractor, judge, compactor) — is one entry here, so
//! a sub-run resolves its spec (instructions, model, tools) *by id* instead of
//! sharing a single hard-coded config. Closing that gap is what lets memory /
//! goal / compact each be an ordinary, separately-configured agent rather than a
//! bespoke mechanism.
//!
//! The catalog is data-only: it holds already-compiled `RunnableConfig`s (from
//! `RunnableConfig::builder` or `awaken-config-store::compile`). It never reaches
//! a store, a model, or the kernel — the sub-run driver reads it to resolve an id.

use std::collections::HashMap;

use awaken_runtime_contract::runnable::RunnableConfig;

/// Maps an agent id to its runnable config. A later registration for the same id
/// replaces the earlier one (last write wins), so a host can layer defaults then
/// overrides.
#[derive(Clone, Default)]
pub struct AgentCatalog {
    configs: HashMap<String, RunnableConfig>,
}

impl AgentCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `config` under its own agent id (`root_agent_id`).
    pub fn insert(&mut self, config: RunnableConfig) {
        let id = config.snapshot().root_agent_id.0.clone();
        self.configs.insert(id, config);
    }

    /// Builder-style [`insert`](Self::insert), for one-liner assembly.
    #[must_use]
    pub fn with_agent(mut self, config: RunnableConfig) -> Self {
        self.insert(config);
        self
    }

    /// The config registered for `agent_id`, if any.
    pub fn resolve(&self, agent_id: &str) -> Option<&RunnableConfig> {
        self.configs.get(agent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::ModelBinding;

    fn config(id: &str, instructions: &str) -> RunnableConfig {
        RunnableConfig::builder(id)
            .instructions(instructions)
            .model(ModelBinding::new("default", "stub", "default"))
            .build()
    }

    #[test]
    fn resolves_each_agent_by_its_own_id() {
        let catalog = AgentCatalog::new()
            .with_agent(config("assistant", "be helpful"))
            .with_agent(config("judge", "be strict"));

        assert_eq!(
            catalog
                .resolve("assistant")
                .unwrap()
                .snapshot()
                .resolved_spec
                .instructions,
            "be helpful"
        );
        assert_eq!(
            catalog
                .resolve("judge")
                .unwrap()
                .snapshot()
                .resolved_spec
                .instructions,
            "be strict"
        );
        assert!(catalog.resolve("missing").is_none());
    }

    #[test]
    fn last_registration_wins() {
        let catalog = AgentCatalog::new()
            .with_agent(config("memory-extractor", "v1"))
            .with_agent(config("memory-extractor", "v2"));

        assert_eq!(
            catalog
                .resolve("memory-extractor")
                .unwrap()
                .snapshot()
                .resolved_spec
                .instructions,
            "v2"
        );
    }
}
