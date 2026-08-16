//! The Managed Models API (`/v1/models`) the official `@anthropic-ai/sdk` drives via
//! `client.beta.models.list` / `.retrieve`. It reports the models this
//! deployment can route to as `BetaModelInfo`. The list is a plain
//! [`Page`](https://docs.anthropic.com/en/api/models-list) (`data` + `has_more` +
//! `first_id` / `last_id`), NOT the vault family's cursor page.
//!
//! The executable Agent inventory is injected by the process entry point. Production uses a live,
//! Workspace-aware executable projection; [`default_models`] remains only for
//! bare-host fixtures that have no configuration plane.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use serde::Serialize;

use crate::common::scope::RequiredWorkspaceScope;
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

    /// The `BetaModelInfo` projection. `max_input_tokens`/`max_tokens` carry the
    /// model's published context window / output ceiling (or `null` when unknown);
    /// `allowed_fallback_models` is an empty list (fallbacks are a gateway concern).
    fn project(&self) -> ModelInfo<'_> {
        ModelInfo {
            id: &self.id,
            kind: "model",
            display_name: &self.display_name,
            created_at: CREATED_AT,
            allowed_fallback_models: Vec::new(),
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
    allowed_fallback_models: Vec<String>,
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

/// `GET /v1/models` — the full directory as a `Page<BetaModelInfo>` (one page:
/// `has_more:false`). `first_id` / `last_id` bracket the page for the SDK's
/// id-cursor paginator; both `null` when the directory is empty.
async fn list_models(
    State(available): State<AvailableModels>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
) -> axum::response::Response {
    let Ok(models) = available.in_workspace(&workspace).await else {
        return model_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "model directory unavailable",
        );
    };
    let data: Vec<_> = models.iter().map(ModelEntry::project).collect();
    let first_id = models.first().map(|m| m.id.clone());
    let last_id = models.last().map(|m| m.id.clone());
    (
        StatusCode::OK,
        axum::Json(Page::new(data, false, first_id, last_id)),
    )
        .into_response()
}

/// `GET /v1/models/{id}` — one model as `BetaModelInfo`, or `404`. Doubles as the
/// SDK's alias-resolution endpoint (an exact id here resolves to itself).
async fn get_model(
    State(available): State<AvailableModels>,
    RequiredWorkspaceScope(workspace): RequiredWorkspaceScope,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Ok(models) = available.in_workspace(&workspace).await else {
        return model_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "model directory unavailable",
        );
    };
    match models.iter().find(|m| m.id == id) {
        Some(entry) => (StatusCode::OK, axum::Json(entry.project())).into_response(),
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

    #[test]
    fn default_models_are_nonempty_and_project_to_beta_model_info() {
        // Cause/effect decision table: known limits -> numeric max fields;
        // unknown capabilities -> explicit null; every entry -> the exact eight
        // BetaModelInfo fields. The typed projection is the sole response owner.
        let models = default_models();
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
        // Every entry projects to the `BetaModelInfo` core shape the SDK decodes.
        for m in &models {
            let v = serde_json::to_value(m.project()).unwrap();
            assert_eq!(v.as_object().unwrap().len(), 8);
            assert_eq!(v["type"], "model");
            assert_eq!(v["id"], m.id);
            assert!(v["display_name"].is_string());
            assert!(v["created_at"].is_string());
            assert!(v["allowed_fallback_models"].is_array());
            // Capabilities stay null (unknown); the token limits now carry the model's
            // published context window / output ceiling.
            assert!(v["capabilities"].is_null());
            assert_eq!(
                v["max_input_tokens"], 200_000,
                "the published context window is reported"
            );
            assert!(
                v["max_tokens"].as_u64().is_some(),
                "the output ceiling is reported"
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
}
