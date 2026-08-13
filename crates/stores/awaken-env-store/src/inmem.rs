//! In-memory reference [`EnvRegistry`] backend.
//!
//! [`InMemoryEnvRegistry`] is the single-process reference fixture. Product
//! composition selects the durable SQLite/Postgres sibling explicitly. The neutral
//! port + value objects (`EnvItem`/`EnvUpdate`) live inward in
//! `awaken-environment-contract`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
#[cfg(test)]
use awaken_environment_contract::EnvironmentConfig;
use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvItem,
    EnvRegistry, EnvUpdate, EnvironmentRegistrationIntent, EnvironmentRegistrationIntentFilter,
    EnvironmentRevision, OBJECT_AT,
};

pub struct InMemoryEnvRegistry {
    state: Mutex<InMemoryEnvRegistryState>,
}

#[derive(Default)]
struct InMemoryEnvRegistryState {
    envs: BTreeMap<String, EnvItem>,
    revisions: BTreeMap<(String, EnvironmentRevision), EnvItem>,
    commands: BTreeMap<String, (String, String)>,
    intents: BTreeMap<(String, EnvironmentRevision), EnvironmentRegistrationIntent>,
    seq: u64,
}

impl Default for InMemoryEnvRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryEnvRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(InMemoryEnvRegistryState::default()),
        }
    }
}

#[async_trait]
impl EnvRegistry for InMemoryEnvRegistry {
    async fn create_once(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<CreateEnvironmentOutcome, CreateEnvironmentError> {
        let fingerprint = command.fingerprint();
        let mut state = self.state.lock().unwrap();
        if let Some((existing_fingerprint, environment_id)) =
            state.commands.get(&command.command_id)
        {
            if existing_fingerprint != &fingerprint {
                return Err(CreateEnvironmentError::IdempotencyConflict);
            }
            let item =
                state.envs.get(environment_id).cloned().ok_or_else(|| {
                    CreateEnvironmentError::Store("command target is missing".into())
                })?;
            return Ok(CreateEnvironmentOutcome::Replayed(item));
        }
        let n = state.seq;
        state.seq = state
            .seq
            .checked_add(1)
            .expect("Environment id sequence exhausted");
        let id = format!("env_{n:016}");
        let item = EnvItem {
            id: id.clone(),
            revision: EnvironmentRevision(1),
            name: command.name,
            description: command.description,
            metadata: command.metadata,
            scope: command.scope,
            config: command.config,
            sandbox_policy: None,
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

    async fn list_active(&self) -> Vec<EnvItem> {
        self.state
            .lock()
            .unwrap()
            .envs
            .values()
            .filter(|e| e.archived_at.is_none())
            .cloned()
            .collect()
    }

    async fn list_all(&self) -> Vec<EnvItem> {
        self.state.lock().unwrap().envs.values().cloned().collect()
    }

    async fn get(&self, id: &str) -> Option<EnvItem> {
        self.state.lock().unwrap().envs.get(id).cloned()
    }

    async fn get_revision(&self, id: &str, revision: EnvironmentRevision) -> Option<EnvItem> {
        self.state
            .lock()
            .unwrap()
            .revisions
            .get(&(id.to_string(), revision))
            .cloned()
    }

    async fn exists(&self, id: &str) -> bool {
        self.state.lock().unwrap().envs.contains_key(id)
    }

    async fn update(&self, id: &str, patch: EnvUpdate) -> Option<EnvItem> {
        let mut state = self.state.lock().unwrap();
        let item = state.envs.get_mut(id)?;
        if item.archived_at.is_some() {
            return None;
        }
        if !item.apply(patch) {
            return Some(item.clone());
        }
        let item = item.clone();
        state
            .revisions
            .insert((item.id.clone(), item.revision), item.clone());
        state.intents.insert(
            (item.id.clone(), item.revision),
            EnvironmentRegistrationIntent::for_item(&item),
        );
        Some(item)
    }

    async fn archive(&self, id: &str) -> Option<EnvItem> {
        let mut state = self.state.lock().unwrap();
        let item = state.envs.get_mut(id)?;
        if item.archived_at.is_some() {
            return Some(item.clone());
        }
        item.archived_at = Some(OBJECT_AT.to_string());
        item.revision = EnvironmentRevision(
            item.revision
                .0
                .checked_add(1)
                .expect("Environment revision exhausted"),
        );
        let item = item.clone();
        state
            .revisions
            .insert((item.id.clone(), item.revision), item.clone());
        state.intents.insert(
            (item.id.clone(), item.revision),
            EnvironmentRegistrationIntent::for_item(&item),
        );
        Some(item)
    }

    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, String> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .intents
            .get(&(id.to_string(), revision))
            .cloned())
    }

    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, String> {
        Ok(self
            .state
            .lock()
            .unwrap()
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
    ) -> Result<bool, String> {
        let mut state = self.state.lock().unwrap();
        let Some(stored) = state
            .intents
            .get_mut(&(intent.environment_id.clone(), intent.revision))
        else {
            return Ok(false);
        };
        if stored.operation != intent.operation {
            return Err("Environment registration intent operation mismatch".into());
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
        InMemoryEnvRegistry::new()
    }

    #[tokio::test]
    async fn create_get_list_and_archive() {
        let r = r();
        let e = r
            .create("prod".into(), String::new(), BTreeMap::new(), config())
            .await;
        assert!(r.exists(&e.id).await);
        assert!(e.is_self_hosted());
        assert_eq!(r.list_active().await.len(), 1);
        r.archive(&e.id).await.expect("archive");
        assert_eq!(r.list_active().await.len(), 0, "archived drops from active");
        assert!(r.get(&e.id).await.is_some(), "still retrievable");
    }

    /// Terminal archive preserves immutable history while denying new selection;
    /// archiving a non-existent id fails closed without fabricating a record.
    #[tokio::test]
    async fn archive_preserves_history_and_missing_id_fails_closed() {
        let r = r();
        // C: id does not exist -> archive returns None (fail-closed, no fabrication).
        assert!(
            r.archive("env_missing").await.is_none(),
            "archive of missing id"
        );
        let e = r
            .create("prod".into(), String::new(), BTreeMap::new(), config())
            .await;
        // Terminal denial keeps the current tombstone and exact authored revision.
        assert!(r.archive(&e.id).await.is_some());
        assert!(r.get(&e.id).await.is_some(), "archive keeps the record");
        assert!(
            r.get_revision(&e.id, EnvironmentRevision(1))
                .await
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
            .await;
        let up = r
            .update(
                &e.id,
                EnvUpdate {
                    name: Some("renamed".into()),
                    metadata: Some(BTreeMap::from([("drop".into(), None)])),
                    ..Default::default()
                },
            )
            .await
            .expect("updated");
        assert_eq!(up.name, "renamed");
        assert!(up.metadata.contains_key("keep"));
        assert!(!up.metadata.contains_key("drop"), "null deletes the key");
    }

    // (`network_policy`/`project` are wire/provisioning projections; they moved to the
    // Managed adapter along with their tests. This crate keeps only the neutral registry.)
}
