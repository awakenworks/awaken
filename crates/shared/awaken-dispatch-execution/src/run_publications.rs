// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: The original complete frozen publication closure is shared by all execution callers.
//! Exact non-root executable publication closure of an admitted Run.
use awaken_runtime_contract::ExecutableAgentSnapshot;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};
/// Freeze every non-root Agent publication one Run can execute through. The
/// runtime-contract owns recursive delegation closure; this host-level owner
/// extends that same closure with extension-authored ordinary auxiliary Agents
/// rather than letting a cold Worker or replay consult a mutable catalog later.
pub fn freeze_run_publications(
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
pub fn exact_run_publication_source(
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
