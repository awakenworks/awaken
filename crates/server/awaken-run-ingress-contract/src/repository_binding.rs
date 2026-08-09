//! Claim-bound Repository binding verification contract and wire value.

use awaken_resource_contract::ConfigVersion;
use serde::{Deserialize, Serialize};

use crate::{RunClaim, WorkerIdentity};

pub const REPOSITORY_BINDING_PATH: &str = "/v1/worker/resources/repositories/verify";

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("Repository binding verifier: {0}")]
pub struct RepositoryBindingVerifierError(String);

impl RepositoryBindingVerifierError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[async_trait::async_trait]
pub trait RepositoryBindingVerifier: Send + Sync {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        claim: Option<&RunClaim>,
    ) -> Result<(), RepositoryBindingVerifierError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBindingRequest {
    pub claim: RunClaim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<WorkerIdentity>,
    pub workspace_id: String,
    pub repository_id: String,
    pub config_version: ConfigVersion,
}
