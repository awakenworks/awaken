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
    declared_hand: Option<String>,
}

#[derive(Default)]
pub(crate) struct InstalledAgentCatalog {
    entries: Mutex<HashMap<(String, String), InstalledEntry>>,
    publications: Mutex<HashMap<(String, String), ExecutableAgentSnapshot>>,
    unavailable: Mutex<HashSet<(String, String)>>,
}

impl InstalledAgentCatalog {
    pub(crate) fn install(
        &self,
        workspace: &str,
        agent_id: &str,
        source_revision: u64,
        snapshot: ExecutableAgentSnapshot,
        declared_hand: Option<String>,
    ) {
        let key = (workspace.to_string(), agent_id.to_string());
        self.unavailable
            .lock()
            .expect("unavailable Agent catalog")
            .remove(&key);
        self.publications
            .lock()
            .expect("published Agent catalog")
            .insert(
                (workspace.to_string(), snapshot.fingerprint.0.clone()),
                snapshot.clone(),
            );
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
                    declared_hand,
                },
            );
        }
    }

    pub(crate) fn snapshot_by_fingerprint(
        &self,
        workspace: &str,
        fingerprint: &str,
    ) -> Option<ExecutableAgentSnapshot> {
        self.publications
            .lock()
            .expect("published Agent catalog")
            .get(&(workspace.to_string(), fingerprint.to_string()))
            .cloned()
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

    pub(crate) fn declared_hand_for_agent(&self, agent_id: &str) -> Result<Option<String>, String> {
        let entries = self.entries.lock().expect("installed Agent catalog");
        let mut declarations = entries
            .iter()
            .filter(|((_, installed_id), _)| installed_id == agent_id)
            .filter_map(|(_, entry)| entry.declared_hand.clone())
            .collect::<std::collections::BTreeSet<_>>();
        match declarations.len() {
            0 => Ok(None),
            1 => Ok(declarations.pop_first()),
            _ => Err(format!(
                "Agent id `{agent_id}` has ambiguous Hand declarations across Workspaces"
            )),
        }
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
                    .then_some(&entry.snapshot.resolved_spec.plugin_config.agent)
                    .filter(|bindings| {
                        bindings
                            .skills
                            .iter()
                            .any(|skill| skill.skill_id == skill_id)
                    })
                    .map(|_| agent_id.clone())
            })
            .collect();
        agents.sort();
        agents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_moves_forward_while_exact_publications_remain_addressable() {
        let catalog = InstalledAgentCatalog::default();
        let first = ExecutableAgentSnapshot::builder("researcher")
            .fingerprint("fp-1")
            .build();
        let second = ExecutableAgentSnapshot::builder("researcher")
            .fingerprint("fp-2")
            .build();

        catalog.install("workspace", "researcher", 1, first.clone(), None);
        catalog.install("workspace", "researcher", 2, second.clone(), None);

        assert_eq!(catalog.snapshot_in("workspace", "researcher"), Some(second));
        assert_eq!(
            catalog.snapshot_by_fingerprint("workspace", "fp-1"),
            Some(first)
        );
    }

    #[test]
    fn declared_hand_lookup_is_current_and_rejects_cross_workspace_ambiguity() {
        // Cause/effect graph:
        // active installed publications -> logical Hand lookup by runtime Agent id;
        // an unambiguous declaration selects placement, while conflicting tenant
        // declarations fail closed because activation currently carries no workspace.
        //
        // Decision table:
        // | active declarations for Agent | result |
        // | none / only None | None |
        // | one distinct id, repeated or not | that id |
        // | two distinct ids | ambiguity error |
        // | conflict removed by uninstall | remaining id |
        let catalog = InstalledAgentCatalog::default();
        let snapshot = ExecutableAgentSnapshot::builder("researcher")
            .fingerprint("fp")
            .build();

        catalog.install("a", "researcher", 1, snapshot.clone(), None);
        assert_eq!(catalog.declared_hand_for_agent("researcher").unwrap(), None);

        catalog.install(
            "a",
            "researcher",
            2,
            snapshot.clone(),
            Some("hand-a".to_owned()),
        );
        catalog.install(
            "b",
            "researcher",
            1,
            snapshot.clone(),
            Some("hand-a".to_owned()),
        );
        assert_eq!(
            catalog.declared_hand_for_agent("researcher").unwrap(),
            Some("hand-a".to_owned())
        );

        catalog.install("c", "researcher", 1, snapshot, Some("hand-b".to_owned()));
        assert!(catalog.declared_hand_for_agent("researcher").is_err());

        catalog.uninstall("c", "researcher");
        assert_eq!(
            catalog.declared_hand_for_agent("researcher").unwrap(),
            Some("hand-a".to_owned())
        );
    }
}
