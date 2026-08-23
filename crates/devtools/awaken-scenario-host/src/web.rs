//! Deterministic Web fixture assembled through the production Managed process.

use std::sync::Arc;

use awaken_ext_builtin_tools::{
    WebSearchCredentialRequirement, WebSearchProvider, WebSearchProviderDescriptor,
    WebSearchProviderRegistry, WebSearchRequest, WebSearchResult,
};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::tool::ToolError;
use awaken_runtime_contract::{CredentialMaterial, CredentialUsage};
use axum::Router;
use serde_json::json;

use crate::models::WebToolDrivingModel;

const SCENARIO_WEB_SEARCH_PROVIDER_ID: &str = "scenario-web-search";
const SCENARIO_WEB_SEARCH_SECRET: &str = "scenario-search-secret";

struct ScenarioWebSearchProvider;

#[async_trait::async_trait]
impl WebSearchProvider for ScenarioWebSearchProvider {
    fn descriptor(&self) -> WebSearchProviderDescriptor {
        WebSearchProviderDescriptor {
            id: SCENARIO_WEB_SEARCH_PROVIDER_ID.into(),
            label: "Scenario paid Web search".into(),
            credential: WebSearchCredentialRequirement::Exact(CredentialUsage::HttpHeader {
                name: "X-Scenario-Web-Key".into(),
                scheme: None,
            }),
            options_schema: json!({
                "type": "object",
                "additionalProperties": false,
            }),
        }
    }

    async fn search(
        &self,
        request: WebSearchRequest,
        credential: Option<&CredentialMaterial>,
    ) -> Result<Vec<WebSearchResult>, ToolError> {
        let secret_matches = credential
            .and_then(|material| material.single_secret().ok())
            .is_some_and(|secret| secret.expose_secret() == SCENARIO_WEB_SEARCH_SECRET);
        if !secret_matches {
            return Err(ToolError::Execution(
                "scenario web-search credential mismatch".into(),
            ));
        }
        let location = request.user_location.as_ref().ok_or_else(|| {
            ToolError::Execution("scenario web-search location is missing".into())
        })?;
        let location_matches = location.city.as_deref() == Some("Shanghai")
            && location.country.as_deref() == Some("CN")
            && location.region.as_deref() == Some("Shanghai")
            && location.timezone.as_deref() == Some("Asia/Shanghai");
        if !location_matches {
            return Err(ToolError::Execution(
                "scenario web-search location mismatch".into(),
            ));
        }
        let marker = "credential-ok;location=Shanghai|CN|Shanghai|Asia/Shanghai";
        Ok(vec![
            WebSearchResult {
                title: request.query,
                url: "https://allowed.test/result".into(),
                snippet: marker.into(),
            },
            WebSearchResult {
                title: "filtered fixture result".into(),
                url: "https://blocked.test/result".into(),
                snippet: "must-not-reach-the-model".into(),
            },
        ])
    }
}

/// Build the one AllInOne Web scenario. The registry enters `ProcessStartup`
/// before router assembly, so Control validation and Host dispatch share it.
pub async fn build_management_web_router() -> Router {
    let registry = WebSearchProviderRegistry::try_new([
        Arc::new(ScenarioWebSearchProvider) as Arc<dyn WebSearchProvider>
    ])
    .expect("the scenario Web provider descriptor is valid and unique");
    awaken_cli::build_all_in_one_router_with_host_customizer(
        Arc::new(WebToolDrivingModel),
        ModelBinding::new("default", "management-web", "default"),
        Some(registry),
        |host| host,
    )
    .await
}
