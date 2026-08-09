//! A composition-root registry mapping a human agent id to its compiled
//! [`ExecutableAgentSnapshot`].
//!
//! Every locally-runnable agent — the main assistant, native delegates, and the
//! auxiliary agents (memory extractor, judge, compactor) — is one entry here, so
//! a sub-run resolves its spec (instructions, model, tools) *by id* instead of
//! sharing a single hard-coded config. Closing that gap is what lets memory /
//! goal / compact each be an ordinary, separately-configured agent rather than a
//! bespoke mechanism.
//!
//! The catalog is data-only: it holds already-compiled `ExecutableAgentSnapshot`s (from
//! `ExecutableAgentSnapshot::builder` or `awaken-config-store::compile`). It never reaches
//! a store, a model, or the kernel — the sub-run driver reads it to resolve an id.

use std::collections::HashMap;

use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

/// Resolve one auxiliary Agent through the same Coordinator publication source
/// as a foreground Session. The built-in snapshot is only the absent-publication
/// default for the same id; it is not a second mutable catalog. A per-caller
/// instruction override derives one complete snapshot and moves all fingerprint
/// fields together.
pub(crate) fn resolve_auxiliary_snapshot(
    publications: Option<&dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
    workspace_id: &str,
    agent_id: &str,
    fallback: ExecutableAgentSnapshot,
    instructions_override: Option<&str>,
) -> ExecutableAgentSnapshot {
    let mut snapshot = publications
        .and_then(|source| {
            source.current(
                workspace_id,
                &awaken_runtime_contract::snapshot::AgentId(agent_id.to_string()),
            )
        })
        .unwrap_or(fallback);
    if let Some(instructions) = instructions_override.filter(|value| !value.trim().is_empty()) {
        snapshot.resolved_spec.instructions = instructions.to_string();
        snapshot
            .recompute_fingerprint()
            .expect("an executable Agent snapshot is JSON-serializable");
    }
    snapshot
}

/// Maps an agent id to its executable snapshot. A later registration for the same id
/// replaces the earlier one (last write wins), so a host can layer defaults then
/// overrides.
#[derive(Clone, Default)]
pub struct AgentCatalog {
    configs: HashMap<String, ExecutableAgentSnapshot>,
}

impl AgentCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `config` under its own agent id (`root_agent_id`).
    pub fn insert(&mut self, config: ExecutableAgentSnapshot) {
        let id = config.root_agent_id.0.clone();
        self.configs.insert(id, config);
    }

    /// Builder-style [`insert`](Self::insert), for one-liner assembly.
    #[must_use]
    pub fn with_agent(mut self, config: ExecutableAgentSnapshot) -> Self {
        self.insert(config);
        self
    }

    /// The config registered for `agent_id`, if any.
    pub fn resolve(&self, agent_id: &str) -> Option<&ExecutableAgentSnapshot> {
        self.configs.get(agent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::ModelBinding;

    fn config(id: &str, instructions: &str) -> ExecutableAgentSnapshot {
        ExecutableAgentSnapshot::builder(id)
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
                .resolved_spec
                .instructions,
            "be helpful"
        );
        assert_eq!(
            catalog.resolve("judge").unwrap().resolved_spec.instructions,
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
                .resolved_spec
                .instructions,
            "v2"
        );
    }

    #[test]
    fn auxiliary_resolution_uses_one_publication_source_and_derives_overrides() {
        // Cause/effect decision table: R1 no publication -> built-in snapshot;
        // R2 publication present -> exact published snapshot; R3 R2 + nonblank
        // caller instructions -> complete derived snapshot with a new coherent
        // fingerprint; R4 blank override -> R2 unchanged.
        struct Publications(ExecutableAgentSnapshot);
        impl awaken_runtime_contract::PublishedAgentSnapshotSource for Publications {
            fn current(
                &self,
                _workspace: &str,
                agent_id: &awaken_runtime_contract::snapshot::AgentId,
            ) -> Option<ExecutableAgentSnapshot> {
                (agent_id.0 == self.0.root_agent_id.0).then(|| self.0.clone())
            }

            fn exact(
                &self,
                _workspace: &str,
                _fingerprint: &awaken_runtime_contract::resolved::CatalogFingerprint,
            ) -> Option<ExecutableAgentSnapshot> {
                None
            }

            fn at_revision(
                &self,
                _workspace: &str,
                _agent_id: &awaken_runtime_contract::snapshot::AgentId,
                _source_revision: u64,
            ) -> Option<ExecutableAgentSnapshot> {
                None
            }
        }

        let fallback = config("memory-extractor", "built-in");
        let published = config("memory-extractor", "published");
        assert_eq!(
            resolve_auxiliary_snapshot(None, "ws", "memory-extractor", fallback.clone(), None)
                .resolved_spec
                .instructions,
            "built-in",
            "R1"
        );
        let source = Publications(published.clone());
        assert_eq!(
            resolve_auxiliary_snapshot(
                Some(&source),
                "ws",
                "memory-extractor",
                fallback.clone(),
                Some(" "),
            ),
            published,
            "R2/R4"
        );
        let derived = resolve_auxiliary_snapshot(
            Some(&source),
            "ws",
            "memory-extractor",
            fallback,
            Some("per-agent"),
        );
        assert_eq!(derived.resolved_spec.instructions, "per-agent", "R3");
        assert_eq!(
            derived.fingerprint,
            derived.resolved_spec.catalog_fingerprint
        );
        assert_ne!(derived.fingerprint, published.fingerprint, "R3");
    }
}
