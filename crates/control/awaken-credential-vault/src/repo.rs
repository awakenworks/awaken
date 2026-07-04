//! The credential source repository port (ADR-0043) — stores the **secret-free**
//! [`CredentialSource`] rows (the sealed material lives behind [`SecretStore`], a
//! separate port). Its own `credential` migration scope is what lets the whole
//! domain be split into its own database/service (blast-radius isolation).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{
    CredentialCreateParams, CredentialError, CredentialSource, CredentialSourceId, SecretStore,
    create_source,
};

/// The credential-source store port. Secret-free rows only.
#[async_trait::async_trait]
pub trait CredentialRepo: Send + Sync {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError>;
    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError>;
    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError>;
}

/// In-memory [`CredentialRepo`] (dev / tests / single-machine default).
#[derive(Default)]
pub struct InMemoryCredentialRepo {
    rows: Mutex<HashMap<String, CredentialSource>>,
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
}
