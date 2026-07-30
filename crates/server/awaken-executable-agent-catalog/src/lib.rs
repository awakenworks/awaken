//! Coordinator-owned executable Agent catalog and AllInOne local adapter.
//!
//! This crate owns only the rebuildable execution projection. Control's
//! `StoredPublication` remains authoritative and exact historical registrations
//! remain addressable after current availability changes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use awaken_executable_agent_contract::{
    ExecutableAgentRegistrar, ExecutableAgentRegistration, ExecutableAgentRegistrationError,
    ExecutableAgentRegistrationOutcome, ExecutableAgentWithdrawal,
    ExecutableAgentWithdrawalOutcome,
};
use awaken_resource_contract::{
    AgentResourceReferenceSource, InputResourceId, ResourceKind, ResourceTarget,
};
use awaken_runtime_contract::snapshot::AgentId;
use awaken_runtime_contract::{
    CatalogFingerprint, ExecutableAgentSnapshot, PublishedAgentSnapshotSource,
};
use awaken_session_contract::{AgentConfigSource, AgentConfigView};

mod http;
mod postgres;
mod schema;

pub use http::{
    EXECUTABLE_AGENT_REGISTER_PATH, EXECUTABLE_AGENT_WITHDRAW_PATH, HttpExecutableAgentRegistrar,
    executable_agent_registration_router,
};
pub use postgres::PostgresExecutableAgentRegistrar;
pub use schema::executable_agent_catalog_bundle;

type AgentKey = (String, String);
type RevisionKey = (String, String, u64);
type FingerprintKey = (String, String);

#[derive(Clone)]
struct CurrentEntry {
    lifecycle_revision: u64,
    registration: Option<ExecutableAgentRegistration>,
}

#[derive(Clone, Default)]
struct CatalogState {
    current: BTreeMap<AgentKey, CurrentEntry>,
    revisions: BTreeMap<RevisionKey, ExecutableAgentRegistration>,
    fingerprints: BTreeMap<FingerprintKey, ExecutableAgentSnapshot>,
}

/// Coordinator's current/exact/fingerprint read projection.
#[derive(Default)]
pub struct ExecutableAgentCatalog {
    state: RwLock<CatalogState>,
}

impl ExecutableAgentCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentRegistration> {
        self.state
            .read()
            .expect("executable Agent catalog")
            .current
            .get(&(workspace_id.to_owned(), agent_id.to_owned()))
            .and_then(|entry| entry.registration.clone())
    }

    pub fn at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<ExecutableAgentRegistration> {
        self.state
            .read()
            .expect("executable Agent catalog")
            .revisions
            .get(&(
                workspace_id.to_owned(),
                agent_id.to_owned(),
                source_revision,
            ))
            .cloned()
    }

    pub fn exact(&self, workspace_id: &str, fingerprint: &str) -> Option<ExecutableAgentSnapshot> {
        self.state
            .read()
            .expect("executable Agent catalog")
            .fingerprints
            .get(&(workspace_id.to_owned(), fingerprint.to_owned()))
            .cloned()
    }

    #[must_use]
    pub fn is_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.state
            .read()
            .expect("executable Agent catalog")
            .current
            .get(&(workspace_id.to_owned(), agent_id.to_owned()))
            .is_some_and(|entry| entry.registration.is_none())
    }

    pub fn declared_hand_for_agent(&self, agent_id: &str) -> Result<Option<String>, String> {
        let state = self.state.read().expect("executable Agent catalog");
        let mut declarations = state
            .current
            .iter()
            .filter(|((_, installed_id), entry)| {
                installed_id == agent_id && entry.registration.is_some()
            })
            .filter_map(|(_, entry)| entry.registration.as_ref()?.declared_hand.clone())
            .collect::<BTreeSet<_>>();
        match declarations.len() {
            0 => Ok(None),
            1 => Ok(declarations.pop_first()),
            _ => Err(format!(
                "Agent id `{agent_id}` has ambiguous Hand declarations across Workspaces"
            )),
        }
    }

    fn register_locked(
        state: &mut CatalogState,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
        registration.validate()?;
        let revision_key = (
            registration.workspace_id.clone(),
            registration.agent_id.clone(),
            registration.source_revision,
        );
        if let Some(existing) = state.revisions.get(&revision_key) {
            return if existing == &registration {
                Ok(ExecutableAgentRegistrationOutcome::AlreadyRegistered)
            } else {
                Err(ExecutableAgentRegistrationError::Conflict(format!(
                    "Workspace `{}` Agent `{}` revision {} already has fingerprint `{}`",
                    registration.workspace_id,
                    registration.agent_id,
                    registration.source_revision,
                    existing.snapshot.fingerprint.0
                )))
            };
        }
        let fingerprint_key = (
            registration.workspace_id.clone(),
            registration.snapshot.fingerprint.0.clone(),
        );
        if let Some(existing) = state.fingerprints.get(&fingerprint_key)
            && existing != &registration.snapshot
        {
            return Err(ExecutableAgentRegistrationError::Conflict(format!(
                "Workspace `{}` fingerprint `{}` names different snapshots",
                registration.workspace_id, registration.snapshot.fingerprint.0
            )));
        }
        state
            .fingerprints
            .insert(fingerprint_key, registration.snapshot.clone());
        state.revisions.insert(revision_key, registration.clone());

        let key = (
            registration.workspace_id.clone(),
            registration.agent_id.clone(),
        );
        let advances_current = state
            .current
            .get(&key)
            .is_none_or(|current| registration.source_revision > current.lifecycle_revision);
        if advances_current {
            state.current.insert(
                key,
                CurrentEntry {
                    lifecycle_revision: registration.source_revision,
                    registration: Some(registration),
                },
            );
            Ok(ExecutableAgentRegistrationOutcome::RegisteredCurrent)
        } else {
            Ok(ExecutableAgentRegistrationOutcome::RegisteredHistorical)
        }
    }

    fn preview_registration(
        &self,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
        let mut state = self.state.read().expect("executable Agent catalog").clone();
        Self::register_locked(&mut state, registration)
    }

    fn withdraw_locked(
        state: &mut CatalogState,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        withdrawal.validate()?;
        let key = (withdrawal.workspace_id, withdrawal.agent_id);
        match state.current.get_mut(&key) {
            Some(current) if withdrawal.lifecycle_revision < current.lifecycle_revision => {
                Ok(ExecutableAgentWithdrawalOutcome::HistoricalNoop)
            }
            Some(current)
                if withdrawal.lifecycle_revision == current.lifecycle_revision
                    && current.registration.is_none() =>
            {
                Ok(ExecutableAgentWithdrawalOutcome::AlreadyWithdrawn)
            }
            Some(current) => {
                current.lifecycle_revision = withdrawal.lifecycle_revision;
                current.registration = None;
                Ok(ExecutableAgentWithdrawalOutcome::WithdrawnCurrent)
            }
            None => {
                state.current.insert(
                    key,
                    CurrentEntry {
                        lifecycle_revision: withdrawal.lifecycle_revision,
                        registration: None,
                    },
                );
                Ok(ExecutableAgentWithdrawalOutcome::WithdrawnCurrent)
            }
        }
    }

    fn preview_withdrawal(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        let mut state = self.state.read().expect("executable Agent catalog").clone();
        Self::withdraw_locked(&mut state, withdrawal)
    }
}

/// Explicit AllInOne adapter over the same Coordinator catalog state machine
/// exposed by the authenticated HTTP registration router.
#[derive(Clone)]
pub struct LocalExecutableAgentRegistrar {
    catalog: Arc<ExecutableAgentCatalog>,
}

impl LocalExecutableAgentRegistrar {
    #[must_use]
    pub fn new(catalog: Arc<ExecutableAgentCatalog>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl ExecutableAgentRegistrar for LocalExecutableAgentRegistrar {
    async fn register(
        &self,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
        let mut state = self
            .catalog
            .state
            .write()
            .expect("executable Agent catalog");
        ExecutableAgentCatalog::register_locked(&mut state, registration)
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        let mut state = self
            .catalog
            .state
            .write()
            .expect("executable Agent catalog");
        ExecutableAgentCatalog::withdraw_locked(&mut state, withdrawal)
    }
}

impl PublishedAgentSnapshotSource for ExecutableAgentCatalog {
    fn current(&self, workspace: &str, agent_id: &AgentId) -> Option<ExecutableAgentSnapshot> {
        ExecutableAgentCatalog::current(self, workspace, &agent_id.0)
            .map(|registration| registration.snapshot)
    }

    fn exact(
        &self,
        workspace: &str,
        fingerprint: &CatalogFingerprint,
    ) -> Option<ExecutableAgentSnapshot> {
        ExecutableAgentCatalog::exact(self, workspace, &fingerprint.0)
    }

    fn at_revision(
        &self,
        workspace: &str,
        agent_id: &AgentId,
        source_revision: u64,
    ) -> Option<ExecutableAgentSnapshot> {
        ExecutableAgentCatalog::at_revision(self, workspace, &agent_id.0, source_revision)
            .map(|registration| registration.snapshot)
    }
}

impl AgentConfigSource for ExecutableAgentCatalog {
    fn agent_view_in(&self, workspace_id: &str, agent_id: &str) -> Option<AgentConfigView> {
        self.current(workspace_id, agent_id)
            .map(|registration| registration.agent_view)
    }

    fn agent_unavailable_in(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.is_unavailable(workspace_id, agent_id)
    }
}

impl AgentResourceReferenceSource for ExecutableAgentCatalog {
    fn agents_referencing(&self, target: &ResourceTarget) -> Vec<String> {
        let state = self.state.read().expect("executable Agent catalog");
        let mut agents = state
            .current
            .iter()
            .filter_map(|((workspace, agent_id), entry)| {
                let registration = entry.registration.as_ref()?;
                if workspace != &target.workspace_id {
                    return None;
                }
                let bound = match target.kind {
                    ResourceKind::Skill => registration
                        .agent_view
                        .skills
                        .iter()
                        .any(|skill| skill.skill_id == target.resource_id),
                    ResourceKind::File => registration.agent_view.resources.iter().any(|binding| {
                        matches!(&binding.target, InputResourceId::File(id) if id.as_str() == target.resource_id)
                    }),
                    ResourceKind::MemoryStore => registration
                        .agent_view
                        .resources
                        .iter()
                        .any(|binding| {
                            matches!(&binding.target, InputResourceId::MemoryStore(id) if id.as_str() == target.resource_id)
                        }),
                    ResourceKind::Repository => registration
                        .agent_view
                        .resources
                        .iter()
                        .any(|binding| {
                            matches!(&binding.target, InputResourceId::Repository(id) if id.as_str() == target.resource_id)
                        }),
                };
                bound.then(|| agent_id.clone())
            })
            .collect::<Vec<_>>();
        agents.sort();
        agents
    }
}

#[cfg(test)]
mod test_support {
    use awaken_executable_agent_contract::ExecutableAgentRegistration;
    use awaken_runtime_contract::snapshot::AgentId;
    use awaken_runtime_contract::{
        AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
        AgentSnapshotMetadata, ExecutableAgentSnapshot,
    };
    use awaken_session_contract::AgentConfigView;

    pub(crate) fn registration(revision: u64, fingerprint: &str) -> ExecutableAgentRegistration {
        let mut snapshot = ExecutableAgentSnapshot::builder("agent-a")
            .fingerprint(fingerprint)
            .build();
        snapshot.metadata = AgentSnapshotMetadata {
            source: AgentConfigRevisionRef {
                agent_id: AgentId("agent-a".into()),
                revision,
            },
            publication_version: AgentPublicationVersion(format!("v{revision}")),
            resolution: Default::default(),
            fingerprint: AgentSnapshotFingerprint(fingerprint.into()),
        };
        ExecutableAgentRegistration {
            workspace_id: "workspace-a".into(),
            agent_id: "agent-a".into(),
            source_revision: revision,
            snapshot,
            agent_view: AgentConfigView::default(),
            declared_hand: Some("hand-a".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::registration;

    #[tokio::test]
    async fn registration_is_idempotent_monotonic_and_conflict_checked() {
        // Cause/effect decision table:
        // R1 new rev1 -> current rev1; R2 identical retry -> AlreadyRegistered;
        // R3 same identity/different content -> Conflict/current unchanged;
        // R4 rev2 -> current rev2; R5 later arrival of distinct older exact
        // revision -> retained historically without moving current backwards.
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = LocalExecutableAgentRegistrar::new(catalog.clone());
        let rev1 = registration(1, "fp-1");
        assert_eq!(
            registrar.register(rev1.clone()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredCurrent,
            "R1"
        );
        assert_eq!(
            registrar.register(rev1.clone()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::AlreadyRegistered,
            "R2"
        );
        let mut conflict = rev1.clone();
        conflict.declared_hand = Some("other".into());
        assert!(
            matches!(
                registrar.register(conflict).await,
                Err(ExecutableAgentRegistrationError::Conflict(_))
            ),
            "R3"
        );
        let rev2 = registration(2, "fp-2");
        assert_eq!(
            registrar.register(rev2.clone()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredCurrent,
            "R4"
        );
        assert_eq!(catalog.current("workspace-a", "agent-a"), Some(rev2), "R4");
        assert_eq!(
            catalog.at_revision("workspace-a", "agent-a", 1),
            Some(rev1),
            "R5"
        );
    }

    #[tokio::test]
    async fn withdrawal_is_monotonic_and_preserves_exact_history() {
        // Cause/effect decision table:
        // W1 register rev2 -> available; W2 withdraw lifecycle rev3 -> current
        // unavailable but exact rev2/fingerprint retained; W3 identical tombstone
        // -> idempotent; W4 late rev1 registration/withdrawal -> cannot revive or
        // move the lifecycle pointer; W5 new rev4 publication -> available again.
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = LocalExecutableAgentRegistrar::new(catalog.clone());
        let rev2 = registration(2, "fp-2");
        registrar.register(rev2.clone()).await.unwrap();
        let withdrawal = ExecutableAgentWithdrawal {
            workspace_id: "workspace-a".into(),
            agent_id: "agent-a".into(),
            lifecycle_revision: 3,
        };
        assert_eq!(
            registrar.withdraw(withdrawal.clone()).await.unwrap(),
            ExecutableAgentWithdrawalOutcome::WithdrawnCurrent,
            "W2"
        );
        assert!(catalog.current("workspace-a", "agent-a").is_none(), "W2");
        assert!(catalog.is_unavailable("workspace-a", "agent-a"), "W2");
        assert_eq!(
            catalog.at_revision("workspace-a", "agent-a", 2),
            Some(rev2),
            "W2"
        );
        assert!(catalog.exact("workspace-a", "fp-2").is_some(), "W2");
        assert_eq!(
            registrar.withdraw(withdrawal).await.unwrap(),
            ExecutableAgentWithdrawalOutcome::AlreadyWithdrawn,
            "W3"
        );
        assert_eq!(
            registrar.register(registration(1, "fp-1")).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredHistorical,
            "W4"
        );
        assert!(catalog.current("workspace-a", "agent-a").is_none(), "W4");
        assert_eq!(
            registrar.register(registration(4, "fp-4")).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredCurrent,
            "W5"
        );
        assert_eq!(
            catalog
                .current("workspace-a", "agent-a")
                .unwrap()
                .source_revision,
            4,
            "W5"
        );
    }
}
