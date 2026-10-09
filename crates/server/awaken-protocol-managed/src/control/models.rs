// @formal surface requirement=product.public_dto.anti_corruption
// @formal reason: This source implements the repository-controlled public dto product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! The Models API (`/v1/models`) the official `@anthropic-ai/sdk` drives via
//! GA `client.models` or `client.beta.models`. Both report the same executable
//! inventory through their distinct `ModelInfo` / `BetaModelInfo` projections. The list is a plain
//! [`Page`](https://docs.anthropic.com/en/api/models-list) (`data` + `has_more` +
//! `first_id` / `last_id`), NOT the vault family's cursor page.
//!
//! The executable Agent inventory is injected by the process entry point. Production uses a live,
//! Workspace-aware executable projection; [`default_models`] remains only for
//! explicit fixtures that provide their own observed publication instant.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use serde::{Deserialize, Serialize};

use crate::common::headers::ManagedCapability;
use crate::common::scope::RequiredWorkspaceScope;
use crate::resources::flavor::{
    ManagedResourceApiSurface, resource_api_surface, without_beta_selector,
};
use crate::types::page::{deserialize_optional_query_value, paginate_id_page, split_array_query};
use crate::types::{ErrorResponse, Page};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelListQuery {
    #[serde(default, deserialize_with = "deserialize_optional_query_value")]
    before_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_query_value")]
    after_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_query_value")]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelLifecycle {
    Active,
    Deprecated,
    Retired,
}

impl ModelListQuery {
    pub(crate) fn from_query(raw: &str) -> Result<(Self, Vec<ModelLifecycle>), String> {
        let query = split_array_query(raw, "lifecycle")?;
        if query.values.len() > 3 {
            return Err("lifecycle accepts at most 3 values".into());
        }
        let lifecycles = query
            .values
            .into_iter()
            .map(|value| {
                serde_json::from_value(serde_json::Value::String(value))
                    .map_err(|_| "lifecycle must be active, deprecated or retired".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut scalar = form_urlencoded::Serializer::new(String::new());
        scalar.extend_pairs(query.parameters);
        let page =
            serde_urlencoded::from_str(&scalar.finish()).map_err(|error| error.to_string())?;
        Ok((page, lifecycles))
    }
}

/// One model the deployment can serve, projected to the `BetaModelInfo` core
/// fields. `display_name` is human-facing; `id` is what an agent names as its
/// `model`.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub display_name: String,
    /// First local durable publication time, not an upstream model release
    /// date or a clock sampled while serving this GET. `None` is retained
    /// internally until pagination selects the row; v1 never emits null.
    pub created_at: Option<String>,
    /// The model's published context window (max input tokens) — the intrinsic
    /// attribute compaction and the ACP auto-compact window derive their budget from.
    /// `None` when the deployment doesn't publish it.
    pub context_window: Option<u32>,
    /// Hard ceiling on a single response's output tokens, when published.
    pub max_output_tokens: Option<u32>,
}

impl ModelEntry {
    fn without_time(id: &str, display_name: &str) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            created_at: None,
            context_window: None,
            max_output_tokens: None,
        }
    }

    pub fn new(id: &str, display_name: &str, created_at: &str) -> Self {
        let mut entry = Self::without_time(id, display_name);
        entry.created_at = Some(created_at.to_string());
        entry
    }

    fn from_publication(
        reference: awaken_executable_agent_contract::PublishedModelReference,
    ) -> Self {
        let mut entry = Self::without_time(&reference.model_reference, &reference.model_reference);
        entry.created_at = reference.first_published_at_unix_ms.and_then(|time| {
            (awaken_session_application_contract::project_epoch_millis(u128::from(time)) == time)
                .then(|| awaken_session_application_contract::epoch_millis_to_rfc3339(time))
        });
        entry
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
    fn project(&self, surface: ManagedResourceApiSurface) -> Option<ModelInfo<'_>> {
        Some(ModelInfo {
            id: &self.id,
            kind: "model",
            display_name: &self.display_name,
            created_at: self.created_at.as_deref()?,
            allowed_fallback_models: (surface != ManagedResourceApiSurface::Ga).then(Vec::new),
            // This directory reports locally reachable executable publications,
            // not an upstream provider's lifecycle or retirement catalogue.
            lifecycle: "active",
            deprecated_at: None,
            retires_at: None,
            line: None,
            capabilities: None,
            max_input_tokens: self.context_window,
            max_tokens: self.max_output_tokens,
        })
    }
}

#[derive(Debug, Serialize)]
struct ModelInfo<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    display_name: &'a str,
    created_at: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_fallback_models: Option<Vec<String>>,
    lifecycle: &'static str,
    deprecated_at: Option<&'a str>,
    retires_at: Option<&'a str>,
    line: Option<&'a str>,
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
            Self::ExecutableAgents(registrations) => registrations
                .current_model_publications(workspace_id)
                .await
                .map(|references| {
                    references
                        .into_iter()
                        .map(ModelEntry::from_publication)
                        .collect()
                })
                .map_err(|error| error.to_string()),
        }
    }
}

/// Deterministic model fixture for tests without a configuration plane. The
/// caller supplies the fixture's publication instant explicitly.
#[must_use]
pub fn default_models(published_at_unix_ms: u64) -> Vec<ModelEntry> {
    let at = awaken_session_application_contract::epoch_millis_to_rfc3339(published_at_unix_ms);
    vec![
        // Published context windows (max input tokens) — the intrinsic attribute the
        // compaction window derives from; 200K is the standard Claude context.
        ModelEntry::new("claude-opus-4-8", "Claude Opus 4.8", &at).with_limits(200_000, 64_000),
        ModelEntry::new("claude-sonnet-5", "Claude Sonnet 5", &at).with_limits(200_000, 64_000),
        ModelEntry::new("claude-haiku-4-5-20251001", "Claude Haiku 4.5", &at)
            .with_limits(200_000, 32_000),
        ModelEntry::new("claude-fable-5", "Fable 5", &at).with_limits(200_000, 32_000),
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

/// `GET /v1/models` — the executable directory as the GA/Beta id-cursor `Page`.
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
    let (query, lifecycles) =
        match ModelListQuery::from_query(&without_beta_selector(raw.as_deref())) {
            Ok(query) => query,
            Err(error) => {
                return model_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    error.to_string(),
                );
            }
        };
    let Ok(models) = available.in_workspace(&workspace).await else {
        return model_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "model directory unavailable",
        );
    };
    // Current publications are all locally active. Filter this same collection
    // before pagination; never add a retired-history reader or synthetic rows.
    let models = if lifecycles.is_empty() || lifecycles.contains(&ModelLifecycle::Active) {
        models
    } else {
        Vec::new()
    };
    let page = match paginate_id_page(
        &models,
        query.before_id.as_deref(),
        query.after_id.as_deref(),
        query.limit,
        |model| model.id.as_str(),
    ) {
        Ok(page) => page,
        Err(error) => {
            return model_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                error.to_string(),
            );
        }
    };
    let Some(data) = page
        .data
        .iter()
        .map(|model| model.project(surface))
        .collect::<Option<Vec<_>>>()
    else {
        return model_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "model publication time unavailable",
        );
    };
    (
        StatusCode::OK,
        axum::Json(Page::new(data, page.has_more, page.first_id, page.last_id)),
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
        Some(entry) => match entry.project(surface) {
            Some(model) => (StatusCode::OK, axum::Json(model)).into_response(),
            None => model_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "model publication time unavailable",
            ),
        },
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

    #[derive(Default)]
    struct UnavailableInventory(std::sync::atomic::AtomicUsize);

    struct PublishedInventory(Vec<awaken_executable_agent_contract::PublishedModelReference>);

    #[derive(Default)]
    struct WorkspaceInventory(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl awaken_executable_agent_contract::ExecutableAgentInventorySource for WorkspaceInventory {
        async fn current_registrations(
            &self,
            _: &str,
        ) -> Result<
            Vec<awaken_executable_agent_contract::ExecutableAgentRegistration>,
            awaken_executable_agent_contract::ExecutableAgentRegistrationError,
        > {
            panic!("Models must read the existing publication port")
        }

        async fn current_model_publications(
            &self,
            workspace: &str,
        ) -> Result<
            Vec<awaken_executable_agent_contract::PublishedModelReference>,
            awaken_executable_agent_contract::ExecutableAgentRegistrationError,
        > {
            self.0.lock().unwrap().push(workspace.to_owned());
            Ok(vec![
                awaken_executable_agent_contract::PublishedModelReference {
                    model_reference: format!("model-{workspace}"),
                    first_published_at_unix_ms: Some(1_704_067_200_000),
                },
            ])
        }
    }

    #[async_trait::async_trait]
    impl awaken_executable_agent_contract::ExecutableAgentInventorySource for PublishedInventory {
        async fn current_registrations(
            &self,
            _workspace_id: &str,
        ) -> Result<
            Vec<awaken_executable_agent_contract::ExecutableAgentRegistration>,
            awaken_executable_agent_contract::ExecutableAgentRegistrationError,
        > {
            Ok(Vec::new())
        }

        async fn current_model_publications(
            &self,
            _workspace_id: &str,
        ) -> Result<
            Vec<awaken_executable_agent_contract::PublishedModelReference>,
            awaken_executable_agent_contract::ExecutableAgentRegistrationError,
        > {
            Ok(self.0.clone())
        }
    }

    #[async_trait::async_trait]
    impl awaken_executable_agent_contract::ExecutableAgentInventorySource for UnavailableInventory {
        async fn current_registrations(
            &self,
            _workspace_id: &str,
        ) -> Result<
            Vec<awaken_executable_agent_contract::ExecutableAgentRegistration>,
            awaken_executable_agent_contract::ExecutableAgentRegistrationError,
        > {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        // capabilities. Effects: E1 both emit the eleven shared ModelInfo fields;
        // E2 Beta alone emits allowed_fallback_models; E3 limits are numeric and
        // capabilities null; E4 created_at is the fixture-supplied instant.
        // Constraint: flavor never selects inventory/state.
        // Decision rules: R1 C1=GA+C2+C3->E1+E3; R2 C1=Beta+C2+C3->E1+E2+E3.
        let models = default_models(1_704_067_200_000);
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
        for m in &models {
            for (rule, surface, fields, fallback) in [
                ("R1", ManagedResourceApiSurface::Ga, 11, false),
                ("R2", ManagedResourceApiSurface::CapabilityBeta, 12, true),
                ("R3", ManagedResourceApiSurface::QueryBeta, 12, true),
            ] {
                let v = serde_json::to_value(m.project(surface).expect("fixture time")).unwrap();
                assert_eq!(v.as_object().unwrap().len(), fields, "{rule}");
                assert_eq!(v["type"], "model", "{rule}");
                assert_eq!(v["id"], m.id, "{rule}");
                assert!(v["display_name"].is_string(), "{rule}");
                assert!(v["created_at"].is_string(), "{rule}");
                assert_eq!(v["created_at"], "2024-01-01T00:00:00Z", "E4");
                assert_eq!(
                    v.get("allowed_fallback_models").is_some(),
                    fallback,
                    "{rule}"
                );
                assert!(v["capabilities"].is_null(), "{rule}");
                assert_eq!(v["lifecycle"], "active", "local reachability {rule}");
                for unknown in ["deprecated_at", "retires_at", "line"] {
                    assert!(
                        v.get(unknown).unwrap().is_null(),
                        "unknown fact {unknown} {rule}"
                    );
                }
                assert_eq!(v["max_input_tokens"], 200_000, "{rule}");
                assert!(v["max_tokens"].as_u64().is_some(), "{rule}");
            }
        }
    }

    // Test design: models_route_uses_the_canonical_header_flavor_without_splitting_inventory
    // Cause/effect graph: C1 no Managed beta selector; C2 the canonical Managed beta
    // header; C3 `beta=true` without a capability; C4 every request
    // addresses the same fixed inventory. Effects:
    // E1 C1 returns GA ModelInfo without allowed_fallback_models; E2 C2
    // and C3 return BetaModelInfo with it; E3 all retain the same model id.
    // Constraint: transport selection changes projection only. Decision
    // Decision table: R1 C1+C4->E1+E3; R2 C2+C4->E2+E3;
    #[tokio::test]
    async fn models_route_uses_the_canonical_header_flavor_without_splitting_inventory() {
        // R3 C3+C4->E2+E3.
        let app = models_router(Arc::new(vec![ModelEntry::new(
            "model-a",
            "Model A",
            "2024-01-01T00:00:00Z",
        )]))
        .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            "default".into(),
        )));
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
    async fn models_list_honors_every_page_params_axis() {
        // Causal graph: C1 ordered executable inventory; C2 before/after are
        // mutually exclusive boundaries; C3 limit bounds the selected slice;
        // C4 TypeScript null is `field=` while Python omission is absent.
        // Effects: E1 cursor boundaries are exclusive; E2 has_more and edge ids
        // describe exactly that slice; E3 malformed, competing, or unknown
        // coordinates fail 400. This prevents a superficially decodable
        // `Page` from silently ignoring the official SDK's paginator inputs.
        let app = models_router(Arc::new(vec![
            ModelEntry::new("a", "A", "2024-01-01T00:00:00Z"),
            ModelEntry::new("b", "B", "2024-01-01T00:00:00Z"),
            ModelEntry::new("c", "C", "2024-01-01T00:00:00Z"),
        ]))
        .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            "default".into(),
        )));
        for (rule, uri, expected, has_more) in [
            ("R1", "/v1/models?limit=1", vec!["a"], true),
            ("R2", "/v1/models?after_id=a&limit=1", vec!["b"], true),
            ("R3", "/v1/models?before_id=c", vec!["a", "b"], false),
            (
                "R4",
                "/v1/models?beta=true&limit=",
                vec!["a", "b", "c"],
                false,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{rule}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let ids = value["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(ids, expected, "{rule}");
            assert_eq!(value["has_more"], has_more, "{rule}");
            assert_eq!(
                value["first_id"].as_str(),
                expected.first().copied(),
                "{rule}"
            );
            assert_eq!(
                value["last_id"].as_str(),
                expected.last().copied(),
                "{rule}"
            );
        }
        for (rule, uri) in [
            ("R5", "/v1/models?before_id=c&after_id=a"),
            ("R6", "/v1/models?after_id=missing"),
            ("R7", "/v1/models?limit=0"),
            ("R8", "/v1/models?unexpected=true"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{rule}");
        }
    }

    #[tokio::test]
    async fn lifecycle_filters_share_local_inventory_and_page_rules_across_surfaces() {
        // Causes: L1 GA/header-Beta/query-Beta; L2 omitted/null/empty SDK array
        // or active/deprecated/retired members; L3 exclusive cursor and limit.
        // Effects: R1 defaults or a set containing active retain the SAME local
        // inventory and page edges; R2 inactive-only selects an empty page.
        // Constraint: current publication reachability is local active truth;
        // it supplies no upstream line, dates, retirement history or capability.
        // The exact SDK serializes [] as omission and null as lifecycle=.
        let app = models_router(Arc::new(vec![
            ModelEntry::new("a", "A", "2024-01-01T00:00:00Z"),
            ModelEntry::new("b", "B", "2024-01-01T00:00:00Z"),
        ]))
        .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            "workspace-a".into(),
        )));
        for (surface, beta_query, header) in [
            ("GA", "", false),
            ("header-Beta", "", true),
            ("query-Beta", "&beta=true", false),
        ] {
            for (rule, filter, ids, has_more) in [
                ("R1 omission/SDK-empty-array", "", vec!["a"], true),
                ("R1 null", "&lifecycle=", vec!["a"], true),
                ("R1 active", "&lifecycle%5B%5D=active", vec!["a"], true),
                (
                    "R1 mixed",
                    "&lifecycle[]=deprecated&lifecycle[]=active&lifecycle[]=retired",
                    vec!["a"],
                    true,
                ),
                (
                    "R1 cursor",
                    "&lifecycle[]=active&after_id=a",
                    vec!["b"],
                    false,
                ),
                ("R2 deprecated", "&lifecycle[]=deprecated", vec![], false),
                ("R2 retired", "&lifecycle[]=retired", vec![], false),
                (
                    "R2 inactive union",
                    "&lifecycle[]=deprecated&lifecycle[]=retired",
                    vec![],
                    false,
                ),
            ] {
                let mut request = axum::http::Request::builder()
                    .uri(format!("/v1/models?limit=1{filter}{beta_query}"));
                if header {
                    request =
                        request.header("anthropic-beta", ManagedCapability::ManagedAgents.beta());
                }
                let response = app
                    .clone()
                    .oneshot(request.body(axum::body::Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{surface} {rule}");
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let actual = value["data"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v["id"].as_str().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(actual, ids, "{surface} {rule}");
                assert_eq!(value["has_more"], has_more, "{surface} {rule}");
                assert_eq!(
                    value["first_id"].as_str(),
                    ids.first().copied(),
                    "{surface} {rule}"
                );
                assert_eq!(
                    value["last_id"].as_str(),
                    ids.last().copied(),
                    "{surface} {rule}"
                );
                for model in value["data"].as_array().unwrap() {
                    assert_eq!(model["lifecycle"], "active", "{surface} {rule}");
                    for key in ["line", "deprecated_at", "retires_at", "capabilities"] {
                        assert!(model.get(key).unwrap().is_null(), "{surface} {rule} {key}");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn invalid_lifecycle_queries_fail_before_inventory_io() {
        // Causes: Q1 unknown member/empty member/non-array scalar; Q2 more than
        // three array members (count before dedup); Q3 GA/Beta transport.
        // Effect Q1 or Q2 -> 400 and zero inventory reads, even when its source
        // is unavailable. A valid query still reads the source and yields 503:
        // an inactive filter cannot hide source failure or manufacture history.
        let inventory = Arc::new(UnavailableInventory::default());
        let app = models_router_with_inventory(inventory.clone()).layer(axum::Extension(
            awaken_tenancy::WorkspaceScope("workspace-a".into()),
        ));
        for beta in ["", "&beta=true"] {
            for query in [
                "lifecycle=active",
                "lifecycle[]=unknown",
                "lifecycle[]=",
                "lifecycle[]=active&lifecycle[]=active&lifecycle[]=active&lifecycle[]=active",
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(format!("/v1/models?{query}{beta}"))
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}{beta}");
                assert_eq!(
                    inventory.0.load(std::sync::atomic::Ordering::SeqCst),
                    0,
                    "no IO"
                );
            }
        }
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/models?lifecycle[]=retired")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(inventory.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lifecycle_projection_keeps_workspace_source_and_retrieve_parity() {
        // Causes: W1 two trusted Workspace scopes; W2 GA/Beta list/retrieve;
        // W3 active predicate. Effects: each operation reads only its injected
        // Workspace publication port, retains its exact model/time, and emits
        // identical local lifecycle/unknown metadata. No cross-Workspace cache,
        // fixture defaults or second model catalogue participates.
        let inventory = Arc::new(WorkspaceInventory::default());
        for workspace in ["a", "b"] {
            let app = models_router_with_inventory(inventory.clone()).layer(axum::Extension(
                awaken_tenancy::WorkspaceScope(workspace.into()),
            ));
            for beta in [false, true] {
                for path in [
                    "/v1/models?lifecycle[]=active".to_owned(),
                    format!("/v1/models/model-{workspace}"),
                ] {
                    let mut request = axum::http::Request::builder().uri(&path);
                    if beta {
                        request = request
                            .header("anthropic-beta", ManagedCapability::ManagedAgents.beta());
                    }
                    let response = app
                        .clone()
                        .oneshot(request.body(axum::body::Body::empty()).unwrap())
                        .await
                        .unwrap();
                    assert_eq!(response.status(), StatusCode::OK);
                    let body = response.into_body().collect().await.unwrap().to_bytes();
                    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let model = if path.starts_with("/v1/models?") {
                        &value["data"][0]
                    } else {
                        &value
                    };
                    assert_eq!(model["id"], format!("model-{workspace}"));
                    assert_eq!(model["created_at"], "2024-01-01T00:00:00Z");
                    assert_eq!(model["lifecycle"], "active");
                    for unknown in ["line", "deprecated_at", "retires_at", "capabilities"] {
                        assert!(model.get(unknown).unwrap().is_null());
                    }
                    assert_eq!(model.get("allowed_fallback_models").is_some(), beta);
                }
            }
        }
        assert_eq!(
            *inventory.0.lock().unwrap(),
            ["a", "a", "a", "a", "b", "b", "b", "b"]
        );
    }

    // Test design: retrieve_accepts_complete_slash_bearing_model_id
    // Cause/effect graph: C1 the registered model id contains provider/model path
    // separators; C2 the request supplies that complete id through the
    // wildcard route. Effect: E1 retrieval matches the exact id and returns
    // its BetaModelInfo document. Decision rule R1=C1&&C2 -> E1. This test
    // belongs to the HTTP adapter; directory implementations need only
    // supply opaque model ids.
    // Decision table: exact complete id=>200 canonical model; truncated/unknown id=>404.
    #[tokio::test]
    async fn retrieve_accepts_complete_slash_bearing_model_id() {
        let id = "provider/claude/model-a";
        let app = models_router(Arc::new(vec![ModelEntry::new(
            id,
            "Model A",
            "2024-01-01T00:00:00Z",
        )]))
        .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            "default".into(),
        )));
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
        let app = models_router_with_inventory(Arc::new(UnavailableInventory::default())).layer(
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

    #[tokio::test]
    async fn live_models_use_durable_first_publication_and_reject_unknown_time() {
        // Causes: M1 exact first local publication instant exists; M2 the
        // inventory has no durable time or an unrenderable time; M3 list
        // versus exact retrieve; M4 a page excludes/includes an unknown model.
        // Effects: E1 both routes carry the same factual instant, with no GET
        // clock; E2 unknown history fails closed instead of publishing a fake
        // date; E3 unaffected pages stay readable while a page containing
        // unknown time fails. Rules M1+M3=>E1, M2+M3=>E2, M1+M2+M4=>E3.
        let known = models_router_with_inventory(Arc::new(PublishedInventory(vec![
            awaken_executable_agent_contract::PublishedModelReference {
                model_reference: "model-a".into(),
                first_published_at_unix_ms: Some(1_704_067_200_000),
            },
        ])))
        .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            "default".into(),
        )));
        for path in ["/v1/models", "/v1/models/model-a"] {
            let response = known
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "E1");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let model = if path == "/v1/models" {
                &value["data"][0]
            } else {
                &value
            };
            assert_eq!(model["created_at"], "2024-01-01T00:00:00Z", "E1");
        }
        for time in [None, Some(u64::MAX)] {
            let unknown = models_router_with_inventory(Arc::new(PublishedInventory(vec![
                awaken_executable_agent_contract::PublishedModelReference {
                    model_reference: "model-a".into(),
                    first_published_at_unix_ms: time,
                },
            ])))
            .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
                "default".into(),
            )));
            for path in ["/v1/models", "/v1/models/model-a"] {
                let response = unknown
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(path)
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "E2");
            }
        }
        let mixed = models_router_with_inventory(Arc::new(PublishedInventory(vec![
            awaken_executable_agent_contract::PublishedModelReference {
                model_reference: "model-a".into(),
                first_published_at_unix_ms: Some(1_704_067_200_000),
            },
            awaken_executable_agent_contract::PublishedModelReference {
                model_reference: "model-b".into(),
                first_published_at_unix_ms: None,
            },
        ])))
        .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            "default".into(),
        )));
        for (path, expected) in [
            ("/v1/models?limit=1", StatusCode::OK),
            ("/v1/models/model-a", StatusCode::OK),
            ("/v1/models", StatusCode::SERVICE_UNAVAILABLE),
            (
                "/v1/models?after_id=model-a&limit=1",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            ("/v1/models/model-b", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let response = mixed
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "E3 {path}");
        }
    }
}
