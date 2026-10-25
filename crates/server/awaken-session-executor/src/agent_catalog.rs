// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: This source implements the repository-controlled snapshot product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! A process-startup registry mapping a human agent id to its compiled
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
    use awaken_session_run_delivery::RUN_PUBLICATIONS;

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
    fn run_publication_closure_freezes_custom_auxiliaries_and_keeps_default_fallbacks() {
        // Cause/effect graph: C1 an active extension selects its built-in id or
        // an explicit custom id; C2 the custom publication is present/absent;
        // C3 multiple extension roles may contribute to one immutable closure;
        // C4 the selected auxiliary id is the declaring owner itself, the top
        // activation root, or a different execution owner. An ordinary Agent may
        // fill two compatible roles, but a self-edge cannot acquire an auxiliary
        // capability set and a top-root back-edge cannot cross the non-root wire.
        // Effects: E1 default ids remain compatible with an empty legacy bundle;
        // E2 every explicit custom id is frozen once; E3 a missing custom id is
        // rejected before dispatch instead of silently changing to a built-in;
        // E4 self/root back-edges are rejected before any auxiliary executor is
        // built; E5 a different execution owner's exact snapshot is reused.
        //
        // | Rule | Extension id | Publication | Effect |
        // |---|---|---|---|
        // | P1 | default | absent | E1 |
        // | P2 | custom | present | E2 |
        // | P3 | custom | absent | E3 |
        // | P4 | self or top-root back-edge | any | E4 |
        // | P5 | different execution owner | present | E5 |
        // Constraint/Invariant: `RunDispatch.agent_publications` is the sole
        // non-root execution-publication closure; built-ins are deterministic
        // compatibility defaults, never substitutes for a custom Agent. P1-P5
        // cover default compatibility, both custom-publication partitions, and
        // every execution-owner relationship.
        let defaults = ExecutableAgentSnapshot::builder("assistant")
            .plugins([
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                awaken_ext_compact::COMPACT_PLUGIN_ID.to_string(),
            ])
            .model(ModelBinding::new("default", "stub", "default"))
            .build();
        assert!(
            RUN_PUBLICATIONS
                .freeze(&defaults, None, "workspace")
                .expect("default auxiliary Agents have deterministic built-ins")
                .is_empty(),
            "P1/E1"
        );

        let custom_ids = ["extractor-custom", "selector-custom", "compactor-custom"];
        let custom = ExecutableAgentSnapshot::builder("assistant")
            .plugins([
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                awaken_ext_compact::COMPACT_PLUGIN_ID.to_string(),
            ])
            .plugin_config([
                (
                    awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                    serde_json::json!({
                        "agent_id": custom_ids[0],
                        "selector_agent_id": custom_ids[1]
                    }),
                ),
                (
                    awaken_ext_compact::COMPACT_PLUGIN_ID.to_string(),
                    serde_json::json!({"agent_id": custom_ids[2]}),
                ),
            ])
            .model(ModelBinding::new("default", "stub", "default"))
            .build();
        let publications = custom_ids
            .iter()
            .map(|id| config(id, &format!("{id} instructions")))
            .collect::<Vec<_>>();
        let source =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new(publications.clone())
                .expect("three exact auxiliary publications");
        let frozen = RUN_PUBLICATIONS
            .freeze(&custom, Some(&source), "workspace")
            .expect("all explicit auxiliary publications are available");
        assert_eq!(
            frozen
                .iter()
                .map(|snapshot| snapshot.root_agent_id.0.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            custom_ids.into_iter().collect(),
            "P2/E2"
        );

        for missing in custom_ids {
            let incomplete = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new(
                publications
                    .iter()
                    .filter(|snapshot| snapshot.root_agent_id.0 != missing)
                    .cloned(),
            )
            .expect("remaining publications are complete snapshots");
            let error = RUN_PUBLICATIONS
                .freeze(&custom, Some(&incomplete), "workspace")
                .expect_err("a custom auxiliary publication cannot fall back");
            assert!(error.contains(missing), "P3/E3: {error}");
        }

        let self_roles = [
            (
                "Memory extractor",
                awaken_ext_memory::MEMORY_PLUGIN_ID,
                serde_json::json!({
                    "agent_id": "assistant",
                    "recall_enabled": false
                }),
            ),
            (
                "Memory selector",
                awaken_ext_memory::MEMORY_PLUGIN_ID,
                serde_json::json!({
                    "extraction_enabled": false,
                    "selector_agent_id": "assistant"
                }),
            ),
            (
                "Compact",
                awaken_ext_compact::COMPACT_PLUGIN_ID,
                serde_json::json!({"agent_id": "assistant"}),
            ),
        ];
        for (role, plugin_id, plugin_config) in self_roles {
            let root_self = ExecutableAgentSnapshot::builder("assistant")
                .plugins([plugin_id.to_string()])
                .plugin_config([(plugin_id.to_string(), plugin_config)])
                .model(ModelBinding::new("default", "stub", "default"))
                .build();
            let error = match RUN_PUBLICATIONS.freeze(&root_self, None, "workspace") {
                Ok(frozen) => panic!("P4/E4 {role} self unexpectedly froze {frozen:?}"),
                Err(error) => error,
            };
            assert!(
                error.contains("select itself"),
                "P4/E4 {role} self: {error}"
            );
        }

        let delegate = config("delegate-owner", "delegate");
        let mut root_to_delegate = ExecutableAgentSnapshot::builder("assistant")
            .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()])
            .plugin_config([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                serde_json::json!({
                    "agent_id": "delegate-owner",
                    "recall_enabled": false
                }),
            )])
            .model(ModelBinding::new("default", "stub", "default"))
            .build();
        root_to_delegate.resolved_spec.plugin_config.agent.delegates = vec![
            awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                agent_id: delegate.root_agent_id.clone(),
                source_revision: None,
                recursive_self: false,
            },
        ];
        root_to_delegate
            .recompute_fingerprint()
            .expect("recompute root-to-delegate fixture");
        let source =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([delegate.clone()])
                .expect("one delegate owner publication");
        assert_eq!(
            RUN_PUBLICATIONS
                .freeze(&root_to_delegate, Some(&source), "workspace")
                .expect("a different owner may fill a compatible auxiliary role"),
            vec![delegate],
            "P5/E5 root to delegate"
        );

        let mut delegate_self = ExecutableAgentSnapshot::builder("delegate-self")
            .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()])
            .plugin_config([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                serde_json::json!({
                    "agent_id": "delegate-self",
                    "recall_enabled": false
                }),
            )])
            .model(ModelBinding::new("default", "stub", "default"))
            .build();
        delegate_self
            .recompute_fingerprint()
            .expect("recompute delegate self fixture");
        let mut root = config("assistant", "root");
        root.resolved_spec.plugin_config.agent.delegates = vec![
            awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                agent_id: delegate_self.root_agent_id.clone(),
                source_revision: None,
                recursive_self: false,
            },
        ];
        root.recompute_fingerprint()
            .expect("recompute root with self-edge delegate");
        let source =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([delegate_self])
                .expect("delegate self publication");
        let error = RUN_PUBLICATIONS
            .freeze(&root, Some(&source), "workspace")
            .expect_err("a delegated owner cannot select itself as auxiliary");
        assert!(
            error.contains("delegate-self") && error.contains("select itself"),
            "P4/E4 delegate self: {error}"
        );

        let delegate_to_root = ExecutableAgentSnapshot::builder("delegate-back-edge")
            .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()])
            .plugin_config([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                serde_json::json!({
                    "agent_id": "assistant",
                    "recall_enabled": false
                }),
            )])
            .model(ModelBinding::new("default", "stub", "default"))
            .build();
        let mut root = config("assistant", "root");
        root.resolved_spec.plugin_config.agent.delegates = vec![
            awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                agent_id: delegate_to_root.root_agent_id.clone(),
                source_revision: None,
                recursive_self: false,
            },
        ];
        root.recompute_fingerprint()
            .expect("recompute root with back-edge delegate");
        let source =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([delegate_to_root])
                .expect("delegate back-edge publication");
        let error = RUN_PUBLICATIONS
            .freeze(&root, Some(&source), "workspace")
            .expect_err("the activation root cannot enter the non-root source");
        assert!(
            error.contains("activation root"),
            "P4/E4 delegate to top root: {error}"
        );

        let delegate_b = config("delegate-b", "delegate b");
        let delegate_a = ExecutableAgentSnapshot::builder("delegate-a")
            .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()])
            .plugin_config([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                serde_json::json!({
                    "agent_id": "delegate-b",
                    "recall_enabled": false
                }),
            )])
            .model(ModelBinding::new("default", "stub", "default"))
            .build();
        let mut root = config("assistant", "root");
        root.resolved_spec.plugin_config.agent.delegates = [
            delegate_a.root_agent_id.clone(),
            delegate_b.root_agent_id.clone(),
        ]
        .into_iter()
        .map(
            |agent_id| awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                agent_id,
                source_revision: None,
                recursive_self: false,
            },
        )
        .collect();
        root.recompute_fingerprint()
            .expect("recompute two-delegate root");
        let source = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([
            delegate_a.clone(),
            delegate_b.clone(),
        ])
        .expect("two compatible delegate publications");
        let frozen = RUN_PUBLICATIONS
            .freeze(&root, Some(&source), "workspace")
            .expect("one delegate may fill another compatible auxiliary role");
        assert_eq!(
            frozen
                .iter()
                .map(|snapshot| snapshot.root_agent_id.0.as_str())
                .collect::<std::collections::BTreeSet<_>>(),
            [delegate_a, delegate_b]
                .iter()
                .map(|snapshot| snapshot.root_agent_id.0.as_str())
                .collect(),
            "P5/E5 delegate to different delegate"
        );
    }

    #[test]
    fn exact_run_source_accepts_only_the_complete_non_root_closure() {
        // Cause/effect graph: C1 the transported bundle is complete, missing,
        // extra, or structurally duplicate; C2 the root is/is-not kept outside
        // the bundle. Effects: E1 return one immutable lookup containing only
        // the exact non-root closure; E2 reject every inexact/invalid bundle.
        // Constraint: this helper validates one admitted `RunDispatch`; it does
        // not create a second wire projection or add the activation root.
        //
        // | Rule | Bundle | Root in bundle | Effect |
        // |---|---|---|---|
        // | H1 | complete | no | E1 |
        // | H2 | missing | no | E2 |
        // | H3 | extra | no | E2 |
        // | H4 | duplicate | no | E2 |
        let auxiliary = config("custom-extractor", "extract");
        let root = ExecutableAgentSnapshot::builder("assistant")
            .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()])
            .plugin_config([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                serde_json::json!({
                    "agent_id": "custom-extractor",
                    "recall_enabled": false
                }),
            )])
            .model(ModelBinding::new("default", "stub", "default"))
            .build();

        let source = RUN_PUBLICATIONS
            .exact_source(&root, std::slice::from_ref(&auxiliary), "workspace")
            .expect("H1 exact non-root closure");
        use awaken_runtime_contract::PublishedAgentSnapshotSource as _;
        assert!(
            source.current("workspace", &root.root_agent_id).is_none(),
            "H1/E1 root remains exclusively in RunActivation"
        );
        assert_eq!(
            source.current("workspace", &auxiliary.root_agent_id),
            Some(auxiliary.clone()),
            "H1/E1"
        );
        assert!(
            RUN_PUBLICATIONS
                .exact_source(&root, &[], "workspace")
                .is_err(),
            "H2/E2"
        );
        assert!(
            RUN_PUBLICATIONS
                .exact_source(
                    &root,
                    &[auxiliary.clone(), config("extra", "extra")],
                    "workspace",
                )
                .is_err(),
            "H3/E2"
        );
        assert!(
            RUN_PUBLICATIONS
                .exact_source(&root, &[auxiliary.clone(), auxiliary], "workspace",)
                .is_err(),
            "H4/E2"
        );
    }
}
