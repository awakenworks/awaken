//! In-memory reference [`EnvRegistry`] backend.
//!
//! [`InMemoryEnvRegistry`] is the open-tier single-process default the environments
//! routes wire when no durable backend is configured; it lives here beside the durable
//! sqlite/postgres sibling. The neutral port + value objects (`EnvItem`/`EnvUpdate`)
//! it operates on live inward in `awaken-session-contract`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
#[cfg(test)]
use awaken_environment_contract::EnvironmentConfig;
use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvItem,
    EnvRegistry, EnvUpdate, EnvironmentRevision, OBJECT_AT,
};

pub struct InMemoryEnvRegistry {
    envs: Mutex<BTreeMap<String, EnvItem>>,
    revisions: Mutex<BTreeMap<(String, EnvironmentRevision), EnvItem>>,
    commands: Mutex<BTreeMap<String, (String, String)>>,
    seq: AtomicU64,
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
            envs: Mutex::new(BTreeMap::new()),
            revisions: Mutex::new(BTreeMap::new()),
            commands: Mutex::new(BTreeMap::new()),
            seq: AtomicU64::new(0),
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
        let mut commands = self.commands.lock().unwrap();
        if let Some((existing_fingerprint, environment_id)) = commands.get(&command.command_id) {
            if existing_fingerprint != &fingerprint {
                return Err(CreateEnvironmentError::IdempotencyConflict);
            }
            let item = self
                .envs
                .lock()
                .unwrap()
                .get(environment_id)
                .cloned()
                .ok_or_else(|| CreateEnvironmentError::Store("command target is missing".into()))?;
            return Ok(CreateEnvironmentOutcome::Replayed(item));
        }
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
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
        self.envs.lock().unwrap().insert(id, item.clone());
        self.revisions
            .lock()
            .unwrap()
            .insert((item.id.clone(), item.revision), item.clone());
        commands.insert(command.command_id, (fingerprint, item.id.clone()));
        Ok(CreateEnvironmentOutcome::Created(item))
    }

    async fn list_active(&self) -> Vec<EnvItem> {
        self.envs
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.archived_at.is_none())
            .cloned()
            .collect()
    }

    async fn list_all(&self) -> Vec<EnvItem> {
        self.envs.lock().unwrap().values().cloned().collect()
    }

    async fn get(&self, id: &str) -> Option<EnvItem> {
        self.envs.lock().unwrap().get(id).cloned()
    }

    async fn get_revision(&self, id: &str, revision: EnvironmentRevision) -> Option<EnvItem> {
        self.revisions
            .lock()
            .unwrap()
            .get(&(id.to_string(), revision))
            .cloned()
    }

    async fn exists(&self, id: &str) -> bool {
        self.envs.lock().unwrap().contains_key(id)
    }

    async fn update(&self, id: &str, patch: EnvUpdate) -> Option<EnvItem> {
        let mut envs = self.envs.lock().unwrap();
        let item = envs.get_mut(id)?;
        if item.archived_at.is_some() {
            return None;
        }
        item.apply(patch);
        let item = item.clone();
        self.revisions
            .lock()
            .unwrap()
            .insert((item.id.clone(), item.revision), item.clone());
        Some(item)
    }

    async fn archive(&self, id: &str) -> Option<EnvItem> {
        let mut envs = self.envs.lock().unwrap();
        let item = envs.get_mut(id)?;
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
        self.revisions
            .lock()
            .unwrap()
            .insert((item.id.clone(), item.revision), item.clone());
        Some(item)
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
