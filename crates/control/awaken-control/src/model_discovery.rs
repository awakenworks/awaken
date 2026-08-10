//! Control-owned provider model-directory discovery adapter.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_dialect_selects_exact_discovery_adapter() {
        // Causes: the authored endpoint uses one of the five closed API
        // dialects. Effects: Anthropic, OpenAI Chat/Responses, Gemini, and
        // Vertex select their corresponding provider discovery adapters.
        // Decision rules D1-D5 enumerate every enum variant, so a new dialect
        // cannot compile until its discovery behavior is deliberately chosen.
        for (dialect, expected) in [
            (ApiDialect::AnthropicMessages, AdapterKind::Anthropic),
            (ApiDialect::OpenAiChat, AdapterKind::OpenAI),
            (ApiDialect::OpenAiResponses, AdapterKind::OpenAI),
            (ApiDialect::Gemini, AdapterKind::Gemini),
            (ApiDialect::VertexGemini, AdapterKind::Vertex),
        ] {
            let provider_id = awaken_model_catalog::ProviderId::new("provider");
            let endpoint = ProtocolEndpoint {
                id: awaken_model_catalog::ProtocolEndpointId::for_surface(
                    &provider_id,
                    dialect,
                    None,
                ),
                provider_id,
                dialect,
                base_url: None,
                timeout_secs: 30,
                display_name: "Provider".into(),
                version: 1,
            };
            assert_eq!(GenaiModelDiscovery::adapter(&endpoint), expected);
        }
    }
}
