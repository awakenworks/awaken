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

/// The agent-registry state.
#[derive(Default)]
pub struct AgentRegistryState {
    inner: Mutex<BTreeMap<String, Record>>,
    seq: AtomicU64,
}

impl AgentRegistryState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    let store = state.inner.lock().unwrap();
    let record = store.get(&id).ok_or_else(not_found)?;
    Ok(Json(record.project(&id)))
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
