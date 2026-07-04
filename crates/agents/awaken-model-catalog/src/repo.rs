//! The catalog repository port (ADR-0043) — CRUD over the catalog aggregates,
//! the write path the admin config API drives. The in-memory backend here is P0;
//! a sqlite/postgres backend over the `catalog` migration scope follows. Every
//! publish validates reference integrity via [`ProviderCatalog::validate`]
//! (fail-closed, G22).

use std::sync::Mutex;

use crate::{
    CatalogError, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderCatalog,
    ProviderId,
};

/// A catalog write/read failure.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("provider `{0}` not found")]
    ProviderNotFound(String),
    #[error("endpoint `{0}` not found")]
    EndpointNotFound(String),
    #[error(transparent)]
    Invariant(#[from] CatalogError),
}

/// The catalog store port. Consumers (admin API, resolver) depend on this, not on
/// a concrete backend, so the domain is split/merge-friendly (its own scope).
#[async_trait::async_trait]
pub trait CatalogRepo: Send + Sync {
    async fn put_provider(&self, provider: Provider) -> Result<(), RepoError>;
    async fn put_endpoint(&self, endpoint: ProtocolEndpoint) -> Result<(), RepoError>;
    async fn put_offering(&self, offering: Offering) -> Result<(), RepoError>;
    async fn get_provider(&self, id: &ProviderId) -> Result<Provider, RepoError>;
    async fn get_endpoint(&self, id: &ProtocolEndpointId) -> Result<ProtocolEndpoint, RepoError>;
    /// The full catalog projection (what the resolver queries). Validated.
    async fn snapshot(&self) -> Result<ProviderCatalog, RepoError>;
}

/// In-memory [`CatalogRepo`] (dev / tests / single-machine default).
#[derive(Default)]
pub struct InMemoryCatalogRepo {
    inner: Mutex<ProviderCatalog>,
}

impl InMemoryCatalogRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl CatalogRepo for InMemoryCatalogRepo {
    async fn put_provider(&self, provider: Provider) -> Result<(), RepoError> {
        self.inner
            .lock()
            .expect("catalog mutex")
            .providers
            .insert(provider.id.0.clone(), provider);
        Ok(())
    }

    async fn put_endpoint(&self, endpoint: ProtocolEndpoint) -> Result<(), RepoError> {
        let mut cat = self.inner.lock().expect("catalog mutex");
        if !cat.providers.contains_key(endpoint.provider_id.as_str()) {
            return Err(RepoError::ProviderNotFound(endpoint.provider_id.0.clone()));
        }
        cat.endpoints.insert(endpoint.id.0.clone(), endpoint);
        Ok(())
    }

    async fn put_offering(&self, offering: Offering) -> Result<(), RepoError> {
        let mut cat = self.inner.lock().expect("catalog mutex");
        if !cat
            .endpoints
            .contains_key(offering.protocol_endpoint_id.as_str())
        {
            return Err(RepoError::EndpointNotFound(
                offering.protocol_endpoint_id.0.clone(),
            ));
        }
        cat.offerings.push(offering);
        // Re-validate the whole catalog on each mutation (fail-closed); roll back
        // the just-added offering if it breaks an invariant, so a rejected write
        // leaves no trace (else a bad offering would poison later snapshots).
        if let Err(err) = cat.validate() {
            cat.offerings.pop();
            return Err(err.into());
        }
        Ok(())
    }

    async fn get_provider(&self, id: &ProviderId) -> Result<Provider, RepoError> {
        self.inner
            .lock()
            .expect("catalog mutex")
            .providers
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| RepoError::ProviderNotFound(id.0.clone()))
    }

    async fn get_endpoint(&self, id: &ProtocolEndpointId) -> Result<ProtocolEndpoint, RepoError> {
        self.inner
            .lock()
            .expect("catalog mutex")
            .endpoints
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| RepoError::EndpointNotFound(id.0.clone()))
    }

    async fn snapshot(&self) -> Result<ProviderCatalog, RepoError> {
        let cat = self.inner.lock().expect("catalog mutex").clone();
        cat.validate()?;
        Ok(cat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelApiCompat;

    fn provider() -> Provider {
        Provider {
            id: ProviderId::new("anthropic"),
            slug: "anthropic".into(),
            display_name: "Anthropic".into(),
            version: 1,
        }
    }
    fn endpoint() -> ProtocolEndpoint {
        ProtocolEndpoint {
            id: ProtocolEndpointId::new("ep1"),
            provider_id: ProviderId::new("anthropic"),
            flavor: ModelApiCompat::AnthropicMessages,
            base_url: None,
            timeout_secs: 300,
            display_name: "prod".into(),
            version: 1,
        }
    }

    #[tokio::test]
    async fn crud_round_trip_and_snapshot() {
        let repo = InMemoryCatalogRepo::new();
        repo.put_provider(provider()).await.unwrap();
        repo.put_endpoint(endpoint()).await.unwrap();
        repo.put_offering(Offering {
            model_id: "claude-opus-4-8".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            flavor: ModelApiCompat::AnthropicMessages,
            upstream_model: None,
        })
        .await
        .unwrap();

        assert_eq!(
            repo.get_provider(&ProviderId::new("anthropic"))
                .await
                .unwrap()
                .slug,
            "anthropic"
        );
        let snap = repo.snapshot().await.unwrap();
        assert_eq!(snap.offerings.len(), 1);
    }

    #[tokio::test]
    async fn endpoint_needs_existing_provider() {
        let repo = InMemoryCatalogRepo::new();
        assert!(matches!(
            repo.put_endpoint(endpoint()).await,
            Err(RepoError::ProviderNotFound(_))
        ));
    }

    #[tokio::test]
    async fn offering_flavor_mismatch_fails_closed() {
        let repo = InMemoryCatalogRepo::new();
        repo.put_provider(provider()).await.unwrap();
        repo.put_endpoint(endpoint()).await.unwrap();
        let bad = Offering {
            model_id: "m".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            flavor: ModelApiCompat::OpenAiChat, // endpoint is AnthropicMessages
            upstream_model: None,
        };
        assert!(repo.put_offering(bad).await.is_err());
    }
}
