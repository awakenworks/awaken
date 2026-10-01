// @formal evidence model_binding.constructor_and_wire_share_one_admission
//! Cloud catalog transport. Control alone projects its public model catalog.
//!
//! The broker owns catalog contents and route revisions. This adapter reads
//! that catalog once per refresh; Control stamps the successful observation
//! on its process clock when it projects the result.

use async_trait::async_trait;

use super::{
    BrokeredInferenceError, BrokeredModel, BrokeredModelCatalogClient, HttpBrokeredInferenceClient,
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
