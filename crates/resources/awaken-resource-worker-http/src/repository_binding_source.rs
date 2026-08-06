//! Local Repository binding adapter over the authoritative Resource Catalog.

use std::sync::Arc;

use awaken_resource_contract::{ConfigVersion, ResourceBindingValidator};
use awaken_run_ingress_contract::RunClaim;
use awaken_run_ingress_contract::{RepositoryBindingVerifier, RepositoryBindingVerifierError};

pub struct CatalogRepositoryBindingVerifier {
    validator: Arc<dyn ResourceBindingValidator>,
}

impl CatalogRepositoryBindingVerifier {
    #[must_use]
    pub fn new(validator: Arc<dyn ResourceBindingValidator>) -> Self {
        Self { validator }
    }
}

#[async_trait::async_trait]
impl RepositoryBindingVerifier for CatalogRepositoryBindingVerifier {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        _claim: Option<&RunClaim>,
    ) -> Result<(), RepositoryBindingVerifierError> {
        self.validator
            .validate_repository_binding(workspace_id, repository_id, config_version)
            .map_err(|error| RepositoryBindingVerifierError::new(error.to_string()))
    }
}
