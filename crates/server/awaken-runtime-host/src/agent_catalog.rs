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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

/// Freeze every non-root Agent publication one Run can execute through. The
/// runtime-contract owns recursive delegation closure; this host-level owner
/// extends that same closure with extension-authored ordinary auxiliary Agents
/// rather than letting a cold Worker or replay consult a mutable catalog later.
pub(crate) fn freeze_run_publications(
    parent: &ExecutableAgentSnapshot,
    source: Option<&dyn awaken_runtime_contract::PublishedAgentSnapshotSource>,
    workspace_id: &str,
) -> Result<Vec<ExecutableAgentSnapshot>, String> {
    let mut frozen =
        awaken_runtime_contract::freeze_delegation_publications(parent, source, workspace_id)
            .map_err(|error| error.to_string())?;
    let owners = std::iter::once(parent)
        .chain(frozen.iter())
        .cloned()
        .collect::<Vec<_>>();
    let owner_ids = owners
        .iter()
        .map(|snapshot| snapshot.root_agent_id.0.clone())
        .collect::<HashSet<_>>();
    let mut seen = std::iter::once(parent)
        .chain(frozen.iter())
        .map(|snapshot| snapshot.fingerprint.clone())
        .collect::<HashSet<_>>();
    // The bool records whether the id was explicitly customized. Default ids
    // have deterministic built-ins and may be absent from legacy publication
    // bundles; a custom id has no such fallback and must be frozen exactly.
    let mut auxiliary_ids = BTreeMap::<(String, String), bool>::new();
    let mut require_auxiliary = |declaring_owner_id: &str, agent_id: String, default_id: &str| {
        let required = agent_id != default_id;
        auxiliary_ids
            .entry((declaring_owner_id.to_string(), agent_id))
            .and_modify(|existing| *existing |= required)
            .or_insert(required);
    };
    for owner in &owners {
        if owner
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_memory::MEMORY_PLUGIN_ID)
        {
            let config = awaken_ext_memory::MemoryConfig::from_value(
                owner
                    .resolved_spec
                    .plugin_config
                    .get(awaken_ext_memory::MEMORY_PLUGIN_ID),
            )
            .map_err(|error| {
                format!(
                    "Agent `{}` has invalid Memory configuration: {error}",
                    owner.root_agent_id.0
                )
            })?;
            if config.extraction_enabled {
                require_auxiliary(
                    &owner.root_agent_id.0,
                    config
                        .agent_id
                        .unwrap_or_else(|| awaken_ext_memory::MEMORY_AGENT_ID.to_string()),
                    awaken_ext_memory::MEMORY_AGENT_ID,
                );
            }
            if config.recall_enabled {
                require_auxiliary(
                    &owner.root_agent_id.0,
                    config
                        .selector_agent_id
                        .unwrap_or_else(|| awaken_ext_memory::SELECTOR_AGENT_ID.to_string()),
                    awaken_ext_memory::SELECTOR_AGENT_ID,
                );
            }
        }
        if owner
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID)
        {
            let config = owner
                .resolved_spec
                .plugin_config
                .get(awaken_ext_compact::COMPACT_PLUGIN_ID)
                .map_or_else(
                    || Ok(awaken_ext_compact::CompactConfig::default()),
                    |value| serde_json::from_value(value.clone()),
                )
                .map_err(|error| {
                    format!(
                        "Agent `{}` has invalid Compact configuration: {error}",
                        owner.root_agent_id.0
                    )
                })?;
            require_auxiliary(
                &owner.root_agent_id.0,
                config.agent_id,
                awaken_ext_compact::COMPACT_AGENT_ID,
            );
        }
    }
    for ((declaring_owner_id, agent_id), required) in auxiliary_ids {
        if agent_id == declaring_owner_id {
            return Err(format!(
                "Agent `{declaring_owner_id}` cannot select itself as auxiliary Agent `{agent_id}`"
            ));
        }
        if agent_id == parent.root_agent_id.0 {
            return Err(format!(
                "auxiliary Agent `{agent_id}` selected by `{declaring_owner_id}` points back to the activation root, which is not part of the non-root publication source"
            ));
        }
        if owner_ids.contains(&agent_id) {
            continue;
        }
        let snapshot = source.and_then(|source| {
            source.current(
                workspace_id,
                &awaken_runtime_contract::snapshot::AgentId(agent_id.clone()),
            )
        });
        let Some(snapshot) = snapshot else {
            if required {
                return Err(format!(
                    "custom auxiliary Agent `{agent_id}` has no published snapshot"
                ));
            }
            continue;
        };
        if snapshot.root_agent_id.0 != agent_id {
            return Err(format!(
                "auxiliary Agent `{agent_id}` resolved to publication `{}`",
                snapshot.root_agent_id.0
            ));
        }
        if seen.insert(snapshot.fingerprint.clone()) {
            frozen.push(snapshot);
        }
    }
    Ok(frozen)
}

/// Reconstruct and validate the exact non-root publication source carried by
/// one admitted Run. The root remains exclusively in `RunActivation`; it is an
/// input to closure validation, never an entry in this lookup source.
pub(crate) fn exact_run_publication_source(
    root: &ExecutableAgentSnapshot,
    non_root: &[ExecutableAgentSnapshot],
    workspace_id: &str,
) -> Result<Arc<awaken_runtime_contract::StaticPublishedAgentSnapshots>, String> {
    let source = Arc::new(
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new(non_root.iter().cloned())
            .map_err(|error| format!("invalid Agent publication closure: {error}"))?,
    );
    let expected = freeze_run_publications(root, Some(source.as_ref()), workspace_id)?;
    let identities = |snapshots: &[ExecutableAgentSnapshot]| {
        let mut values = snapshots
            .iter()
            .map(|snapshot| {
                (
                    snapshot.root_agent_id.0.clone(),
                    snapshot.fingerprint.0.clone(),
                )
            })
            .collect::<Vec<_>>();
        values.sort_unstable();
        values
    };
    if identities(&expected) != identities(non_root) {
        return Err("inexact Agent publication closure".into());
    }
    Ok(source)
}

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
) -> Result<ExecutableAgentSnapshot, String> {
    let published = publications.and_then(|source| {
        source.current(
            workspace_id,
            &awaken_runtime_contract::snapshot::AgentId(agent_id.to_string()),
        )
    });
    let mut snapshot = match published {
        Some(snapshot) if snapshot.root_agent_id.0 == agent_id => snapshot,
        Some(snapshot) => {
            return Err(format!(
                "auxiliary Agent `{agent_id}` resolved to publication `{}`",
                snapshot.root_agent_id.0
            ));
        }
        None if fallback.root_agent_id.0 == agent_id => fallback,
        None => {
            return Err(format!(
                "custom auxiliary Agent `{agent_id}` has no published snapshot"
            ));
        }
    };
    if let Some(instructions) = instructions_override.filter(|value| !value.trim().is_empty()) {
        snapshot.resolved_spec.instructions = instructions.to_string();
        snapshot
            .recompute_fingerprint()
            .expect("an executable Agent snapshot is JSON-serializable");
    }
    Ok(snapshot)
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
        // fingerprint; R4 blank override -> R2 unchanged; R5 custom id without
        // publication -> reject rather than execute the differently-named built-in.
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
                .expect("the matching built-in is the default")
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
            )
            .expect("the exact publication exists"),
            published,
            "R2/R4"
        );
        let derived = resolve_auxiliary_snapshot(
            Some(&source),
            "ws",
            "memory-extractor",
            fallback,
            Some("per-agent"),
        )
        .expect("the exact publication exists");
        assert_eq!(derived.resolved_spec.instructions, "per-agent", "R3");
        assert_eq!(
            derived.fingerprint,
            derived.resolved_spec.catalog_fingerprint
        );
        assert_ne!(derived.fingerprint, published.fingerprint, "R3");
        assert!(
            resolve_auxiliary_snapshot(
                None,
                "ws",
                "custom-extractor",
                config("memory-extractor", "built-in"),
                None,
            )
            .is_err(),
            "R5: a custom id cannot silently execute the built-in Agent"
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
            freeze_run_publications(&defaults, None, "workspace")
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
        let frozen = freeze_run_publications(&custom, Some(&source), "workspace")
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
            let error = freeze_run_publications(&custom, Some(&incomplete), "workspace")
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
            let error = match freeze_run_publications(&root_self, None, "workspace") {
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
            freeze_run_publications(&root_to_delegate, Some(&source), "workspace")
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
        let error = freeze_run_publications(&root, Some(&source), "workspace")
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
        let error = freeze_run_publications(&root, Some(&source), "workspace")
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
        let frozen = freeze_run_publications(&root, Some(&source), "workspace")
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

        let source =
            exact_run_publication_source(&root, std::slice::from_ref(&auxiliary), "workspace")
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
            exact_run_publication_source(&root, &[], "workspace").is_err(),
            "H2/E2"
        );
        assert!(
            exact_run_publication_source(
                &root,
                &[auxiliary.clone(), config("extra", "extra")],
                "workspace",
            )
            .is_err(),
            "H3/E2"
        );
        assert!(
            exact_run_publication_source(&root, &[auxiliary.clone(), auxiliary], "workspace",)
                .is_err(),
            "H4/E2"
        );
    }
}
