use super::CatalogModelPublicationResolver;
use awaken_config_resolver::InferenceProfileStore;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_model_catalog::ProviderCatalog;
use awaken_model_catalog::repo::CatalogRepo;
use std::sync::Arc;

#[derive(Clone)]
pub(super) enum CatalogSource {
    Static(ProviderCatalog),
    Live(Arc<dyn CatalogRepo>),
}

impl CatalogSource {
    pub(super) async fn snapshot(&self) -> Result<ProviderCatalog, String> {
        match self {
            Self::Static(catalog) => Ok(catalog.clone()),
            Self::Live(repo) => repo.snapshot().await.map_err(|error| error.to_string()),
        }
    }
}

impl CatalogModelPublicationResolver {
    /// Resolve against a frozen catalog snapshot. This is useful for deterministic
    /// tests; production composition should use [`Self::from_repo`].
    #[must_use]
    pub fn new(catalog: ProviderCatalog, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            source: CatalogSource::Static(catalog),
            credentials,
            profiles: None,
            brokered_access_enabled: true,
            workers: None,
            a2a_cards: Arc::new(super::HttpA2aCardDiscovery),
            credential_selection_sequences: Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
        }
    }

    /// Resolve against the live catalog repository at publication time.
    #[must_use]
    pub fn from_repo(repo: Arc<dyn CatalogRepo>, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            source: CatalogSource::Live(repo),
            credentials,
            profiles: None,
            brokered_access_enabled: true,
            workers: None,
            a2a_cards: Arc::new(super::HttpA2aCardDiscovery),
            credential_selection_sequences: Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
        }
    }

    /// Install the authored Profile read port for explicit Profile choices.
    #[must_use]
    pub fn with_profiles(mut self, profiles: Arc<dyn InferenceProfileStore>) -> Self {
        self.profiles = Some(profiles);
        self
    }

    /// Install the sole live Worker observation authority used to freeze an
    /// exact ACP capability profile into BackendOwned publications.
    #[must_use]
    pub fn with_worker_directory(
        mut self,
        workers: Arc<dyn awaken_worker_registry::WorkerDirectory>,
    ) -> Self {
        self.workers = Some(workers);
        self
    }

    /// Replace HTTP Agent Card discovery with one bounded adapter (tests or a
    /// gateway-mediated deployment). Publication remains the sole interpreter.
    #[must_use]
    pub fn with_a2a_card_discovery(mut self, discovery: Arc<dyn super::A2aCardDiscovery>) -> Self {
        self.a2a_cards = discovery;
        self
    }

    /// Select whether brokered Offering/Profile pairs may enter a new immutable
    /// publication.
    #[must_use]
    pub fn with_brokered_access(mut self, enabled: bool) -> Self {
        self.brokered_access_enabled = enabled;
        self
    }
}
