// @formal entrypoint agent_profile.requested_revision_never_follows_mutable_current
// @formal entrypoint config_activation.installed_generation_never_rolls_back
// @formal entrypoint config_activation.stale_publication_never_installs
//! Coordinator-owned executable Agent catalog and AllInOne local adapter.
//!
//! This crate owns only the rebuildable execution projection. Control's
//! `StoredPublication` remains authoritative and exact historical registrations
//! remain addressable after current availability changes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock, RwLockReadGuard, RwLockWriteGuard};

use async_trait::async_trait;
use awaken_executable_agent_contract::{
    CanonicalPublishedModelReferenceProjector, ExecutableAgentInventorySource,
    ExecutableAgentProfileSource, ExecutableAgentRegistrar, ExecutableAgentRegistration,
    ExecutableAgentRegistrationError, ExecutableAgentRegistrationOutcome,
    ExecutableAgentRegistrationSource, ExecutableAgentSessionProfile, ExecutableAgentWithdrawal,
    ExecutableAgentWithdrawalOutcome, PublishedModelReference, PublishedModelReferenceProjector,
    project_current_model_publications,
};
use awaken_resource_contract::{
    InputResourceId, ResourceKind, ResourcePurgeError, ResourceReference, ResourceReferenceIndex,
    ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
use awaken_runtime_contract::snapshot::AgentId;
use awaken_runtime_contract::{
    CatalogFingerprint, ExecutableAgentSnapshot, PublishedAgentSnapshotSource,
};
use awaken_sync::InvariantRwLock;

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
    /// Replay provenance for legacy registrations amended by a later command.
    time_amendments: BTreeSet<RevisionKey>,
}

/// An upgrade of one historical registration whose original command lacked
/// the publication row's timestamp. It is appended to the same command log;
/// the original immutable command and its sequence remain unchanged.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PublicationTimeEvidence {
    workspace_id: String,
    agent_id: String,
    source_revision: u64,
    fingerprint: String,
    publication_sequence: u64,
    published_at_unix_ms: u64,
    publication_order_verified: bool,
}

/// One owner for deciding whether a registration or withdrawal may replace the
/// current executable projection. Exact history admission remains separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentLifecycleDisposition {
    Advance,
    Retain,
    AlreadyUnavailable,
}

const fn current_lifecycle_disposition(
    current: Option<(u64, bool)>,
    incoming_revision: u64,
    incoming_available: bool,
) -> CurrentLifecycleDisposition {
    match current {
        None => CurrentLifecycleDisposition::Advance,
        Some((current_revision, _)) if incoming_revision < current_revision => {
            CurrentLifecycleDisposition::Retain
        }
        Some((current_revision, current_available))
            if incoming_revision == current_revision && !incoming_available =>
        {
            if current_available {
                CurrentLifecycleDisposition::Advance
            } else {
                CurrentLifecycleDisposition::AlreadyUnavailable
            }
        }
        Some((current_revision, _)) if incoming_revision == current_revision => {
            CurrentLifecycleDisposition::Retain
        }
        Some(_) => CurrentLifecycleDisposition::Advance,
    }
}

#[cfg(any(test, kani))]
fn check_current_lifecycle_disposition(
    current_revision: Option<u64>,
    current_available: bool,
    incoming_revision: u64,
    incoming_available: bool,
) {
    // Causes: C1 current is absent/present; C2 present current is available or
    // tombstoned; C3 incoming revision is lower/equal/higher; C4 command makes
    // the incoming state available (register) or unavailable (withdraw).
    // Effects: R1 absent or higher -> Advance to the incoming revision; R2 lower
    // -> Retain; R3 equal register -> Retain, including an equal tombstone; R4
    // equal withdrawal from available -> Advance to a tombstone; R5 equal
    // tombstone withdrawal -> AlreadyUnavailable. Every outcome preserves or
    // raises the installed lifecycle revision.
    let current = current_revision.map(|revision| (revision, current_available));
    let disposition = current_lifecycle_disposition(current, incoming_revision, incoming_available);
    match (current, disposition) {
        (None, CurrentLifecycleDisposition::Advance) => {}
        (None, _) => panic!("R1 absent current must advance"),
        (Some((old, _)), CurrentLifecycleDisposition::Advance) => {
            assert!(incoming_revision >= old);
        }
        (Some(_), CurrentLifecycleDisposition::Retain)
        | (Some(_), CurrentLifecycleDisposition::AlreadyUnavailable) => {}
    }
    if let Some((old, available)) = current {
        if incoming_revision < old {
            assert_eq!(disposition, CurrentLifecycleDisposition::Retain);
        } else if incoming_revision == old {
            assert_eq!(
                disposition,
                match (incoming_available, available) {
                    (true, _) => CurrentLifecycleDisposition::Retain,
                    (false, true) => CurrentLifecycleDisposition::Advance,
                    (false, false) => CurrentLifecycleDisposition::AlreadyUnavailable,
                }
            );
        } else {
            assert_eq!(disposition, CurrentLifecycleDisposition::Advance);
        }
    }
}

#[cfg(kani)]
/// @obligation config_activation.installed_generation_never_rolls_back
#[kani::proof]
fn installed_lifecycle_revision_never_rolls_back() {
    // No assumptions or stubs: exhaust the complete Option<u64> current domain,
    // both availability states, every u64 incoming revision and both commands.
    check_current_lifecycle_disposition(kani::any(), kani::any(), kani::any(), kani::any());
}

/// Coordinator's current/exact/fingerprint read projection.
#[derive(Default)]
pub struct ExecutableAgentCatalog {
    state: InvariantRwLock<CatalogState>,
    model_reference_projector: OnceLock<Arc<dyn PublishedModelReferenceProjector>>,
}

impl ExecutableAgentCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the composition's pure source projector before its first model
    /// read. A duplicate or late installation cannot change discovery policy.
    pub fn install_model_reference_projector(
        &self,
        projector: Arc<dyn PublishedModelReferenceProjector>,
    ) -> Result<(), ExecutableAgentRegistrationError> {
        self.model_reference_projector.set(projector).map_err(|_| {
            ExecutableAgentRegistrationError::Invalid(
                "model reference projector is already installed or in use".into(),
            )
        })
    }

    /// Fail-closed: a poisoned catalog admits no current registration.
    pub fn current(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentRegistration> {
        self.try_current(workspace_id, agent_id).ok().flatten()
    }

    /// Fail-closed: a poisoned catalog resolves no revision.
    pub fn at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<ExecutableAgentRegistration> {
        self.try_at_revision(workspace_id, agent_id, source_revision)
            .ok()
            .flatten()
    }

    /// Fail-closed: a poisoned catalog resolves no fingerprint.
    pub fn exact(&self, workspace_id: &str, fingerprint: &str) -> Option<ExecutableAgentSnapshot> {
        let state = self.read_state().ok()?;
        state
            .fingerprints
            .get(&(workspace_id.to_owned(), fingerprint.to_owned()))
            .cloned()
    }

    /// Fail-closed: a poisoned catalog reports every Agent unavailable.
    #[must_use]
    pub fn is_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        let Ok(state) = self.read_state() else {
            return true;
        };
        state
            .current
            .get(&(workspace_id.to_owned(), agent_id.to_owned()))
            .is_some_and(|entry| entry.registration.is_none())
    }

    fn read_state(
        &self,
    ) -> Result<RwLockReadGuard<'_, CatalogState>, ExecutableAgentRegistrationError> {
        self.state.read().map_err(catalog_poisoned)
    }

    fn write_state(
        &self,
    ) -> Result<RwLockWriteGuard<'_, CatalogState>, ExecutableAgentRegistrationError> {
        self.state.write().map_err(catalog_poisoned)
    }

    fn try_current(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<ExecutableAgentRegistration>, ExecutableAgentRegistrationError> {
        Ok(self
            .read_state()?
            .current
            .get(&(workspace_id.to_owned(), agent_id.to_owned()))
            .and_then(|entry| entry.registration.clone()))
    }

    fn try_at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Result<Option<ExecutableAgentRegistration>, ExecutableAgentRegistrationError> {
        Ok(self
            .read_state()?
            .revisions
            .get(&(
                workspace_id.to_owned(),
                agent_id.to_owned(),
                source_revision,
            ))
            .cloned())
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
            if existing == &registration {
                return Ok(ExecutableAgentRegistrationOutcome::AlreadyRegistered);
            }
            let mut enriched = existing.clone();
            enriched.published_at_unix_ms = registration.published_at_unix_ms;
            enriched.publication_sequence = registration.publication_sequence;
            enriched.publication_order_verified = registration.publication_order_verified;
            if existing.published_at_unix_ms.is_none()
                && existing.publication_sequence.is_none()
                && enriched == registration
                && let (Some(sequence), Some(time)) = (
                    registration.publication_sequence,
                    registration.published_at_unix_ms,
                )
            {
                Self::record_publication_time_locked(
                    state,
                    PublicationTimeEvidence {
                        workspace_id: registration.workspace_id,
                        agent_id: registration.agent_id,
                        source_revision: registration.source_revision,
                        fingerprint: registration.snapshot.fingerprint.0,
                        publication_sequence: sequence,
                        published_at_unix_ms: time,
                        publication_order_verified: registration.publication_order_verified,
                    },
                )?;
                return Ok(ExecutableAgentRegistrationOutcome::AlreadyRegistered);
            }
            return Err(ExecutableAgentRegistrationError::Conflict(format!(
                "Workspace `{}` Agent `{}` revision {} already has fingerprint `{}`",
                registration.workspace_id,
                registration.agent_id,
                registration.source_revision,
                existing.snapshot.fingerprint.0
            )));
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
        let current = state
            .current
            .get(&key)
            .map(|current| (current.lifecycle_revision, current.registration.is_some()));
        match current_lifecycle_disposition(current, registration.source_revision, true) {
            CurrentLifecycleDisposition::Advance => {
                state.current.insert(
                    key,
                    CurrentEntry {
                        lifecycle_revision: registration.source_revision,
                        registration: Some(registration),
                    },
                );
                Ok(ExecutableAgentRegistrationOutcome::RegisteredCurrent)
            }
            CurrentLifecycleDisposition::Retain => {
                Ok(ExecutableAgentRegistrationOutcome::RegisteredHistorical)
            }
            CurrentLifecycleDisposition::AlreadyUnavailable => {
                unreachable!("an available registration cannot be already unavailable")
            }
        }
    }

    fn record_publication_time_locked(
        state: &mut CatalogState,
        evidence: PublicationTimeEvidence,
    ) -> Result<(), ExecutableAgentRegistrationError> {
        let key = (
            evidence.workspace_id.clone(),
            evidence.agent_id.clone(),
            evidence.source_revision,
        );
        let registration = state.revisions.get_mut(&key).ok_or_else(|| {
            ExecutableAgentRegistrationError::Conflict(
                "publication time names an absent registration".into(),
            )
        })?;
        if evidence.publication_sequence == 0
            || registration.snapshot.fingerprint.0 != evidence.fingerprint
            || registration
                .published_at_unix_ms
                .is_some_and(|known| known != evidence.published_at_unix_ms)
            || registration
                .publication_sequence
                .is_some_and(|known| known != evidence.publication_sequence)
            || (registration.publication_order_verified && !evidence.publication_order_verified)
        {
            return Err(ExecutableAgentRegistrationError::Conflict(
                "publication time conflicts with the exact registration".into(),
            ));
        }
        registration.published_at_unix_ms = Some(evidence.published_at_unix_ms);
        registration.publication_sequence = Some(evidence.publication_sequence);
        registration.publication_order_verified = evidence.publication_order_verified;
        state.time_amendments.insert(key);
        if let Some(current) = state
            .current
            .get_mut(&(evidence.workspace_id, evidence.agent_id))
            && let Some(registered) = current.registration.as_mut()
            && registered.source_revision == evidence.source_revision
        {
            registered.published_at_unix_ms = Some(evidence.published_at_unix_ms);
            registered.publication_sequence = Some(evidence.publication_sequence);
            registered.publication_order_verified = evidence.publication_order_verified;
        }
        Ok(())
    }

    fn record_publication_time(
        &self,
        evidence: PublicationTimeEvidence,
    ) -> Result<(), ExecutableAgentRegistrationError> {
        let mut state = self.write_state()?;
        Self::record_publication_time_locked(&mut state, evidence)
    }

    fn has_time_amendment(
        &self,
        workspace: &str,
        agent: &str,
        revision: u64,
    ) -> Result<bool, ExecutableAgentRegistrationError> {
        Ok(self.read_state()?.time_amendments.contains(&(
            workspace.to_owned(),
            agent.to_owned(),
            revision,
        )))
    }

    fn preview_registration(
        &self,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
        let mut state = self.read_state()?.clone();
        Self::register_locked(&mut state, registration)
    }

    fn withdraw_locked(
        state: &mut CatalogState,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        withdrawal.validate()?;
        let key = (withdrawal.workspace_id, withdrawal.agent_id);
        let current = state
            .current
            .get(&key)
            .map(|current| (current.lifecycle_revision, current.registration.is_some()));
        match current_lifecycle_disposition(current, withdrawal.lifecycle_revision, false) {
            CurrentLifecycleDisposition::Advance => {
                state.current.insert(
                    key,
                    CurrentEntry {
                        lifecycle_revision: withdrawal.lifecycle_revision,
                        registration: None,
                    },
                );
                Ok(ExecutableAgentWithdrawalOutcome::WithdrawnCurrent)
            }
            CurrentLifecycleDisposition::Retain => {
                Ok(ExecutableAgentWithdrawalOutcome::HistoricalNoop)
            }
            CurrentLifecycleDisposition::AlreadyUnavailable => {
                Ok(ExecutableAgentWithdrawalOutcome::AlreadyWithdrawn)
            }
        }
    }

    fn preview_withdrawal(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        let mut state = self.read_state()?.clone();
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
        self.try_current(workspace_id, agent_id)
    }

    async fn registration_at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Result<Option<ExecutableAgentRegistration>, ExecutableAgentRegistrationError> {
        self.try_at_revision(workspace_id, agent_id, source_revision)
    }
}

#[async_trait]
impl ExecutableAgentInventorySource for ExecutableAgentCatalog {
    async fn current_registrations(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ExecutableAgentRegistration>, ExecutableAgentRegistrationError> {
        let state = self.read_state()?;
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

    async fn current_model_publications(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<PublishedModelReference>, ExecutableAgentRegistrationError> {
        let state = self.read_state()?;
        let projector = self
            .model_reference_projector
            .get_or_init(|| Arc::new(CanonicalPublishedModelReferenceProjector));
        project_current_model_publications(
            state.current.iter().filter_map(|((workspace, _), entry)| {
                (workspace == workspace_id)
                    .then_some(entry.registration.as_ref())
                    .flatten()
            }),
            state
                .revisions
                .iter()
                .filter_map(|((workspace, _, _), registration)| {
                    (workspace == workspace_id).then_some(registration)
                }),
            projector.as_ref(),
        )
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
            let state = self.catalog.read_state()?;
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

fn catalog_poisoned(_: awaken_sync::LockPoisoned) -> ExecutableAgentRegistrationError {
    ExecutableAgentRegistrationError::Storage("executable Agent catalog lock poisoned".into())
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
        let mut state = self.catalog.write_state()?;
        ExecutableAgentCatalog::register_locked(&mut state, registration)
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        let mut state = self.catalog.write_state()?;
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

    fn session_profile_at_revision_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.at_revision(workspace_id, agent_id, source_revision)
            .map(|registration| registration.session_profile)
    }

    fn executable_snapshot_at_revision_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<ExecutableAgentSnapshot> {
        self.at_revision(workspace_id, agent_id, source_revision)
            .map(|registration| registration.snapshot)
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
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "test", "model", "native",
            ))
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
            published_at_unix_ms: None,
            publication_sequence: None,
            publication_order_verified: false,
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
    async fn source_projector_is_immutable_and_shared_by_directory_and_readiness() {
        // C1 source adapter is installed before the first model read; C2 it is
        // installed twice; C3 either directory or Dream readiness reads it;
        // C4 a default read happens before installation. Rules: R1 C1+C3 =>
        // the same source-owned selection via the sole codec; R2 C2 or C4 =>
        // refuse policy replacement, including an initially empty directory.
        struct Source;
        impl PublishedModelReferenceProjector for Source {
            fn project(
                &self,
                _: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
            ) -> Result<
                Option<awaken_executable_agent_contract::ModelSelection>,
                awaken_executable_agent_contract::ManagedModelIdError,
            > {
                awaken_executable_agent_contract::parse_managed_model_id(
                    "source-model;provider=vendor",
                )
                .map(Some)
            }
        }
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        catalog
            .install_model_reference_projector(Arc::new(Source))
            .unwrap();
        assert!(
            catalog
                .install_model_reference_projector(Arc::new(Source))
                .is_err(),
            "R2 duplicate"
        );
        LocalExecutableAgentRegistrar::new(catalog.clone())
            .register(registration(1, "fp-source"))
            .await
            .unwrap();
        let directory = catalog
            .current_model_publications("workspace-a")
            .await
            .unwrap();
        let readiness = awaken_executable_agent_contract::current_model_references(
            catalog.as_ref(),
            "workspace-a",
        )
        .await
        .unwrap();
        assert_eq!(readiness, vec!["source-model;provider=vendor"], "R1");
        assert_eq!(directory[0].model_reference, readiness[0], "R1");
        let late = ExecutableAgentCatalog::new();
        assert!(
            late.current_model_publications("workspace-a")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            late.install_model_reference_projector(Arc::new(Source))
                .is_err(),
            "R2 late"
        );
    }

    #[tokio::test]
    async fn model_first_publication_survives_withdrawal_and_legacy_enrichment() {
        // Causes: C1 a dated first registration, C2 withdrawal then a later
        // publication of the same model whose clock moved backwards, C3 an old command without a time,
        // C4 exact durable-time enrichment, C5 another Workspace, C6 recovery
        // registers revision 2 before historical revision 1. Effects:
        // E1 retain the earliest time after withdrawal/reintroduction;
        // E2 unknown history makes first time unknown until exact evidence;
        // E3 enrichment restores E1; E4 Workspaces remain separate; E5
        // recovery arrival order cannot change the first commit.
        // Rules C1+C2=>E1, C1+C2+C3=>E2, C1+C2+C3+C4=>E3, C5=>E4, C1+C2+C6=>E5.
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = LocalExecutableAgentRegistrar::new(catalog.clone());
        let mut first = registration(1, "fp-1");
        first.publication_sequence = Some(1);
        first.published_at_unix_ms = Some(2_000);
        first.publication_order_verified = true;
        registrar.register(first).await.unwrap();
        registrar
            .withdraw(ExecutableAgentWithdrawal {
                workspace_id: "workspace-a".into(),
                agent_id: "agent-a".into(),
                lifecycle_revision: 1,
            })
            .await
            .unwrap();
        let mut later = registration(2, "fp-2");
        later.publication_sequence = Some(2);
        later.published_at_unix_ms = Some(1_000);
        later.publication_order_verified = true;
        registrar.register(later).await.unwrap();
        assert_eq!(
            catalog
                .current_model_publications("workspace-a")
                .await
                .unwrap()[0]
                .first_published_at_unix_ms,
            Some(2_000),
            "E1"
        );
        let legacy = registration(3, "fp-3");
        registrar.register(legacy.clone()).await.unwrap();
        assert_eq!(
            catalog
                .current_model_publications("workspace-a")
                .await
                .unwrap()[0]
                .first_published_at_unix_ms,
            None,
            "E2"
        );
        let mut enriched = legacy;
        enriched.publication_sequence = Some(3);
        enriched.published_at_unix_ms = Some(3_000);
        enriched.publication_order_verified = true;
        assert_eq!(
            registrar.register(enriched).await.unwrap(),
            ExecutableAgentRegistrationOutcome::AlreadyRegistered,
            "E3"
        );
        assert_eq!(
            catalog
                .current_model_publications("workspace-a")
                .await
                .unwrap()[0]
                .first_published_at_unix_ms,
            Some(2_000),
            "E3"
        );
        let mut other = registration(1, "fp-other");
        other.workspace_id = "workspace-b".into();
        other.publication_sequence = Some(4);
        other.published_at_unix_ms = Some(4_000);
        other.publication_order_verified = true;
        registrar.register(other).await.unwrap();
        assert_eq!(
            catalog
                .current_model_publications("workspace-b")
                .await
                .unwrap()[0]
                .first_published_at_unix_ms,
            Some(4_000),
            "E4"
        );
        let recovered = Arc::new(ExecutableAgentCatalog::new());
        let recovery = LocalExecutableAgentRegistrar::new(recovered.clone());
        let mut newer = registration(2, "fp-2");
        newer.publication_sequence = Some(2);
        newer.published_at_unix_ms = Some(1_000);
        newer.publication_order_verified = true;
        recovery.register(newer).await.unwrap();
        let mut older = registration(1, "fp-1");
        older.publication_sequence = Some(1);
        older.published_at_unix_ms = Some(2_000);
        older.publication_order_verified = true;
        recovery.register(older).await.unwrap();
        assert_eq!(
            recovered
                .current("workspace-a", "agent-a")
                .unwrap()
                .source_revision,
            2,
            "E5 current stays newest"
        );
        assert_eq!(
            recovered
                .current_model_publications("workspace-a")
                .await
                .unwrap()[0]
                .first_published_at_unix_ms,
            Some(2_000),
            "E5 first time follows oldest sequence"
        );
    }

    #[tokio::test]
    async fn historical_postgres_order_is_unknown_after_two_unfenced_rows() {
        // Causes: H1 one old registration has a row time but predates the
        // Workspace commit-order lock; H2 a second old row of the same model
        // has a backwards clock. Effects: E1 one row is unambiguous; E2 two
        // rows make first-commit time unknown, even though seq is ordered.
        // Rules H1=>E1, H1+H2=>E2.
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = LocalExecutableAgentRegistrar::new(catalog.clone());
        let mut first = registration(1, "fp-old-1");
        first.publication_sequence = Some(1);
        first.published_at_unix_ms = Some(2_000);
        registrar.register(first).await.unwrap();
        assert_eq!(
            catalog
                .current_model_publications("workspace-a")
                .await
                .unwrap()[0]
                .first_published_at_unix_ms,
            Some(2_000),
            "E1"
        );
        let mut second = registration(2, "fp-old-2");
        second.publication_sequence = Some(2);
        second.published_at_unix_ms = Some(1_000);
        registrar.register(second).await.unwrap();
        assert_eq!(
            catalog
                .current_model_publications("workspace-a")
                .await
                .unwrap()[0]
                .first_published_at_unix_ms,
            None,
            "E2"
        );
    }

    #[test]
    fn current_lifecycle_disposition_decision_table() {
        // R1-R5 in `check_current_lifecycle_disposition`: cross absent,
        // available and tombstone current states with both command types and
        // lower/equal/higher revision relations. Boundary representatives cover
        // zero and u64::MAX; Kani supplies every full-width value.
        let revisions = [0, 1, 7, u64::MAX - 1, u64::MAX];
        for current in [None].into_iter().chain(
            revisions
                .into_iter()
                .flat_map(|revision| [Some((revision, false)), Some((revision, true))]),
        ) {
            for incoming_revision in revisions {
                for incoming_available in [false, true] {
                    check_current_lifecycle_disposition(
                        current.map(|(revision, _)| revision),
                        current.is_some_and(|(_, available)| available),
                        incoming_revision,
                        incoming_available,
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn exact_session_profile_does_not_follow_mutable_current_selection() {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = LocalExecutableAgentRegistrar::new(catalog.clone());
        let mut historical = registration(1, "fp-1");
        historical.session_profile.model = Some("model-one".into());
        registrar.register(historical).await.unwrap();
        let mut current = registration(2, "fp-2");
        current.session_profile.model = Some("model-two".into());
        registrar.register(current).await.unwrap();

        assert_eq!(
            catalog
                .session_profile_in("workspace-a", "agent-a")
                .and_then(|profile| profile.model),
            Some("model-two".into())
        );
        assert_eq!(
            catalog
                .session_profile_at_revision_in("workspace-a", "agent-a", 1)
                .and_then(|profile| profile.model),
            Some("model-one".into())
        );
        assert!(
            catalog
                .session_profile_at_revision_in("workspace-a", "agent-a", 3)
                .is_none()
        );
        assert_eq!(
            catalog
                .executable_snapshot_at_revision_in("workspace-a", "agent-a", 1)
                .map(|snapshot| snapshot.fingerprint.0),
            Some("fp-1".into())
        );
    }

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
        let store = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .await
                .unwrap(),
        );
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
        let restored_store = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .await
                .unwrap(),
        );
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
        // R4 rev3 -> current rev3; R5 later arrival of distinct older exact
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
        let rev3 = registration(3, "fp-3");
        assert_eq!(
            registrar.register(rev3.clone()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredCurrent,
            "R4"
        );
        assert_eq!(
            catalog.current("workspace-a", "agent-a"),
            Some(rev3.clone()),
            "R4"
        );
        let rev2 = registration(2, "fp-2");
        assert_eq!(
            registrar.register(rev2.clone()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredHistorical,
            "R5 actual late arrival"
        );
        assert_eq!(catalog.current("workspace-a", "agent-a"), Some(rev3), "R5");
        assert_eq!(
            catalog.at_revision("workspace-a", "agent-a", 2),
            Some(rev2),
            "R5 full historical registration"
        );
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
        // move the lifecycle pointer; W4e equal unseen rev3 -> history only;
        // W5 new rev4 publication -> available again.
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
            registrar
                .withdraw(ExecutableAgentWithdrawal {
                    workspace_id: "workspace-a".into(),
                    agent_id: "agent-a".into(),
                    lifecycle_revision: 1,
                })
                .await
                .unwrap(),
            ExecutableAgentWithdrawalOutcome::HistoricalNoop,
            "W4 actual older withdrawal"
        );
        assert!(catalog.current("workspace-a", "agent-a").is_none(), "W4");
        assert_eq!(
            catalog.at_revision("workspace-a", "agent-a", 2),
            Some(registration(2, "fp-2")),
            "W4 full exact history retained"
        );
        assert_eq!(
            registrar.register(registration(3, "fp-3")).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredHistorical,
            "W4e an equal revision cannot revive the tombstone"
        );
        assert!(catalog.current("workspace-a", "agent-a").is_none(), "W4e");
        assert_eq!(
            catalog.at_revision("workspace-a", "agent-a", 3),
            Some(registration(3, "fp-3")),
            "W4e equal revision is retained exactly"
        );
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
