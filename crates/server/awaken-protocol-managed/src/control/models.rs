//! The Models API (`/v1/models`) the official `@anthropic-ai/sdk` drives via
//! GA `client.models` or `client.beta.models`. Both report the same executable
//! inventory through their distinct `ModelInfo` / `BetaModelInfo` projections. The list is a plain
//! [`Page`](https://docs.anthropic.com/en/api/models-list) (`data` + `has_more` +
//! `first_id` / `last_id`), NOT the vault family's cursor page.
//!
//! The executable Agent inventory is injected by the process entry point. Production uses a live,
//! Workspace-aware executable projection; [`default_models`] remains only for
//! bare-host fixtures that have no configuration plane.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use serde::Serialize;

use crate::common::headers::ManagedCapability;
use crate::common::scope::RequiredWorkspaceScope;
use crate::resources::flavor::{ManagedResourceApiSurface, resource_api_surface};
use crate::types::{ErrorResponse, Page};

/// Deterministic release timestamp stamped on every model (the wire needs a valid
/// RFC-3339 `created_at`; a reproducible constant keeps tests stable).
const CREATED_AT: &str = "2026-01-01T00:00:00Z";

/// One model the deployment can serve, projected to the `BetaModelInfo` core
/// fields. `display_name` is human-facing; `id` is what an agent names as its
/// `model`.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub display_name: String,
    /// The model's published context window (max input tokens) — the intrinsic
    /// attribute compaction and the ACP auto-compact window derive their budget from.
    /// `None` when the deployment doesn't publish it.
    pub context_window: Option<u32>,
    /// Hard ceiling on a single response's output tokens, when published.
    pub max_output_tokens: Option<u32>,
}

impl ModelEntry {
    pub fn new(id: &str, display_name: &str) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            context_window: None,
            max_output_tokens: None,
        }
    }

    /// Attach the model's published token limits (context window + output ceiling).
    #[must_use]
    pub fn with_limits(mut self, context_window: u32, max_output_tokens: u32) -> Self {
        self.context_window = Some(context_window);
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    /// The GA/Beta Model projection. `max_input_tokens`/`max_tokens` carry the
    /// model's published context window / output ceiling (or `null` when unknown);
    /// Beta alone includes an empty `allowed_fallback_models` list because
    /// fallbacks remain a gateway concern.
    fn project(&self, surface: ManagedResourceApiSurface) -> ModelInfo<'_> {
        ModelInfo {
            id: &self.id,
            kind: "model",
            display_name: &self.display_name,
            created_at: CREATED_AT,
            allowed_fallback_models: (surface != ManagedResourceApiSurface::Ga).then(Vec::new),
            capabilities: None,
            max_input_tokens: self.context_window,
            max_tokens: self.max_output_tokens,
        }
    }
}

#[derive(Debug, Serialize)]
struct ModelInfo<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    display_name: &'a str,
    created_at: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_fallback_models: Option<Vec<String>>,
    capabilities: Option<()>,
    max_input_tokens: Option<u32>,
    max_tokens: Option<u32>,
}

fn model_error(
    status: StatusCode,
    kind: &'static str,
    message: impl Into<String>,
) -> axum::response::Response {
    (status, axum::Json(ErrorResponse::new(kind, message))).into_response()
}

#[derive(Clone)]
enum AvailableModels {
    Fixed(Arc<Vec<ModelEntry>>),
    ExecutableAgents(Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>),
}

impl AvailableModels {
    async fn in_workspace(&self, workspace_id: &str) -> Result<Vec<ModelEntry>, String> {
        match self {
            Self::Fixed(models) => Ok(models.as_ref().clone()),
            Self::ExecutableAgents(registrations) => {
                awaken_executable_agent_contract::current_model_references(
                    registrations.as_ref(),
                    workspace_id,
                )
                .await
                .map(|references| {
                    references
                        .into_iter()
                        .map(|model_reference| ModelEntry::new(&model_reference, &model_reference))
                        .collect()
                })
                .map_err(|error| error.to_string())
            }
        }
    }
}

/// Deterministic model fixture for bare-host tests without a configuration plane.
#[must_use]
pub fn default_models() -> Vec<ModelEntry> {
    vec![
        // Published context windows (max input tokens) — the intrinsic attribute the
        // compaction window derives from; 200K is the standard Claude context.
        ModelEntry::new("claude-opus-4-8", "Claude Opus 4.8").with_limits(200_000, 64_000),
        ModelEntry::new("claude-sonnet-5", "Claude Sonnet 5").with_limits(200_000, 64_000),
        ModelEntry::new("claude-haiku-4-5-20251001", "Claude Haiku 4.5")
            .with_limits(200_000, 32_000),
        ModelEntry::new("claude-fable-5", "Fable 5").with_limits(200_000, 32_000),
    ]
}

/// Mount the Models API over a fixed model directory.
pub fn models_router(models: Arc<Vec<ModelEntry>>) -> Router {
    models_router_for(AvailableModels::Fixed(models))
}

/// Mount the Models API over the live Workspace-aware executable Agent inventory.
pub fn models_router_with_inventory(
    registrations: Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>,
) -> Router {
    models_router_for(AvailableModels::ExecutableAgents(registrations))
}

fn models_router_for(models: AvailableModels) -> Router {
    Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/models/{*id}", get(get_model))
        .with_state(models)
}

/// `GET /v1/models` — the full directory as a GA/Beta model `Page` (one page:
/// `has_more:false`). `first_id` / `last_id` bracket the page for the SDK's
/// id-cursor paginator; both `null` when the directory is empty.
async fn list_models(
    State(available): State<AvailableModels>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> axum::response::Response {
    let surface =
        match resource_api_surface(raw.as_deref(), &headers, ManagedCapability::ManagedAgents) {
            Ok(surface) => surface,
            Err(message) => {
                return model_error(StatusCode::BAD_REQUEST, "invalid_request_error", message);
            }
        };
    let Ok(models) = available.in_workspace(&workspace).await else {
        return model_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "model directory unavailable",
        );
    };
    let data: Vec<_> = models.iter().map(|model| model.project(surface)).collect();
    let first_id = models.first().map(|m| m.id.clone());
    let last_id = models.last().map(|m| m.id.clone());
    (
        StatusCode::OK,
        axum::Json(Page::new(data, false, first_id, last_id)),
    )
        .into_response()
}

/// `GET /v1/models/{id}` — one model in the selected GA/Beta projection, or `404`. Doubles as the
/// SDK's alias-resolution endpoint (an exact id here resolves to itself).
async fn get_model(
    State(available): State<AvailableModels>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> impl IntoResponse {
    let surface =
        match resource_api_surface(raw.as_deref(), &headers, ManagedCapability::ManagedAgents) {
            Ok(surface) => surface,
            Err(message) => {
                return model_error(StatusCode::BAD_REQUEST, "invalid_request_error", message);
            }
        };
    let Ok(models) = available.in_workspace(&workspace).await else {
        return model_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "model directory unavailable",
        );
    };
    match models.iter().find(|m| m.id == id) {
        Some(entry) => (StatusCode::OK, axum::Json(entry.project(surface))).into_response(),
        None => model_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            format!("model `{id}` not found"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    struct UnavailableInventory;

    #[async_trait::async_trait]
    impl awaken_executable_agent_contract::ExecutableAgentInventorySource for UnavailableInventory {
        async fn current_registrations(
            &self,
            _workspace_id: &str,
        ) -> Result<
            Vec<awaken_executable_agent_contract::ExecutableAgentRegistration>,
            awaken_executable_agent_contract::ExecutableAgentRegistrationError,
        > {
            Err(
                awaken_executable_agent_contract::ExecutableAgentRegistrationError::Unavailable(
                    "refresh failed".into(),
                ),
            )
        }
    }

    #[test]
    fn default_models_project_one_ga_and_beta_contract() {
        // Cause/effect graph: C1 GA/Beta flavor; C2 known token limits; C3 unknown
        // capabilities. Effects: E1 both emit the seven shared ModelInfo fields;
        // E2 Beta alone emits allowed_fallback_models; E3 limits are numeric and
        // capabilities null. Constraint: flavor never selects inventory/state.
        // Decision rules: R1 C1=GA+C2+C3->E1+E3; R2 C1=Beta+C2+C3->E1+E2+E3.
        let models = default_models();
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
        for m in &models {
            for (rule, surface, fields, fallback) in [
                ("R1", ManagedResourceApiSurface::Ga, 7, false),
                ("R2", ManagedResourceApiSurface::CapabilityBeta, 8, true),
                ("R3", ManagedResourceApiSurface::QueryBeta, 8, true),
            ] {
                let v = serde_json::to_value(m.project(surface)).unwrap();
                assert_eq!(v.as_object().unwrap().len(), fields, "{rule}");
                assert_eq!(v["type"], "model", "{rule}");
                assert_eq!(v["id"], m.id, "{rule}");
                assert!(v["display_name"].is_string(), "{rule}");
                assert!(v["created_at"].is_string(), "{rule}");
                assert_eq!(
                    v.get("allowed_fallback_models").is_some(),
                    fallback,
                    "{rule}"
                );
                assert!(v["capabilities"].is_null(), "{rule}");
                assert_eq!(v["max_input_tokens"], 200_000, "{rule}");
                assert!(v["max_tokens"].as_u64().is_some(), "{rule}");
            }
        }
    }

    #[tokio::test]
    async fn models_route_uses_the_canonical_header_flavor_without_splitting_inventory() {
        // Causes: C1 no Managed beta selector; C2 the canonical Managed beta
        // header; C3 `beta=true` without a capability; C4 every request
        // addresses the same fixed inventory. Effects:
        // E1 C1 returns GA ModelInfo without allowed_fallback_models; E2 C2
        // and C3 return BetaModelInfo with it; E3 all retain the same model id.
        // Constraint: transport selection changes projection only. Decision
        // table: R1 C1+C4->E1+E3; R2 C2+C4->E2+E3;
        // R3 C3+C4->E2+E3.
        let app = models_router(Arc::new(vec![ModelEntry::new("model-a", "Model A")])).layer(
            axum::Extension(awaken_tenancy::WorkspaceScope("default".into())),
        );
        for (rule, uri, capability, fallback) in [
            ("R1", "/v1/models", false, false),
            ("R2", "/v1/models", true, true),
            ("R3", "/v1/models?beta=true", false, true),
        ] {
            let mut request = axum::http::Request::builder().uri(uri);
            if capability {
                request = request.header("anthropic-beta", ManagedCapability::ManagedAgents.beta());
            }
            let response = app
                .clone()
                .oneshot(request.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{rule}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["data"][0]["id"], "model-a", "{rule}");
            assert_eq!(
                value["data"][0].get("allowed_fallback_models").is_some(),
                fallback,
                "{rule}"
            );
        }
    }

    #[tokio::test]
    async fn retrieve_accepts_complete_slash_bearing_model_id() {
        // Causes: C1 the registered model id contains provider/model path
        // separators; C2 the request supplies that complete id through the
        // wildcard route. Effect: E1 retrieval matches the exact id and returns
        // its BetaModelInfo document. Decision rule R1=C1&&C2 -> E1. This test
        // belongs to the HTTP adapter; directory implementations need only
        // supply opaque model ids.
        let id = "provider/claude/model-a";
        let app = models_router(Arc::new(vec![ModelEntry::new(id, "Model A")])).layer(
            axum::Extension(awaken_tenancy::WorkspaceScope("default".into())),
        );
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v1/models/{id}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK, "R1");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["id"], id, "R1");
    }

    #[tokio::test]
    async fn live_model_inventory_unavailability_fails_list_and_retrieve_closed() {
        // Causes: C1 live inventory succeeds/fails; C2 list/retrieve operation.
        // Effects: E1 failure returns 503 and no stale model document. Constraint
        // K1 fixed fixture models do not use this live source. Decision table:
        // D1 !C1+list=>E1; D2 !C1+retrieve=>E1.
        let app = models_router_with_inventory(Arc::new(UnavailableInventory)).layer(
            axum::Extension(awaken_tenancy::WorkspaceScope("default".into())),
        );
        for (rule, path) in [("D1", "/v1/models"), ("D2", "/v1/models/model-a")] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{rule}");
        }
    }
}
