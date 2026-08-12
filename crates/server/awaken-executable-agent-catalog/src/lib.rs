//! Coordinator-owned executable Agent catalog and AllInOne local adapter.
//!
//! This crate owns only the rebuildable execution projection. Control's
//! `StoredPublication` remains authoritative and exact historical registrations
//! remain addressable after current availability changes.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use awaken_executable_agent_contract::{
    ExecutableAgentInventorySource, ExecutableAgentProfileSource, ExecutableAgentRegistrar,
    ExecutableAgentRegistration, ExecutableAgentRegistrationError,
    ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationSource,
    ExecutableAgentSessionProfile, ExecutableAgentWithdrawal, ExecutableAgentWithdrawalOutcome,
};
use awaken_resource_contract::{
    InputResourceId, ResourceKind, ResourcePurgeError, ResourceReference, ResourceReferenceIndex,
    ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
use awaken_runtime_contract::snapshot::AgentId;
use awaken_runtime_contract::{
    CatalogFingerprint, ExecutableAgentSnapshot, PublishedAgentSnapshotSource,
};

mod http;
mod postgres;
mod schema;

pub use http::{
    EXECUTABLE_AGENT_PUBLISH_PERMISSION, EXECUTABLE_AGENT_REGISTER_PATH,
    EXECUTABLE_AGENT_WITHDRAW_PATH, EXECUTABLE_AGENT_WITHDRAW_PERMISSION,
    HttpExecutableAgentRegistrar, executable_agent_registration_router,
    executable_agent_registration_router_with_authenticator,
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

#[async_trait]
impl ExecutableAgentRegistrationSource for ExecutableAgentCatalog {
    async fn current_registration(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<ExecutableAgentRegistration>, ExecutableAgentRegistrationError> {
        Ok(self.current(workspace_id, agent_id))
    }

    async fn registration_at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Result<Option<ExecutableAgentRegistration>, ExecutableAgentRegistrationError> {
        Ok(self.at_revision(workspace_id, agent_id, source_revision))
    }
}

#[async_trait]
impl ExecutableAgentInventorySource for ExecutableAgentCatalog {
    async fn current_registrations(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ExecutableAgentRegistration>, ExecutableAgentRegistrationError> {
        let state = self.state.read().expect("executable Agent catalog");
        let mut registrations = state
            .current
            .iter()
            .filter_map(|((workspace, _), entry)| {
                (workspace == workspace_id)
                    .then(|| entry.registration.clone())
                    .flatten()
            })
            .collect::<Vec<_>>();
        registrations.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
        Ok(registrations)
    }
}

/// Explicit AllInOne adapter over the same Coordinator catalog state machine
/// exposed by the authenticated HTTP registration router.
#[derive(Clone)]
pub struct LocalExecutableAgentRegistrar {
    catalog: Arc<ExecutableAgentCatalog>,
}

/// Projects current Agent bindings into the atomic Resources reference index.
/// Adds happen before executable exposure; removals happen after withdrawal, so
/// every crash window is conservative (leak-safe) rather than use-after-purge.
pub struct ReferenceIndexedExecutableAgentRegistrar {
    catalog: Arc<ExecutableAgentCatalog>,
    delegate: Arc<dyn ExecutableAgentRegistrar>,
    references: Arc<dyn ResourceReferenceIndex>,
    mutation: tokio::sync::Mutex<()>,
}

impl ReferenceIndexedExecutableAgentRegistrar {
    #[must_use]
    pub fn new(
        catalog: Arc<ExecutableAgentCatalog>,
        delegate: Arc<dyn ExecutableAgentRegistrar>,
        references: Arc<dyn ResourceReferenceIndex>,
    ) -> Self {
        Self {
            catalog,
            delegate,
            references,
            mutation: tokio::sync::Mutex::new(()),
        }
    }

    /// Rebuild durable reference rows after the executable command log has
    /// rehydrated the in-memory catalog. This uses the same holder replacement
    /// operation as live registration; retries are idempotent.
    pub async fn synchronize_current_references(
        &self,
    ) -> Result<(), ExecutableAgentRegistrationError> {
        let _mutation = self.mutation.lock().await;
        let current = {
            let state = self.catalog.state.read().expect("executable Agent catalog");
            state
                .current
                .iter()
                .map(|((workspace_id, agent_id), entry)| {
                    (
                        workspace_id.clone(),
                        agent_id.clone(),
                        entry.registration.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        for (workspace_id, agent_id, registration) in current {
            self.references
                .replace_references(
                    ResourceReferenceKind::AgentBinding,
                    &agent_holder(&workspace_id, &agent_id),
                    registration
                        .as_ref()
                        .map_or_else(Vec::new, agent_reference_records),
                )
                .await
                .map_err(registration_storage)?;
        }
        Ok(())
    }
}

fn agent_holder(workspace_id: &str, agent_id: &str) -> String {
    format!("{workspace_id}:{agent_id}")
}

fn agent_reference_records(
    registration: &ExecutableAgentRegistration,
) -> Vec<ResourceReferenceRecord> {
    let reference_id = agent_holder(&registration.workspace_id, &registration.agent_id);
    let reference = || ResourceReference {
        kind: ResourceReferenceKind::AgentBinding,
        reference_id: reference_id.clone(),
    };
    let mut records = registration
        .session_profile
        .resources
        .iter()
        .map(|binding| {
            let (kind, id) = match &binding.target {
                InputResourceId::File(id) => (ResourceKind::File, id.as_str()),
                InputResourceId::MemoryStore(id) => (ResourceKind::MemoryStore, id.as_str()),
                InputResourceId::Repository(id) => (ResourceKind::Repository, id.as_str()),
            };
            ResourceReferenceRecord {
                target: ResourceTarget::new(&registration.workspace_id, kind, id),
                reference: reference(),
            }
        })
        .collect::<Vec<_>>();
    records.extend(
        registration
            .session_profile
            .skills
            .iter()
            .filter(|skill| skill.kind == awaken_agent_contract::AgentSkillKind::Custom)
            .map(|skill| ResourceReferenceRecord {
                target: ResourceTarget::new(
                    &registration.workspace_id,
                    ResourceKind::Skill,
                    &skill.skill_id,
                ),
                reference: reference(),
            }),
    );
    records.sort();
    records.dedup();
    records
}

fn registration_storage(error: ResourcePurgeError) -> ExecutableAgentRegistrationError {
    ExecutableAgentRegistrationError::Storage(error.to_string())
}

#[async_trait]
impl ExecutableAgentRegistrar for ReferenceIndexedExecutableAgentRegistrar {
    async fn register(
        &self,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
        let _mutation = self.mutation.lock().await;
        let preview = self.catalog.preview_registration(registration.clone())?;
        let is_current_replay = self
            .catalog
            .current(&registration.workspace_id, &registration.agent_id)
            .as_ref()
            == Some(&registration);
        let holder = agent_holder(&registration.workspace_id, &registration.agent_id);
        let records = agent_reference_records(&registration);
        if preview == ExecutableAgentRegistrationOutcome::RegisteredCurrent {
            // Retain the old current publication's rows while adding every new
            // edge. Only after the delegate exposes the replacement may the
            // exact replace remove obsolete rows. Every intermediate state is
            // conservative, including a partial add or failed delegate.
            for record in &records {
                self.references
                    .add_reference(record.clone())
                    .await
                    .map_err(registration_storage)?;
            }
            let outcome = self.delegate.register(registration).await?;
            if outcome == ExecutableAgentRegistrationOutcome::RegisteredCurrent {
                self.references
                    .replace_references(ResourceReferenceKind::AgentBinding, &holder, records)
                    .await
                    .map_err(registration_storage)?;
            }
            return Ok(outcome);
        }
        if is_current_replay {
            self.references
                .replace_references(ResourceReferenceKind::AgentBinding, &holder, records)
                .await
                .map_err(registration_storage)?;
        }
        self.delegate.register(registration).await
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        let _mutation = self.mutation.lock().await;
        let workspace_id = withdrawal.workspace_id.clone();
        let agent_id = withdrawal.agent_id.clone();
        let outcome = self.delegate.withdraw(withdrawal).await?;
        if outcome == ExecutableAgentWithdrawalOutcome::WithdrawnCurrent {
            self.references
                .replace_references(
                    ResourceReferenceKind::AgentBinding,
                    &agent_holder(&workspace_id, &agent_id),
                    Vec::new(),
                )
                .await
                .map_err(registration_storage)?;
        }
        Ok(outcome)
    }
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

impl ExecutableAgentProfileSource for ExecutableAgentCatalog {
    fn session_profile_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.current(workspace_id, agent_id)
            .map(|registration| registration.session_profile)
    }

    fn agent_unavailable_in(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.is_unavailable(workspace_id, agent_id)
    }
}

#[cfg(test)]
mod test_support {
    use awaken_executable_agent_contract::ExecutableAgentRegistration;
    use awaken_executable_agent_contract::ExecutableAgentSessionProfile;
    use awaken_runtime_contract::snapshot::AgentId;
    use awaken_runtime_contract::{
        AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
        AgentSnapshotMetadata, ExecutableAgentSnapshot,
    };

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
            session_profile: ExecutableAgentSessionProfile::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::registration;

    #[tokio::test]
    async fn agent_reference_projection_is_fenced_monotonic_and_conservative() {
        use awaken_resource_contract::{
            BindingId, FileId, InputBinding, InputResourceId, ResourceAccess,
            ResourceReclamationFence, ResourceReferenceIndex,
        };

        // FMECA cause/effect graph:
        // C1 a new current publication binds File/custom Skill; C2 an older
        // historical publication arrives late; C3 current is withdrawn; C4 a
        // reclamation fence already owns a target; C5 restart rehydrates the
        // catalog before the decorator exists. Effects: E1 references exist
        // before executable exposure; E2 late history cannot replace current
        // edges; E3 withdrawal removes edges only after exposure ends; E4 a
        // fenced target rejects publication and remains unavailable. Failure
        // mode "scan/delete TOCTOU" has S=10,O=4,D=9,RPN=360. Mitigation is the
        // ResourceReferenceIndex transaction, with stale rows the only permitted
        // crash residue. Decision rules: R1=C1->E1; R2=C1+C2->E2;
        // E5 startup synchronization rebuilds active rows. Rules:
        // R3=C1+C3->E3; R4=C4+C1->E4; R5=C5->E5.
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let store = Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
        let delegate = Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone()));
        let registrar =
            ReferenceIndexedExecutableAgentRegistrar::new(catalog.clone(), delegate, store.clone());
        let mut current = registration(2, "fp-2");
        current.session_profile.resources.push(InputBinding {
            binding_id: BindingId::from("file-binding"),
            target: InputResourceId::File(FileId::from("file-current")),
            mount_path: "inputs/current".into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        });
        current
            .session_profile
            .skills
            .push(awaken_agent_contract::AgentSkillBinding::custom(
                "skill-current",
            ));
        registrar.register(current.clone()).await.unwrap();
        for target in [
            ResourceTarget::new("workspace-a", ResourceKind::File, "file-current"),
            ResourceTarget::new("workspace-a", ResourceKind::Skill, "skill-current"),
        ] {
            assert_eq!(store.references(&target).await.unwrap().len(), 1, "R1");
        }

        let mut replacement = registration(3, "fp-3");
        replacement.session_profile.resources.push(InputBinding {
            binding_id: BindingId::from("replacement"),
            target: InputResourceId::File(FileId::from("file-replacement")),
            mount_path: "inputs/replacement".into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        });
        registrar.register(replacement).await.unwrap();
        assert!(
            store
                .references(&ResourceTarget::new(
                    "workspace-a",
                    ResourceKind::File,
                    "file-current",
                ))
                .await
                .unwrap()
                .is_empty(),
            "R1 replacement removes old rows only after exposure"
        );

        let mut historical = registration(1, "fp-1");
        historical.session_profile.resources.push(InputBinding {
            binding_id: BindingId::from("old"),
            target: InputResourceId::File(FileId::from("file-old")),
            mount_path: "inputs/old".into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        });
        assert_eq!(
            registrar.register(historical).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredHistorical,
            "R2"
        );
        assert_eq!(
            store
                .references(&ResourceTarget::new(
                    "workspace-a",
                    ResourceKind::File,
                    "file-replacement",
                ))
                .await
                .unwrap()
                .len(),
            1,
            "R2"
        );

        registrar
            .withdraw(ExecutableAgentWithdrawal {
                workspace_id: "workspace-a".into(),
                agent_id: "agent-a".into(),
                lifecycle_revision: 4,
            })
            .await
            .unwrap();
        assert!(
            store
                .references(&ResourceTarget::new(
                    "workspace-a",
                    ResourceKind::File,
                    "file-replacement",
                ))
                .await
                .unwrap()
                .is_empty(),
            "R3"
        );

        let fenced = ResourceTarget::new("workspace-a", ResourceKind::File, "file-fenced");
        assert!(matches!(
            store.acquire_reclamation("purge-1", &fenced).await.unwrap(),
            awaken_resource_contract::AcquireResourceReclamationOutcome::Acquired
        ));
        let mut blocked = registration(5, "fp-5");
        blocked.session_profile.resources.push(InputBinding {
            binding_id: BindingId::from("fenced"),
            target: InputResourceId::File(FileId::from("file-fenced")),
            mount_path: "inputs/fenced".into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        });
        assert!(
            matches!(
                registrar.register(blocked).await,
                Err(ExecutableAgentRegistrationError::Storage(_))
            ),
            "R4"
        );
        assert!(catalog.current("workspace-a", "agent-a").is_none(), "R4");

        let restored_catalog = Arc::new(ExecutableAgentCatalog::new());
        LocalExecutableAgentRegistrar::new(restored_catalog.clone())
            .register(current)
            .await
            .unwrap();
        let restored_store =
            Arc::new(awaken_resource_store::SqliteResourceStore::in_memory().unwrap());
        let restored = ReferenceIndexedExecutableAgentRegistrar::new(
            restored_catalog.clone(),
            Arc::new(LocalExecutableAgentRegistrar::new(restored_catalog)),
            restored_store.clone(),
        );
        restored.synchronize_current_references().await.unwrap();
        assert_eq!(
            restored_store
                .references(&ResourceTarget::new(
                    "workspace-a",
                    ResourceKind::File,
                    "file-current",
                ))
                .await
                .unwrap()
                .len(),
            1,
            "R5"
        );
    }

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
        conflict.session_profile.system = Some("conflicting projection".into());
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
