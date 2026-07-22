//! Workspace-scoped live catalog of published Agent snapshots.
//!
//! Durable authoring repositories remain the source of publication history. This
//! component owns only the process-local hot index used by execution and keeps its
//! Workspace partition intrinsic and authorization-free.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use awaken_config_store::ExecutableAgentSnapshot;

#[derive(Clone)]
struct InstalledEntry {
    source_revision: u64,
    snapshot: ExecutableAgentSnapshot,
}

#[derive(Default)]
pub(crate) struct InstalledAgentCatalog {
    entries: Mutex<HashMap<(String, String), InstalledEntry>>,
    unavailable: Mutex<HashSet<(String, String)>>,
}

impl InstalledAgentCatalog {
    pub(crate) fn install(
        &self,
        workspace: &str,
        agent_id: &str,
        source_revision: u64,
        snapshot: ExecutableAgentSnapshot,
    ) {
        let key = (workspace.to_string(), agent_id.to_string());
        self.unavailable
            .lock()
            .expect("unavailable Agent catalog")
            .remove(&key);
        let mut entries = self.entries.lock().expect("installed Agent catalog");
        if entries
            .get(&key)
            .is_none_or(|current| source_revision >= current.source_revision)
        {
            entries.insert(
                key,
                InstalledEntry {
                    source_revision,
                    snapshot,
                },
            );
        }
    }

    pub(crate) fn snapshot_in(
        &self,
        workspace: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSnapshot> {
        self.entries
            .lock()
            .expect("installed Agent catalog")
            .get(&(workspace.to_string(), agent_id.to_string()))
            .map(|entry| entry.snapshot.clone())
    }

    pub(crate) fn uninstall(&self, workspace: &str, agent_id: &str) {
        let key = (workspace.to_string(), agent_id.to_string());
        self.entries
            .lock()
            .expect("installed Agent catalog")
            .remove(&key);
        self.unavailable
            .lock()
            .expect("unavailable Agent catalog")
            .insert(key);
    }

    pub(crate) fn is_unavailable(&self, workspace: &str, agent_id: &str) -> bool {
        self.unavailable
            .lock()
            .expect("unavailable Agent catalog")
            .contains(&(workspace.to_string(), agent_id.to_string()))
    }

    pub(crate) fn agents_referencing_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Vec<String> {
        let entries = self.entries.lock().expect("installed Agent catalog");
        let mut agents: Vec<_> = entries
            .iter()
            .filter_map(|((workspace, agent_id), entry)| {
                (workspace == workspace_id)
                    .then(|| {
                        awaken_runtime_contract::agent_bindings::AgentBindings::from_config(
                            &entry.snapshot.resolved_spec.plugin_config,
                        )
                    })
                    .flatten()
                    .filter(|bindings| bindings.skill_ids.iter().any(|id| id == skill_id))
                    .map(|_| agent_id.clone())
            })
            .collect();
        agents.sort();
        agents
    }
}
