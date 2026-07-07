//! The Managed **agent registry** (`/v1/agents`), the official `@anthropic-ai/sdk`
//! `beta.agents.*` client's surface: create / retrieve / update / list / archive
//! plus the version history (`/v1/agents/:id/versions`). An agent is a reusable,
//! versioned configuration (model + system + tools + mcp_servers + skills +
//! multiagent topology) a session instantiates by id.
//!
//! Distinct from the workspace **config plane** (`/v1/config/agents/*`), which is
//! the admin authoring surface; this is the public account-level registry the SDK
//! addresses. State is a neutral in-memory store (one process): a stable
//! `agent_…` id, monotonic `version` with optimistic-concurrency updates, and a
//! full snapshot appended to history on every mutation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::dto::ErrorResponse;
use crate::pagination::Page;
use crate::router::ManagedJson;

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

/// A stored agent configuration and its version history.
#[derive(Clone)]
struct Record {
    name: String,
    description: Option<String>,
    /// Normalized `BetaManagedAgentsModelConfig` (`{id, speed?}`).
    model: Value,
    system: Option<String>,
    metadata: BTreeMap<String, String>,
    mcp_servers: Vec<Value>,
    skills: Vec<Value>,
    tools: Vec<Value>,
    multiagent: Option<Value>,
    version: u64,
    archived_at: Option<String>,
    /// The projected agent at each past version (index 0 == v1), for `versions.list`.
    history: Vec<Value>,
}

impl Record {
    fn project(&self, id: &str) -> Value {
        json!({
            "id": id,
            "type": "agent",
            "archived_at": self.archived_at,
            "created_at": OBJECT_AT,
            "updated_at": OBJECT_AT,
            "name": self.name,
            "description": self.description,
            "model": self.model,
            "system": self.system,
            "metadata": self.metadata,
            "mcp_servers": self.mcp_servers,
            "skills": self.skills,
            "tools": self.tools,
            "multiagent": self.multiagent,
            "version": self.version,
        })
    }
}

/// The config-plane projection of an agent: the runtime-authoritative fields the
/// managed wire shows. A neutral view so `/v1/agents` presents an agent authored on
/// the config plane (`/v1/config/agents`) as a *projection* of that single truth
/// rather than a second copy — the "retreat to projection" direction (ADR-0043).
pub struct AgentConfigView {
    pub model: Option<String>,
    pub system: Option<String>,
    pub tool_ids: Vec<String>,
}

/// A source of config-plane agent projections. A **port**: the host implements it
/// over its `ConfigService` (the managed crate cannot depend on the host), so the
/// managed adapter reads the neutral config truth without naming it.
pub trait AgentConfigSource: Send + Sync {
    /// The config-plane view of `agent_id`, if it is published there.
    fn agent_view(&self, agent_id: &str) -> Option<AgentConfigView>;
}

/// The agent-registry state.
#[derive(Default)]
pub struct AgentRegistryState {
    inner: Mutex<BTreeMap<String, Record>>,
    seq: AtomicU64,
    /// When wired, `/v1/agents` reads projections from the config plane: an agent
    /// published there is retrievable here even if it was never created via this
    /// registry, and the config is authoritative for model/system/tools.
    config_source: Option<Arc<dyn AgentConfigSource>>,
}

impl AgentRegistryState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire the config-plane projection source (see [`AgentConfigSource`]).
    #[must_use]
    pub fn with_config_source(mut self, source: Arc<dyn AgentConfigSource>) -> Self {
        self.config_source = Some(source);
        self
    }
}

/// Present tool ids in the managed wire's tool shape.
fn tools_wire(ids: &[String]) -> Vec<Value> {
    ids.iter()
        .map(|id| json!({ "type": "custom", "name": id }))
        .collect()
}

/// Project a config-plane view to the `BetaManagedAgent` wire shape. The agent has
/// no registry record, so presentation fields default (name = id, version = 1).
fn project_config_view(id: &str, view: &AgentConfigView) -> Value {
    json!({
        "id": id,
        "type": "agent",
        "archived_at": null,
        "created_at": OBJECT_AT,
        "updated_at": OBJECT_AT,
        "name": id,
        "description": null,
        "model": { "id": view.model.clone().unwrap_or_default() },
        "system": view.system,
        "metadata": {},
        "mcp_servers": [],
        "skills": [],
        "tools": tools_wire(&view.tool_ids),
        "multiagent": null,
        "version": 1,
    })
}

/// Overlay config-plane truth onto a registry projection: where an agent exists in
/// both, the config plane is authoritative for model/system/tools (single truth).
fn overlay_config_view(wire: &mut Value, view: &AgentConfigView) {
    if let Some(obj) = wire.as_object_mut() {
        if let Some(model) = &view.model {
            obj.insert("model".into(), json!({ "id": model }));
        }
        obj.insert("system".into(), json!(view.system));
        obj.insert("tools".into(), json!(tools_wire(&view.tool_ids)));
    }
}

/// Mount the agent-registry routes.
pub fn agents_router(state: Arc<AgentRegistryState>) -> Router {
    Router::new()
        .route("/v1/agents", post(create_agent).get(list_agents))
        .route("/v1/agents/:id", get(retrieve_agent).post(update_agent))
        .route("/v1/agents/:id/archive", post(archive_agent))
        .route("/v1/agents/:id/versions", get(list_versions))
        .with_state(state)
}

type WireError = (StatusCode, Json<ErrorResponse>);

fn not_found() -> WireError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new("not_found_error", "agent not found")),
    )
}

fn bad_request(message: impl Into<String>) -> WireError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

/// Normalize the `model` field: a bare string becomes `{id: <string>}`; an object
/// (`{id, speed?}`) passes through. Anything else is a `400`.
fn normalize_model(model: &Value) -> Result<Value, WireError> {
    match model {
        Value::String(s) => Ok(json!({ "id": s })),
        Value::Object(_) => Ok(model.clone()),
        _ => Err(bad_request(
            "model must be a string or a model-config object",
        )),
    }
}

fn value_array(body: &Value, key: &str) -> Vec<Value> {
    body.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn value_opt_string(body: &Value, key: &str) -> Option<String> {
    body.get(key).and_then(Value::as_str).map(str::to_string)
}

fn value_metadata(body: &Value, key: &str) -> BTreeMap<String, String> {
    body.get(key)
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

async fn create_agent(
    State(state): State<Arc<AgentRegistryState>>,
    ManagedJson(body): ManagedJson<Value>,
) -> Result<Json<Value>, WireError> {
    let name = value_opt_string(&body, "name").ok_or_else(|| bad_request("name is required"))?;
    let model = normalize_model(
        body.get("model")
            .ok_or_else(|| bad_request("model is required"))?,
    )?;
    let n = state.seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("agent_{n:016}");
    let mut record = Record {
        name,
        description: value_opt_string(&body, "description"),
        model,
        system: value_opt_string(&body, "system"),
        metadata: value_metadata(&body, "metadata"),
        mcp_servers: value_array(&body, "mcp_servers"),
        skills: value_array(&body, "skills"),
        tools: value_array(&body, "tools"),
        multiagent: body.get("multiagent").filter(|v| !v.is_null()).cloned(),
        version: 1,
        archived_at: None,
        history: Vec::new(),
    };
    let projected = record.project(&id);
    record.history.push(projected.clone());
    state.inner.lock().unwrap().insert(id, record);
    Ok(Json(projected))
}

async fn retrieve_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, WireError> {
    // A registry record wins; the config plane, when wired, is authoritative for the
    // fields it owns (model/system/tools) and is overlaid onto the record.
    {
        let store = state.inner.lock().unwrap();
        if let Some(record) = store.get(&id) {
            let mut wire = record.project(&id);
            if let Some(view) = state.config_source.as_ref().and_then(|s| s.agent_view(&id)) {
                overlay_config_view(&mut wire, &view);
            }
            return Ok(Json(wire));
        }
    }
    // No registry record: an agent published on the config plane is retrievable here
    // as a pure projection of that single truth (never created via this registry).
    if let Some(view) = state.config_source.as_ref().and_then(|s| s.agent_view(&id)) {
        return Ok(Json(project_config_view(&id, &view)));
    }
    Err(not_found())
}

/// `GET /v1/agents` — one full page, ascending id order.
async fn list_agents(State(state): State<Arc<AgentRegistryState>>) -> Json<Page<Value>> {
    let store = state.inner.lock().unwrap();
    let data = store.iter().map(|(id, r)| r.project(id)).collect();
    Json(Page::single(data))
}

/// `POST /v1/agents/:id` — update with optimistic concurrency. The body's
/// `version` must match the agent's current version (else `409`); on success the
/// provided fields replace, `version` increments, and a snapshot is appended to
/// history.
async fn update_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<Value>,
) -> Result<Json<Value>, WireError> {
    let expected = body
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad_request("version is required for an update"))?;
    let mut store = state.inner.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(not_found)?;
    if expected != record.version {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "conflict_error",
                format!(
                    "version mismatch: expected {}, got {expected}",
                    record.version
                ),
            )),
        ));
    }
    // Each field replaces only when present in the body.
    if let Some(name) = value_opt_string(&body, "name") {
        record.name = name;
    }
    if let Some(model) = body.get("model") {
        record.model = normalize_model(model)?;
    }
    if body.get("description").is_some() {
        record.description = value_opt_string(&body, "description");
    }
    if body.get("system").is_some() {
        record.system = value_opt_string(&body, "system");
    }
    if body.get("metadata").is_some() {
        record.metadata = value_metadata(&body, "metadata");
    }
    if body.get("mcp_servers").is_some() {
        record.mcp_servers = value_array(&body, "mcp_servers");
    }
    if body.get("skills").is_some() {
        record.skills = value_array(&body, "skills");
    }
    if body.get("tools").is_some() {
        record.tools = value_array(&body, "tools");
    }
    if body.get("multiagent").is_some() {
        record.multiagent = body.get("multiagent").filter(|v| !v.is_null()).cloned();
    }
    record.version += 1;
    let projected = record.project(&id);
    record.history.push(projected.clone());
    Ok(Json(projected))
}

/// `POST /v1/agents/:id/archive` — soft-delete (sets `archived_at`, bumps version).
async fn archive_agent(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, WireError> {
    let mut store = state.inner.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(not_found)?;
    record.archived_at = Some(OBJECT_AT.to_string());
    record.version += 1;
    let projected = record.project(&id);
    record.history.push(projected.clone());
    Ok(Json(projected))
}

/// `GET /v1/agents/:id/versions` — the agent's version history as a cursor page
/// (newest last), each entry a full `BetaManagedAgentsAgent` snapshot.
async fn list_versions(
    State(state): State<Arc<AgentRegistryState>>,
    Path(id): Path<String>,
) -> Result<Json<Page<Value>>, WireError> {
    let store = state.inner.lock().unwrap();
    let record = store.get(&id).ok_or_else(not_found)?;
    Ok(Json(Page::single(record.history.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// A config plane that has published exactly one agent, `assistant`.
    struct OneAgent;
    impl AgentConfigSource for OneAgent {
        fn agent_view(&self, agent_id: &str) -> Option<AgentConfigView> {
            (agent_id == "assistant").then(|| AgentConfigView {
                model: Some("kimi-k2".to_string()),
                system: Some("be helpful".to_string()),
                tool_ids: vec!["fs_read".to_string()],
            })
        }
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn config_plane_agent_is_retrievable_as_a_projection() {
        let state = Arc::new(AgentRegistryState::new().with_config_source(Arc::new(OneAgent)));
        let app = agents_router(state);

        // `assistant` was never created via /v1/agents — it is projected from the
        // config plane (the "retreat to projection" direction).
        let (status, body) = get(&app, "/v1/agents/assistant").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "assistant");
        assert_eq!(body["model"]["id"], "kimi-k2");
        assert_eq!(body["system"], "be helpful");
        assert_eq!(body["tools"][0]["name"], "fs_read");

        // An id in neither the registry nor the config plane is still 404.
        let (status, _) = get(&app, "/v1/agents/ghost").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn overlay_makes_config_authoritative_for_model_system_tools() {
        let mut wire = json!({
            "model": { "id": "stale" }, "system": "stale", "tools": [],
            "name": "keep-me",
        });
        overlay_config_view(
            &mut wire,
            &AgentConfigView {
                model: Some("fresh".to_string()),
                system: Some("fresh sys".to_string()),
                tool_ids: vec!["t".to_string()],
            },
        );
        assert_eq!(wire["model"]["id"], "fresh");
        assert_eq!(wire["system"], "fresh sys");
        assert_eq!(wire["tools"][0]["name"], "t");
        // Presentation fields the config plane does not own are untouched.
        assert_eq!(wire["name"], "keep-me");
    }
}
