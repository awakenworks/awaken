//! The Models API (`/v1/models`) the official `@anthropic-ai/sdk` drives via
//! `client.beta.models.list` / `.retrieve`. It reports the models this
//! deployment can route to as `BetaModelInfo`. The list is a plain
//! [`Page`](https://docs.anthropic.com/en/api/models-list) (`data` + `has_more` +
//! `first_id` / `last_id`), NOT the vault family's cursor page.
//!
//! The directory is injected by the composition root: a gateway backs it from its
//! model catalog, the single-machine open build from the models it is configured
//! to serve (see [`default_models`]). No stub — every entry here is a model the
//! server will actually accept as an agent's `model`.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::json;

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
    fn new(id: &str, display_name: &str) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            context_window: None,
            max_output_tokens: None,
        }
    }

    /// Attach the model's published token limits (context window + output ceiling).
    #[must_use]
    fn with_limits(mut self, context_window: u32, max_output_tokens: u32) -> Self {
        self.context_window = Some(context_window);
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    /// The `BetaModelInfo` JSON projection. `max_input_tokens`/`max_tokens` carry the
    /// model's published context window / output ceiling (or `null` when unknown);
    /// `allowed_fallback_models` is an empty list (fallbacks are a gateway concern).
    fn to_json(&self) -> serde_json::Value {
        json!({
            "id": self.id,
            "type": "model",
            "display_name": self.display_name,
            "created_at": CREATED_AT,
            "allowed_fallback_models": [],
            "capabilities": null,
            "max_input_tokens": self.context_window,
            "max_tokens": self.max_output_tokens,
        })
    }
}

/// The current Claude model reference set the platform serves (ids per the
/// project's model catalog). Used by the open single-machine build as its Models
/// API directory; a gateway deployment supplies its own catalog-derived list
/// instead.
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
    Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/models/{id}", get(get_model))
        .with_state(models)
}

/// `GET /v1/models` — the full directory as a `Page<BetaModelInfo>` (one page:
/// `has_more:false`). `first_id` / `last_id` bracket the page for the SDK's
/// id-cursor paginator; both `null` when the directory is empty.
async fn list_models(State(models): State<Arc<Vec<ModelEntry>>>) -> impl IntoResponse {
    let data: Vec<_> = models.iter().map(ModelEntry::to_json).collect();
    let first_id = models.first().map(|m| m.id.clone());
    let last_id = models.last().map(|m| m.id.clone());
    (
        StatusCode::OK,
        axum::Json(json!({
            "data": data,
            "has_more": false,
            "first_id": first_id,
            "last_id": last_id,
        })),
    )
}

/// `GET /v1/models/{id}` — one model as `BetaModelInfo`, or `404`. Doubles as the
/// SDK's alias-resolution endpoint (an exact id here resolves to itself).
async fn get_model(
    State(models): State<Arc<Vec<ModelEntry>>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match models.iter().find(|m| m.id == id) {
        Some(entry) => (StatusCode::OK, axum::Json(entry.to_json())).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": format!("model `{id}` not found") })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_models_are_nonempty_and_project_to_beta_model_info() {
        let models = default_models();
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
        // Every entry projects to the `BetaModelInfo` core shape the SDK decodes.
        for m in &models {
            let v = m.to_json();
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
}
