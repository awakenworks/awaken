// @formal surface obligation=environment.registration_intent_is_atomic_recoverable_and_idempotent
// @formal reason: In-memory Environment registration intent store.
//! In-memory reference environment definition roles backend.
//!
//! [`InMemoryEnvRegistry`] is the single-process reference fixture. Product
//! composition selects the durable SQLite/Postgres sibling explicitly. The neutral
//! port + value objects (`EnvItem`/`EnvUpdate`) live inward in
//! `awaken-environment-contract`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
#[cfg(test)]
use awaken_environment_contract::EnvironmentConfig;
use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvItem, EnvUpdate,
    EnvironmentDefinitionQueries, EnvironmentDefinitionStore, EnvironmentRegistrationIntent,
    EnvironmentRegistrationIntentFilter, EnvironmentRegistrationOutbox, EnvironmentRevision,
    EnvironmentStoreError,
};

use awaken_sync::{InvariantLock, LeafLock};
use awaken_time::WallClock;
use awaken_time::WallClockExt;

pub struct InMemoryEnvRegistry {
    state: InvariantLock<InMemoryEnvRegistryState>,
    fail_acknowledgements: AtomicUsize,
    intent_filters: LeafLock<Vec<EnvironmentRegistrationIntentFilter>>,
    /// The one source of every observation timestamp this reference registry
    /// writes — the same seam the durable siblings carry.
    clock: Arc<dyn WallClock>,
}

#[derive(Default)]
struct InMemoryEnvRegistryState {
    envs: BTreeMap<String, EnvItem>,
    revisions: BTreeMap<(String, EnvironmentRevision), EnvItem>,
    commands: BTreeMap<String, (String, String)>,
    intents: BTreeMap<(String, EnvironmentRevision), EnvironmentRegistrationIntent>,
    seq: u64,
}

impl InMemoryEnvRegistry {
    /// The reference registry stamping and judging with `clock`, the composing
    /// process's (or test's) one wall clock.
    #[must_use]
    pub fn new(clock: Arc<dyn WallClock>) -> Self {
        Self {
            state: InvariantLock::new(InMemoryEnvRegistryState::default()),
            fail_acknowledgements: AtomicUsize::new(0),
            intent_filters: LeafLock::new(Vec::new()),
            clock,
        }
    }

    /// Test-support fault injection for the delivery/acknowledgement ambiguity.
    pub fn fail_next_acknowledgements(&self, count: usize) {
        self.fail_acknowledgements.store(count, Ordering::SeqCst);
    }

    /// Test-support observation of startup `All` versus steady-state `Pending`.
    #[must_use]
    pub fn registration_intent_filters(&self) -> Vec<EnvironmentRegistrationIntentFilter> {
        self.intent_filters.lock().clone()
    }

    fn state(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, InMemoryEnvRegistryState>, EnvironmentStoreError> {
        self.state.lock().map_err(|_| {
            EnvironmentStoreError::Backend("Environment registry mutex poisoned".into())
        })
    }
}

#[async_trait]
impl EnvironmentDefinitionQueries for InMemoryEnvRegistry {
    async fn list_active(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        Ok(self
            .state()?
            .envs
            .values()
            .filter(|e| e.archived_at.is_none())
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        Ok(self.state()?.envs.values().cloned().collect())
    }

    async fn get(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        Ok(self.state()?.envs.get(id).cloned())
    }

    async fn get_revision(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        Ok(self
            .state()?
            .revisions
            .get(&(id.to_string(), revision))
            .cloned())
    }

    async fn exists(&self, id: &str) -> Result<bool, EnvironmentStoreError> {
        Ok(self.state()?.envs.contains_key(id))
    }
}

fn committed_create_result(
    state: &InMemoryEnvRegistryState,
    command: &CreateEnvironmentCommand,
) -> Result<Option<EnvItem>, CreateEnvironmentError> {
    let fingerprint = command.fingerprint();
    if let Some((existing_fingerprint, environment_id)) = state.commands.get(&command.command_id) {
        if existing_fingerprint != &fingerprint {
            return Err(CreateEnvironmentError::IdempotencyConflict);
        }
        let item = state
            .revisions
            .get(&(environment_id.clone(), EnvironmentRevision(1)))
            .cloned()
            .ok_or_else(|| CreateEnvironmentError::OriginalResultUnavailable {
                environment_id: environment_id.clone(),
            })?;
        return Ok(Some(item));
    }
    Ok(None)
}

#[async_trait]
impl EnvironmentDefinitionStore for InMemoryEnvRegistry {
    async fn find_create_result(
        &self,
        command: &CreateEnvironmentCommand,
    ) -> Result<Option<EnvItem>, CreateEnvironmentError> {
        let state = self
            .state()
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        committed_create_result(&state, command)
    }

    async fn create_once(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<CreateEnvironmentOutcome, CreateEnvironmentError> {
        command.config.validate()?;
        let committed_at = self.clock.observed_at();
        let fingerprint = command.fingerprint();
        let mut state = self
            .state()
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        if let Some(item) = committed_create_result(&state, &command)? {
            return Ok(CreateEnvironmentOutcome::Replayed(item));
        }
        let n = state.seq;
        state.seq = state.seq.checked_add(1).ok_or_else(|| {
            CreateEnvironmentError::Store("Environment id sequence exhausted".into())
        })?;
        let id = format!("env_{n:016}");
        let item = EnvItem {
            id: id.clone(),
            revision: EnvironmentRevision(1),
            name: command.name,
            description: command.description,
            metadata: command.metadata,
            scope: command.scope,
            config: command.config,
            sandbox_policy: command.sandbox_policy,
            created_at: Some(committed_at.clone()),
            updated_at: Some(committed_at),
            archived_at: None,
        };
        state.envs.insert(id, item.clone());
        state
            .revisions
            .insert((item.id.clone(), item.revision), item.clone());
        state.intents.insert(
            (item.id.clone(), item.revision),
            EnvironmentRegistrationIntent::for_item(&item),
        );
        state
            .commands
            .insert(command.command_id, (fingerprint, item.id.clone()));
        Ok(CreateEnvironmentOutcome::Created(item))
    }

    async fn update(
        &self,
        id: &str,
        expected_revision: EnvironmentRevision,
        patch: EnvUpdate,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let committed_at = self.clock.observed_at();
        let mut state = self.state()?;
        let Some(item) = state.envs.get_mut(id) else {
            return Ok(None);
        };
        if item.archived_at.is_some() {
            return Ok(None);
        }
        if item.revision != expected_revision {
            return Err(EnvironmentStoreError::RevisionConflict);
        }
        if !item.apply(patch)? {
            return Ok(Some(item.clone()));
        }
        item.updated_at = Some(committed_at);
        let item = item.clone();
        state
            .revisions
            .insert((item.id.clone(), item.revision), item.clone());
        state.intents.insert(
            (item.id.clone(), item.revision),
            EnvironmentRegistrationIntent::for_item(&item),
        );
        Ok(Some(item))
    }

    async fn archive(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let committed_at = self.clock.observed_at();
        let mut state = self.state()?;
        let Some(item) = state.envs.get_mut(id) else {
            return Ok(None);
        };
        if item.archived_at.is_some() {
            return Ok(Some(item.clone()));
        }
        let revision = EnvironmentRevision(
            item.revision
                .0
                .checked_add(1)
                .ok_or(EnvironmentStoreError::RevisionExhausted)?,
        );
        item.archived_at = Some(committed_at.clone());
        item.updated_at = Some(committed_at);
        item.revision = revision;
        let item = item.clone();
        state
            .revisions
            .insert((item.id.clone(), item.revision), item.clone());
        state.intents.insert(
            (item.id.clone(), item.revision),
            EnvironmentRegistrationIntent::for_item(&item),
        );
        Ok(Some(item))
    }
}

#[async_trait]
impl EnvironmentRegistrationOutbox for InMemoryEnvRegistry {
    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        Ok(self
            .state()?
            .intents
            .get(&(id.to_string(), revision))
            .cloned())
    }

    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        self.intent_filters.lock().push(filter);
        Ok(self
            .state()?
            .intents
            .values()
            .filter(|intent| {
                filter == EnvironmentRegistrationIntentFilter::All || !intent.delivered
            })
            .cloned()
            .collect())
    }

    async fn mark_registration_intent_delivered(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<bool, EnvironmentStoreError> {
        if self
            .fail_acknowledgements
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(EnvironmentStoreError::Backend(
                "injected Environment registration acknowledgement failure".into(),
            ));
        }
        let mut state = self.state()?;
        let Some(stored) = state
            .intents
            .get_mut(&(intent.environment_id.clone(), intent.revision))
        else {
            return Ok(false);
        };
        if stored.operation != intent.operation {
            return Err(EnvironmentStoreError::Backend(
                "Environment registration intent operation mismatch".into(),
            ));
        }
        stored.delivered = true;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EnvironmentConfig {
        EnvironmentConfig::SelfHosted
    }

    fn r() -> InMemoryEnvRegistry {
        InMemoryEnvRegistry::new(awaken_time::system_clock())
    }

    #[tokio::test]
    async fn exhausted_update_and_archive_preserve_current_history_and_delivery_intents() {
        // U1 changed update / A1 archive at MAX reject without a partial write;
        // U2 exact no-op replay succeeds. Observe the aggregate, immutable
        // revisions and registration log, rather than a fixture call counter.
        let r = r();
        let created = r
            .create("before".into(), String::new(), BTreeMap::new(), config())
            .await
            .unwrap();
        {
            let mut state = r.state().unwrap();
            let item = state.envs.get_mut(&created.id).unwrap();
            item.revision = EnvironmentRevision(u64::MAX);
            let item = item.clone();
            state
                .revisions
                .insert((item.id.clone(), item.revision), item);
        }
        let before = r.get(&created.id).await.unwrap().unwrap();
        let history = r.state().unwrap().revisions.clone();
        let intents = r
            .registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap();
        assert_eq!(
            r.update(
                &created.id,
                before.revision,
                EnvUpdate {
                    name: Some("after".into()),
                    ..Default::default()
                }
            )
            .await,
            Err(EnvironmentStoreError::RevisionExhausted),
        );
        assert_eq!(
            r.archive(&created.id).await,
            Err(EnvironmentStoreError::RevisionExhausted)
        );
        assert_eq!(
            r.update(&created.id, before.revision, EnvUpdate::default())
                .await,
            Ok(Some(before.clone()))
        );
        assert_eq!(r.get(&created.id).await, Ok(Some(before)));
        assert_eq!(r.state().unwrap().revisions, history);
        assert_eq!(
            r.registration_intents(EnvironmentRegistrationIntentFilter::All)
                .await
                .unwrap(),
            intents
        );
    }

    #[tokio::test]
    async fn create_get_list_and_archive() {
        let r = r();
        let e = r
            .create("prod".into(), String::new(), BTreeMap::new(), config())
            .await
            .expect("create Environment");
        assert!(r.exists(&e.id).await.expect("read Environment"));
        assert!(e.is_self_hosted());
        assert_eq!(r.list_active().await.expect("list Environments").len(), 1);
        r.archive(&e.id)
            .await
            .expect("archive store operation")
            .expect("archive");
        assert_eq!(
            r.list_active().await.expect("list Environments").len(),
            0,
            "archived drops from active"
        );
        assert!(
            r.get(&e.id).await.expect("read Environment").is_some(),
            "still retrievable"
        );
    }

    /// Terminal archive preserves immutable history while denying new selection;
    /// archiving a non-existent id fails closed without fabricating a record.
    #[tokio::test]
    async fn archive_preserves_history_and_missing_id_fails_closed() {
        let r = r();
        // C: id does not exist -> archive returns None (fail-closed, no fabrication).
        assert!(
            r.archive("env_missing")
                .await
                .expect("archive missing Environment")
                .is_none(),
            "archive of missing id"
        );
        let e = r
            .create("prod".into(), String::new(), BTreeMap::new(), config())
            .await
            .expect("create Environment");
        // Terminal denial keeps the current tombstone and exact authored revision.
        assert!(
            r.archive(&e.id)
                .await
                .expect("archive Environment")
                .is_some()
        );
        assert!(
            r.get(&e.id).await.expect("read Environment").is_some(),
            "archive keeps the record"
        );
        assert!(
            r.get_revision(&e.id, EnvironmentRevision(1))
                .await
                .expect("read Environment revision")
                .is_some(),
            "authored history remains"
        );
    }

    #[tokio::test]
    async fn update_patches_fields_and_metadata_null_deletes() {
        let r = r();
        let e = r
            .create(
                "e".into(),
                String::new(),
                BTreeMap::from([("keep".into(), "1".into()), ("drop".into(), "2".into())]),
                config(),
            )
            .await
            .expect("create Environment");
        let up = r
            .update(
                &e.id,
                e.revision,
                EnvUpdate {
                    name: Some("renamed".into()),
                    metadata: Some(BTreeMap::from([("drop".into(), None)])),
                    ..Default::default()
                },
            )
            .await
            .expect("update Environment store operation")
            .expect("updated");
        assert_eq!(up.name, "renamed");
        assert!(up.metadata.contains_key("keep"));
        assert!(!up.metadata.contains_key("drop"), "null deletes the key");
    }

    // (`network_policy`/`project` are wire/provisioning projections; they moved to the
    // Managed adapter along with their tests. This crate keeps only the neutral registry.)
}
