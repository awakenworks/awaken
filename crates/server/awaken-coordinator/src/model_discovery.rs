//! Provider model-directory adapter shared by product composition roots.

use std::sync::Arc;

use awaken_admin_config_api::{ModelCatalogDiscovery, ModelCatalogDiscoveryError};
use awaken_agent_contract::RedactedString;
use awaken_credential_vault::{CredentialSource, SecretStore};
use awaken_model_catalog::{ApiDialect, DiscoveredModel, ProtocolEndpoint};
use awaken_provider_genai::AdapterKind;

pub struct GenaiModelDiscovery {
    secrets: Arc<dyn SecretStore>,
}

impl GenaiModelDiscovery {
    #[must_use]
    pub fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self { secrets }
    }

    fn adapter(endpoint: &ProtocolEndpoint) -> AdapterKind {
        match endpoint.dialect {
            ApiDialect::AnthropicMessages => AdapterKind::Anthropic,
            ApiDialect::OpenAiChat | ApiDialect::OpenAiResponses => AdapterKind::OpenAI,
            ApiDialect::Gemini => AdapterKind::Gemini,
            ApiDialect::VertexGemini => AdapterKind::Vertex,
        }
    }

    async fn discover_ids(
        endpoint: &ProtocolEndpoint,
        secret: &RedactedString,
    ) -> Result<Vec<DiscoveredModel>, ModelCatalogDiscoveryError> {
        awaken_provider_genai::discover_model_ids(
            Self::adapter(endpoint),
            endpoint.base_url.as_deref(),
            secret.expose_secret(),
        )
        .await
        .map(|ids| {
            ids.into_iter()
                .map(|model_id| DiscoveredModel {
                    model_id,
                    upstream_model: None,
                })
                .collect()
        })
        .map_err(|error| ModelCatalogDiscoveryError::Provider(error.to_string()))
    }
}

#[async_trait::async_trait]
impl ModelCatalogDiscovery for GenaiModelDiscovery {
    async fn discover(
        &self,
        endpoint: &ProtocolEndpoint,
        credential: &CredentialSource,
    ) -> Result<Vec<DiscoveredModel>, ModelCatalogDiscoveryError> {
        let secret = awaken_credential_vault::materialize(credential, self.secrets.as_ref())
            .await
            .map_err(|error| {
                ModelCatalogDiscoveryError::CredentialUnavailable(error.to_string())
            })?;
        Self::discover_ids(endpoint, &secret).await
    }

    async fn discover_with_secret(
        &self,
        endpoint: &ProtocolEndpoint,
        secret: &RedactedString,
    ) -> Result<Vec<DiscoveredModel>, ModelCatalogDiscoveryError> {
        Self::discover_ids(endpoint, secret).await
    }
}
