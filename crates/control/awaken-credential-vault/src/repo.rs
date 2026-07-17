//! The credential source repository port (ADR-0043) — stores the **secret-free**
//! [`CredentialSource`] rows (the sealed material lives behind [`SecretStore`], a
//! separate port). Its own `credential` migration scope is what lets the whole
//! domain be split into its own database/service (blast-radius isolation).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{
    CredentialCreateParams, CredentialError, CredentialPool, CredentialPoolId, CredentialSource,
    CredentialSourceId, SecretStore, create_source,
};

/// The credential-source store port. Secret-free rows only. Pools are stored here
/// too (they are secret-free groupings of sources the resolver fails over across).
#[async_trait::async_trait]
pub trait CredentialRepo: Send + Sync {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError>;
    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError>;
    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError>;

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError>;
    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError>;
    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError>;
}

/// In-memory [`CredentialRepo`] (dev / tests / single-machine default).
#[derive(Default)]
pub struct InMemoryCredentialRepo {
    rows: Mutex<HashMap<String, CredentialSource>>,
    pools: Mutex<HashMap<String, CredentialPool>>,
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
        self.rows
            .lock()
            .expect("cred rows")
            .insert(source.id.0.clone(), source);
        Ok(())
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        self.rows
            .lock()
            .expect("cred rows")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::SourceNotFound(id.0.clone()))
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        Ok(self
            .rows
            .lock()
            .expect("cred rows")
            .values()
            .filter(|s| s.workspace_id == workspace_id)
            .cloned()
            .collect())
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError> {
        self.pools
            .lock()
            .expect("cred pools")
            .insert(pool.id.0.clone(), pool);
        Ok(())
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        self.pools
            .lock()
            .expect("cred pools")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| CredentialError::PoolNotFound(id.0.clone()))
    }

    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError> {
        Ok(self
            .pools
            .lock()
            .expect("cred pools")
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
    let source = create_source(params, store).await?;
    repo.put(source.clone()).await?;
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CredentialKind, InMemorySecretStore, materialize};
    use awaken_agent_contract::RedactedString;

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
