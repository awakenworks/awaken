//! The credential source repository port (ADR-0043) — stores the **secret-free**
//! [`CredentialSource`] rows (the sealed material lives behind [`SecretStore`], a
//! separate port). Its own `credential` migration scope is what lets the whole
//! domain be split into its own database/service (blast-radius isolation).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::{
    CredentialCreateParams, CredentialError, CredentialPool, CredentialPoolId, CredentialSource,
    CredentialSourceId, SecretStore, prepare_source,
};

/// Secret-free durable intent written before secret material is touched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CredentialCreationIntent {
    pub source: CredentialSource,
}

/// The credential-source store port. Secret-free rows only. Pools are stored here
/// too (they are secret-free groupings of sources the resolver fails over across).
#[async_trait::async_trait]
pub trait CredentialRepo: Send + Sync {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError>;
    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError>;
    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError>;

    async fn begin_creation(&self, intent: CredentialCreationIntent)
    -> Result<(), CredentialError>;
    /// Atomically publish the source and retire its creation intent.
    async fn commit_creation(&self, source: CredentialSource) -> Result<(), CredentialError>;
    async fn pending_creations(&self) -> Result<Vec<CredentialCreationIntent>, CredentialError>;
    async fn abort_creation(&self, id: &CredentialSourceId) -> Result<(), CredentialError>;
    /// Every material reference reachable from committed metadata.
    async fn material_refs(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        Err(CredentialError::Storage(
            "credential material inventory is not supported by this repository".to_string(),
        ))
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError>;
    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError>;
    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError>;
}

/// In-memory [`CredentialRepo`] (dev / tests / single-machine default).
#[derive(Default)]
struct RepoState {
    rows: HashMap<String, CredentialSource>,
    pools: HashMap<String, CredentialPool>,
    intents: HashMap<String, CredentialCreationIntent>,
}

#[derive(Default)]
pub struct InMemoryCredentialRepo {
    state: Mutex<RepoState>,
}

impl InMemoryCredentialRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl CredentialRepo for InMemoryCredentialRepo {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .rows
            .insert(source.id.0.clone(), source);
        Ok(())
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .rows
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::SourceNotFound(id.0.clone()))
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .rows
            .values()
            .filter(|s| s.workspace_id == workspace_id)
            .cloned()
            .collect())
    }

    async fn begin_creation(
        &self,
        intent: CredentialCreationIntent,
    ) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .intents
            .entry(intent.source.id.0.clone())
            .or_insert(intent);
        Ok(())
    }

    async fn commit_creation(&self, source: CredentialSource) -> Result<(), CredentialError> {
        let mut state = self.state.lock().expect("credential repo");
        state.rows.insert(source.id.0.clone(), source.clone());
        state.intents.remove(&source.id.0);
        Ok(())
    }

    async fn pending_creations(&self) -> Result<Vec<CredentialCreationIntent>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .intents
            .values()
            .cloned()
            .collect())
    }

    async fn abort_creation(&self, id: &CredentialSourceId) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .intents
            .remove(&id.0);
        Ok(())
    }

    async fn material_refs(&self) -> Result<Vec<crate::SecretRef>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .rows
            .values()
            .filter_map(|source| source.material_ref.clone())
            .collect())
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .pools
            .insert(pool.id.0.clone(), pool);
        Ok(())
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        self.state
            .lock()
            .expect("credential repo")
            .pools
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::PoolNotFound(id.0.clone()))
    }

    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError> {
        Ok(self
            .state
            .lock()
            .expect("credential repo")
            .pools
            .values()
            .filter(|p| p.workspace_id == workspace_id)
            .cloned()
            .collect())
    }
}

/// Enter a credential end-to-end (secret-in / secret-free-out): seal the secret in
/// the [`SecretStore`], persist the secret-free row in the [`CredentialRepo`], and
/// return the row. The one write path an operator/the Managed wire drives.
pub async fn enter_credential(
    params: CredentialCreateParams,
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialSource, CredentialError> {
    let (source, secret) = prepare_source(params);
    repo.begin_creation(CredentialCreationIntent {
        source: source.clone(),
    })
    .await?;

    if let (Some(material_ref), Some(secret)) = (&source.material_ref, secret)
        && let Err(error) = store.put(material_ref, secret).await
    {
        // A failed put may still have partially written. Only retire the durable
        // intent after idempotent cleanup succeeds; otherwise recovery owns it.
        if store.delete(material_ref).await.is_ok() {
            repo.abort_creation(&source.id).await?;
        }
        return Err(error);
    }
    // Never compensate an ambiguous commit error inline: the intent remains the
    // recovery authority, preventing deletion of a source that actually committed.
    repo.commit_creation(source.clone()).await?;
    Ok(source)
}

/// Reconcile every interrupted creation after restart. A published source wins
/// and keeps its material; an unpublished intent is compensated and retired.
pub async fn recover_credential_creations(
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<usize, CredentialError> {
    let intents = repo.pending_creations().await?;
    let mut recovered = 0;
    for intent in intents {
        match repo.get(&intent.source.id).await {
            Ok(_) => repo.abort_creation(&intent.source.id).await?,
            Err(CredentialError::SourceNotFound(_)) => {
                if let Some(material_ref) = &intent.source.material_ref {
                    store.delete(material_ref).await?;
                }
                repo.abort_creation(&intent.source.id).await?;
            }
            Err(error) => return Err(error),
        }
        recovered += 1;
    }
    Ok(recovered)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInventoryReport {
    pub orphaned_deleted: Vec<crate::SecretRef>,
    pub missing_material: Vec<crate::SecretRef>,
}

/// Compare the secret inventory with committed metadata and in-flight intents.
/// Only credential-owned `sec:cred:` keys are eligible for deletion; webhook and
/// OAuth material may share the physical store and is deliberately untouched.
pub async fn reconcile_credential_inventory(
    store: &dyn SecretStore,
    repo: &dyn CredentialRepo,
) -> Result<CredentialInventoryReport, CredentialError> {
    let inventory = store.inventory().await?;
    // Read intents first. Publication atomically removes an intent while adding
    // its metadata row, so either this read sees the in-flight protection or the
    // following committed-reference read sees the published source.
    let pending = repo.pending_creations().await?;
    let committed = repo.material_refs().await?;
    let present: HashSet<String> = inventory.iter().map(|item| item.0.clone()).collect();
    let committed_keys: HashSet<String> = committed.iter().map(|item| item.0.clone()).collect();
    let protected: HashSet<String> = committed_keys
        .iter()
        .cloned()
        .chain(pending.iter().filter_map(|intent| {
            intent
                .source
                .material_ref
                .as_ref()
                .map(|reference| reference.0.clone())
        }))
        .collect();

    let mut orphaned_deleted = Vec::new();
    for reference in inventory {
        if reference.0.starts_with("sec:cred:") && !protected.contains(&reference.0) {
            store.delete(&reference).await?;
            orphaned_deleted.push(reference);
        }
    }
    let missing_material = committed
        .into_iter()
        .filter(|reference| !present.contains(&reference.0))
        .collect();
    Ok(CredentialInventoryReport {
        orphaned_deleted,
        missing_material,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::{CredentialKind, InMemorySecretStore, materialize};
    use awaken_agent_contract::RedactedString;

    struct FaultyDeleteStore {
        inner: InMemorySecretStore,
        fail_before_delete: AtomicBool,
        lose_first_response: AtomicBool,
    }

    #[async_trait::async_trait]
    impl SecretStore for FaultyDeleteStore {
        async fn put(
            &self,
            r: &crate::SecretRef,
            secret: RedactedString,
        ) -> Result<(), CredentialError> {
            self.inner.put(r, secret).await
        }

        async fn get(&self, r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
            self.inner.get(r).await
        }

        async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
            if self.fail_before_delete.load(Ordering::SeqCst) {
                return Err(CredentialError::Storage("delete timeout".into()));
            }
            self.inner.delete(r).await?;
            if self.lose_first_response.swap(false, Ordering::SeqCst) {
                return Err(CredentialError::Storage("delete response lost".into()));
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct RejectingRepo {
        inner: InMemoryCredentialRepo,
    }

    #[async_trait::async_trait]
    impl CredentialRepo for RejectingRepo {
        async fn put(&self, _source: CredentialSource) -> Result<(), CredentialError> {
            Err(CredentialError::Storage("injected row failure".into()))
        }

        async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
            Err(CredentialError::SourceNotFound(id.0.clone()))
        }

        async fn list(
            &self,
            _workspace_id: &str,
        ) -> Result<Vec<CredentialSource>, CredentialError> {
            Ok(Vec::new())
        }

        async fn begin_creation(
            &self,
            intent: CredentialCreationIntent,
        ) -> Result<(), CredentialError> {
            self.inner.begin_creation(intent).await
        }

        async fn commit_creation(&self, _source: CredentialSource) -> Result<(), CredentialError> {
            Err(CredentialError::Storage("injected row failure".into()))
        }

        async fn pending_creations(
            &self,
        ) -> Result<Vec<CredentialCreationIntent>, CredentialError> {
            self.inner.pending_creations().await
        }

        async fn abort_creation(&self, id: &CredentialSourceId) -> Result<(), CredentialError> {
            self.inner.abort_creation(id).await
        }

        async fn put_pool(&self, _pool: CredentialPool) -> Result<(), CredentialError> {
            Ok(())
        }

        async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
            Err(CredentialError::PoolNotFound(id.0.clone()))
        }

        async fn list_pools(
            &self,
            _workspace_id: &str,
        ) -> Result<Vec<CredentialPool>, CredentialError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn enter_stores_row_and_secret_separately() {
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: Some(RedactedString::new("sk-xyz")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();

        // The row is retrievable and secret-free; the secret materializes from the store.
        let got = repo.get(&source.id).await.unwrap();
        assert!(!serde_json::to_string(&got).unwrap().contains("sk-xyz"));
        assert_eq!(
            materialize(&got, &store).await.unwrap().expose_secret(),
            "sk-xyz"
        );
        assert_eq!(repo.list("ws").await.unwrap().len(), 1);
        assert_eq!(repo.list("other").await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn ambiguous_row_commit_is_reconciled_from_the_durable_intent() {
        let store = InMemorySecretStore::new();
        let repo = RejectingRepo::default();
        let result = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("must-not-be-orphaned")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await;

        assert!(
            matches!(result, Err(CredentialError::Storage(message)) if message == "injected row failure")
        );
        // An ambiguous commit error is not compensated inline: deleting here could
        // break a row whose commit succeeded but whose response was lost.
        assert_eq!(store.map.lock().expect("secret store mutex").len(), 1);
        assert_eq!(
            recover_credential_creations(&store, &repo).await.unwrap(),
            1
        );
        assert!(store.map.lock().expect("secret store mutex").is_empty());
        assert!(repo.pending_creations().await.unwrap().is_empty());
    }

    async fn interrupted_creation(store: &dyn SecretStore, repo: &dyn CredentialRepo) {
        let source = CredentialSource {
            id: CredentialSourceId("cred:ws:interrupted".into()),
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: Some("anthropic".into()),
            env_key: None,
            material_ref: Some(crate::SecretRef("sec:cred:ws:interrupted".into())),
            oauth_command: None,
            status: crate::CredentialStatus::Active,
            version: 1,
        };
        repo.begin_creation(CredentialCreationIntent {
            source: source.clone(),
        })
        .await
        .unwrap();
        store
            .put(
                source.material_ref.as_ref().unwrap(),
                RedactedString::new("orphan candidate"),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_timeout_keeps_the_intent_for_a_later_reconciliation() {
        let store = FaultyDeleteStore {
            inner: InMemorySecretStore::new(),
            fail_before_delete: AtomicBool::new(true),
            lose_first_response: AtomicBool::new(false),
        };
        let repo = InMemoryCredentialRepo::new();
        interrupted_creation(&store, &repo).await;
        assert!(recover_credential_creations(&store, &repo).await.is_err());
        assert_eq!(repo.pending_creations().await.unwrap().len(), 1);
        store.fail_before_delete.store(false, Ordering::SeqCst);
        assert_eq!(
            recover_credential_creations(&store, &repo).await.unwrap(),
            1
        );
        assert!(repo.pending_creations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn lost_delete_response_is_retried_idempotently() {
        let store = FaultyDeleteStore {
            inner: InMemorySecretStore::new(),
            fail_before_delete: AtomicBool::new(false),
            lose_first_response: AtomicBool::new(true),
        };
        let repo = InMemoryCredentialRepo::new();
        interrupted_creation(&store, &repo).await;
        assert!(recover_credential_creations(&store, &repo).await.is_err());
        assert_eq!(repo.pending_creations().await.unwrap().len(), 1);
        assert_eq!(
            recover_credential_creations(&store, &repo).await.unwrap(),
            1
        );
        assert!(repo.pending_creations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn inventory_deletes_only_unreferenced_credential_material() {
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let committed = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("kept")),
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();
        let orphan = crate::SecretRef("sec:cred:ws:legacy-orphan".into());
        let webhook = crate::SecretRef("whsec:shared-store".into());
        store
            .put(&orphan, RedactedString::new("delete"))
            .await
            .unwrap();
        store
            .put(&webhook, RedactedString::new("preserve"))
            .await
            .unwrap();

        let report = reconcile_credential_inventory(&store, &repo).await.unwrap();
        assert_eq!(report.orphaned_deleted, vec![orphan.clone()]);
        assert!(report.missing_material.is_empty());
        assert!(store.get(&orphan).await.is_err());
        assert!(store.get(&webhook).await.is_ok());
        assert!(
            store
                .get(committed.material_ref.as_ref().unwrap())
                .await
                .is_ok()
        );
        assert!(
            reconcile_credential_inventory(&store, &repo)
                .await
                .unwrap()
                .orphaned_deleted
                .is_empty()
        );
    }

    #[tokio::test]
    async fn inventory_reports_metadata_whose_secret_is_missing() {
        let repo = InMemoryCredentialRepo::new();
        let store = InMemorySecretStore::new();
        let reference = crate::SecretRef("sec:cred:ws:missing".into());
        let source = CredentialSource {
            id: CredentialSourceId("cred:ws:missing".into()),
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            material_ref: Some(reference.clone()),
            oauth_command: None,
            status: crate::CredentialStatus::Active,
            version: 1,
        };
        repo.put(source.clone()).await.unwrap();

        let report = reconcile_credential_inventory(&store, &repo).await.unwrap();
        assert_eq!(report.missing_material, vec![reference]);
        assert!(materialize(&source, &store).await.is_err());
    }

    #[tokio::test]
    async fn pools_round_trip_and_list_scopes_by_workspace() {
        let repo = InMemoryCredentialRepo::new();
        let pool = |id: &str, ws: &str| CredentialPool {
            id: CredentialPoolId(id.into()),
            workspace_id: ws.into(),
            members: Vec::new(),
            policy: crate::SelectionPolicy::FirstHealthy,
        };
        repo.put_pool(pool("pool:a", "ws")).await.unwrap();
        repo.put_pool(pool("pool:b", "ws")).await.unwrap();
        repo.put_pool(pool("pool:c", "other")).await.unwrap();

        let got = repo
            .get_pool(&CredentialPoolId("pool:a".into()))
            .await
            .unwrap();
        assert_eq!(got.workspace_id, "ws");
        assert_eq!(repo.list_pools("ws").await.unwrap().len(), 2);
        assert_eq!(repo.list_pools("other").await.unwrap().len(), 1);
        assert_eq!(repo.list_pools("empty").await.unwrap().len(), 0);
    }

    /// F11(d): `get` is a deliberate **unscoped by-id primitive** — keyed by source
    /// id only, never by workspace (only `list`/`list_pools` filter by workspace).
    /// Tenancy is NOT enforced on this low-level read; it is enforced one layer up,
    /// at credential *resolution* (`awaken_config_resolver::resolve_credential`),
    /// which is where both the binding's and the source's workspace are known: a
    /// pool member whose source belongs to another workspace is skipped, and an
    /// `Exact` binding on a cross-workspace source fails closed (`SourceMissing`).
    ///
    /// Audit of every `CredentialRepo::get` caller confirms none performs an
    /// unfenced cross-workspace *secret materialization*:
    /// - `awaken-runtime-host::PrefetchedSourceLookup::for_defs` (managed MCP)
    ///   prefetches by id, then materializes only through the fenced resolver;
    /// - the admin-config-api routes (`get_credential`, `archive_credential`,
    ///   `put_mcp_server` existence-check) are IAM-gated by-id management ops that
    ///   read a **secret-free** row / mutate status — they never materialize a
    ///   secret; the sealed material stays behind `SecretStore`.
    ///
    /// So keeping `get` unscoped is correct: it is the shared read primitive, and
    /// the tenant fence lives at the resolution seam. This test pins that primitive.
    #[tokio::test]
    async fn get_is_an_unscoped_by_id_primitive_fenced_at_resolution() {
        let store = InMemorySecretStore::new();
        let repo = InMemoryCredentialRepo::new();
        let owned = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws-owner".into(),
                kind: CredentialKind::Env,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY".into()),
                secret: None,
                oauth_command: None,
            },
            &store,
            &repo,
        )
        .await
        .unwrap();

        // `list` for an unrelated workspace correctly hides the row...
        assert_eq!(repo.list("ws-other").await.unwrap().len(), 0);
        // ...but a direct `get` with the id returns it regardless of workspace.
        let cross_read = repo.get(&owned.id).await.unwrap();
        assert_eq!(cross_read.workspace_id, "ws-owner");
    }

    #[tokio::test]
    async fn a_missing_source_or_pool_is_not_found() {
        let repo = InMemoryCredentialRepo::new();
        assert!(matches!(
            repo.get(&CredentialSourceId("cred:absent".into())).await,
            Err(CredentialError::SourceNotFound(id)) if id == "cred:absent"
        ));
        assert!(matches!(
            repo.get_pool(&CredentialPoolId("pool:absent".into())).await,
            Err(CredentialError::PoolNotFound(id)) if id == "pool:absent"
        ));
    }
}
