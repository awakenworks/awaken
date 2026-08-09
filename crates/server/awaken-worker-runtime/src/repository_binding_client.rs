//! Registered-Worker client for exact Repository binding verification.

use awaken_resource_contract::ConfigVersion;
use awaken_run_ingress_contract::{
    REPOSITORY_BINDING_PATH, RepositoryBindingRequest, RepositoryBindingVerifier,
    RepositoryBindingVerifierError, RunClaim,
};
use awaken_worker_transport_security::WorkerUpstream;

#[derive(Clone)]
pub struct HttpRepositoryBindingVerifier {
    upstream: WorkerUpstream,
}

impl HttpRepositoryBindingVerifier {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self { upstream }
    }
}

#[async_trait::async_trait]
impl RepositoryBindingVerifier for HttpRepositoryBindingVerifier {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        claim: Option<&RunClaim>,
    ) -> Result<(), RepositoryBindingVerifierError> {
        let claim = claim.ok_or_else(|| {
            RepositoryBindingVerifierError::new(
                "remote Repository verification requires a dispatch claim",
            )
        })?;
        if workspace_id.trim().is_empty() || repository_id.trim().is_empty() {
            return Err(RepositoryBindingVerifierError::new(
                "Workspace and Repository identities must not be empty",
            ));
        }
        let request = self
            .upstream
            .http_client()
            .post(format!(
                "{}{REPOSITORY_BINDING_PATH}",
                self.upstream.base_url()
            ))
            .json(&RepositoryBindingRequest {
                claim: claim.clone(),
                identity: self.upstream.worker_identity().cloned(),
                workspace_id: workspace_id.to_owned(),
                repository_id: repository_id.to_owned(),
                config_version,
            });
        let request = self
            .upstream
            .authorize_request("POST", REPOSITORY_BINDING_PATH, request)
            .map_err(RepositoryBindingVerifierError::new)?;
        let response = request
            .send()
            .await
            .map_err(|error| RepositoryBindingVerifierError::new(error.to_string()))?;
        if response.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(RepositoryBindingVerifierError::new(format!(
                "Repository binding authority returned HTTP {}",
                response.status()
            )));
        }
        Ok(())
    }
}
