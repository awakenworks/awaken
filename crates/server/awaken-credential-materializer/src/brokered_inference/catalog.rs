// @formal evidence model_binding.constructor_and_wire_share_one_admission
//! Cloud catalog transport and its one public model projection.
//!
//! The broker owns the catalog contents and route revisions. This adapter
//! reads that catalog once per refresh, then stamps the successful observation
//! with the process clock injected into the shared Cloud client.

use async_trait::async_trait;
use awaken_lease_contract::WallClockExt;

use super::{
    BrokeredInferenceError, BrokeredModel, BrokeredModelCatalogClient, HttpBrokeredInferenceClient,
    fetch_brokered_catalog,
};

#[async_trait]
impl BrokeredModelCatalogClient for HttpBrokeredInferenceClient {
    async fn readiness(&self) -> Result<(), BrokeredInferenceError> {
        let response = self
            .send_with_access_token(|token| {
                self.request(reqwest::Method::GET, "/v1/inference/readiness", token)
            })
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Self::classify(response).await)
        }
    }

    async fn list_models(&self) -> Result<Vec<BrokeredModel>, BrokeredInferenceError> {
        #[derive(serde::Deserialize)]
        struct Page {
            data: Vec<BrokeredModel>,
        }
        let response = self
            .send_with_access_token(|token| {
                self.request(reqwest::Method::GET, "/v1/inference/models", token)
            })
            .await?;
        if !response.status().is_success() {
            return Err(Self::classify(response).await);
        }
        response
            .json::<Page>()
            .await
            .map(|page| page.data)
            .map_err(|_| BrokeredInferenceError::TemporarilyUnavailable)
    }
}

#[async_trait]
impl awaken_admin_config_api::BrokeredCatalogDiscovery for HttpBrokeredInferenceClient {
    async fn projection(&self) -> Result<awaken_model_catalog::BrokeredCatalogProjection, String> {
        let models = fetch_brokered_catalog(self)
            .await
            .map_err(|error| error.to_string())?;
        let models = models
            .into_iter()
            .map(|model| {
                let dialect = match model.native_protocol.as_str() {
                    "anthropic_messages" => awaken_model_catalog::ApiDialect::AnthropicMessages,
                    "openai_chat_completions" => awaken_model_catalog::ApiDialect::OpenAiChat,
                    "openai_responses" => awaken_model_catalog::ApiDialect::OpenAiResponses,
                    protocol => {
                        return Err(format!(
                            "Cloud returned unsupported native protocol `{protocol}`"
                        ));
                    }
                };
                Ok(awaken_model_catalog::BrokeredModelProjection {
                    provider_id: model.provider,
                    model_id: model.original_model_id,
                    dialect,
                    context_window: model.context_window,
                    max_output_tokens: model.max_output_tokens,
                    publication_revision: model.route_publication_revision,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let observed_at_unix_ms = self.clock.now_ms();
        Ok(awaken_model_catalog::BrokeredCatalogProjection {
            broker_id: "awaken-cloud".into(),
            control_base_url: self.base_url.clone(),
            models,
            observed_at_unix_ms,
        })
    }
}
