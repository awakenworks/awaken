//! The catalog repository port (ADR-0043) — CRUD over the catalog aggregates,
//! the write path the admin config API drives. The in-memory backend here is P0;
//! a sqlite/postgres backend over the `catalog` migration scope follows. Every
//! publish validates reference integrity via [`ProviderCatalog::validate`]
//! (fail-closed, G22).

use std::sync::Mutex;

use crate::{
    CatalogError, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderCatalog,
    ProviderId, ValidCatalog,
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

/// In-memory [`CatalogRepo`] (dev / tests / single-machine default). Its stored
/// state is a [`ValidCatalog`], so reference integrity is an invariant of what is
/// held — every mutation re-parses through the construction boundary and `snapshot`
/// hands back the checked inner with no read-time re-validation.
pub struct InMemoryCatalogRepo {
    inner: Mutex<ValidCatalog>,
}

impl Default for InMemoryCatalogRepo {
    fn default() -> Self {
        Self {
            // An empty catalog trivially satisfies reference integrity.
            inner: Mutex::new(
                ValidCatalog::parse(ProviderCatalog::default()).expect("empty catalog is valid"),
            ),
        }
    }
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
        let mut guard = self.inner.lock().expect("catalog mutex");
        let mut next = guard.get().clone();
        next.providers.insert(provider.id.0.clone(), provider);
        // Re-parse through the construction boundary; on rejection the store is
        // never reassigned, so a rejected write leaves no trace (fail-closed).
        *guard = ValidCatalog::parse(next)?;
        Ok(())
    }

    async fn put_endpoint(&self, endpoint: ProtocolEndpoint) -> Result<(), RepoError> {
        let mut guard = self.inner.lock().expect("catalog mutex");
        if !guard
            .get()
            .providers
            .contains_key(endpoint.provider_id.as_str())
        {
            return Err(RepoError::ProviderNotFound(endpoint.provider_id.0.clone()));
        }
        let mut next = guard.get().clone();
        next.endpoints.insert(endpoint.id.0.clone(), endpoint);
        *guard = ValidCatalog::parse(next)?;
        Ok(())
    }

    async fn put_offering(&self, offering: Offering) -> Result<(), RepoError> {
        let mut guard = self.inner.lock().expect("catalog mutex");
        if !guard
            .get()
            .endpoints
            .contains_key(offering.protocol_endpoint_id.as_str())
        {
            return Err(RepoError::EndpointNotFound(
                offering.protocol_endpoint_id.0.clone(),
            ));
        }
        let mut next = guard.get().clone();
        // Upsert on the offering's primary key `(model_id, protocol_endpoint_id)`
        // — the durable backends key their row on exactly this pair (schema PK,
        // `ON CONFLICT … DO UPDATE`), so an in-memory push would diverge by
        // accumulating a duplicate row instead of replacing it.
        match next.offerings.iter_mut().find(|o| {
            o.model_id == offering.model_id
                && o.protocol_endpoint_id == offering.protocol_endpoint_id
        }) {
            Some(existing) => *existing = offering,
            None => next.offerings.push(offering),
        }
        // `ValidCatalog::parse` IS the write-time integrity check: if the new
        // offering breaks an invariant, parse fails and the stored `ValidCatalog`
        // is never reassigned, so a rejected write leaves no trace (fail-closed).
        *guard = ValidCatalog::parse(next)?;
        Ok(())
    }

    async fn get_provider(&self, id: &ProviderId) -> Result<Provider, RepoError> {
        self.inner
            .lock()
            .expect("catalog mutex")
            .get()
            .providers
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| RepoError::ProviderNotFound(id.0.clone()))
    }

    async fn get_endpoint(&self, id: &ProtocolEndpointId) -> Result<ProtocolEndpoint, RepoError> {
        self.inner
            .lock()
            .expect("catalog mutex")
            .get()
            .endpoints
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| RepoError::EndpointNotFound(id.0.clone()))
    }

    async fn snapshot(&self) -> Result<ProviderCatalog, RepoError> {
        // The stored catalog is a `ValidCatalog`, so its integrity already holds —
        // hand back the inner without re-validating (the read-path check moved to
        // the write-time construction boundary above).
        Ok(self.inner.lock().expect("catalog mutex").get().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiDialect;

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
            dialect: ApiDialect::AnthropicMessages,
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
            dialect: ApiDialect::AnthropicMessages,
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
            dialect: ApiDialect::OpenAiChat, // endpoint is AnthropicMessages
            upstream_model: None,
        };
        assert!(repo.put_offering(bad).await.is_err());
    }

    #[test]
    fn repo_error_display_and_from_catalog_error() {
        assert_eq!(
            RepoError::ProviderNotFound("anthropic".into()).to_string(),
            "provider `anthropic` not found"
        );
        assert_eq!(
            RepoError::EndpointNotFound("ep1".into()).to_string(),
            "endpoint `ep1` not found"
        );
        // `#[from]` lifts a CatalogError, and `#[error(transparent)]` forwards its
        // Display verbatim (so the admin API surfaces the invariant message).
        let lifted: RepoError = CatalogError::OfferingEndpointUnknown {
            model: "m".into(),
            endpoint: "ghost".into(),
        }
        .into();
        assert!(matches!(lifted, RepoError::Invariant(_)));
        assert_eq!(
            lifted.to_string(),
            "offering `m` references unknown endpoint `ghost`"
        );
    }

    #[tokio::test]
    async fn snapshot_of_an_empty_repo_is_a_valid_empty_catalog() {
        let repo = InMemoryCatalogRepo::new();
        let snap = repo.snapshot().await.unwrap();
        assert!(snap.providers.is_empty());
        assert!(snap.endpoints.is_empty());
        assert!(snap.offerings.is_empty());
    }

    #[tokio::test]
    async fn put_offering_is_upsert_on_its_key_not_a_duplicate_push() {
        let repo = InMemoryCatalogRepo::new();
        repo.put_provider(provider()).await.unwrap();
        repo.put_endpoint(endpoint()).await.unwrap();
        let mut first = Offering {
            model_id: "claude-opus-4-8".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: Some("v1".into()),
        };
        repo.put_offering(first.clone()).await.unwrap();
        first.upstream_model = Some("v2".into());
        repo.put_offering(first).await.unwrap();
        let snap = repo.snapshot().await.unwrap();
        // Regression: an in-memory push accumulated a duplicate; the durable
        // backends upsert on `(model_id, protocol_endpoint_id)`. One row, latest data.
        assert_eq!(snap.offerings.len(), 1);
        assert_eq!(snap.offerings[0].upstream_model.as_deref(), Some("v2"));
    }
}
