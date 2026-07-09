//! The Managed **deployments** family (`/v1/deployments`) + **deployment runs**
//! (`/v1/deployment_runs`), the official `@anthropic-ai/sdk` `beta.deployments.*`
//! and `beta.deploymentRuns.*` surfaces. A deployment binds an agent to an
//! environment with initial events + a schedule; `run` triggers a
//! `deployment_run`; `pause`/`unpause` toggle the schedule; `archive` soft-deletes.
//!
//! State is a neutral in-memory store (one process): stable `deploy_…` /
//! `deprun_…` ids, deterministic ascending-id list order.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::routes::ManagedJson;
use crate::types::deployment::{Deployment, DeploymentRun};
use crate::types::{ErrorResponse, Page};

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

#[derive(Clone)]
struct DeploymentRecord {
    /// Normalized `BetaManagedAgentsAgentReference` (`{id, type:'agent', version}`).
    agent: Value,
    environment_id: String,
    name: String,
    description: Option<String>,
    metadata: BTreeMap<String, String>,
    initial_events: Vec<Value>,
    resources: Vec<Value>,
    schedule: Option<Value>,
    vault_ids: Vec<String>,
    /// `"active"` | `"paused"`.
    status: &'static str,
    paused_reason: Option<Value>,
    archived_at: Option<String>,
}

impl DeploymentRecord {
    fn project(&self, id: &str) -> Deployment {
        Deployment {
            id: id.to_string(),
            object_type: "deployment",
            agent: self.agent.clone(),
            archived_at: self.archived_at.clone(),
            created_at: OBJECT_AT.to_string(),
            updated_at: OBJECT_AT.to_string(),
            description: self.description.clone(),
            environment_id: self.environment_id.clone(),
            initial_events: self.initial_events.clone(),
            metadata: self.metadata.clone(),
            name: self.name.clone(),
            paused_reason: self.paused_reason.clone(),
            resources: self.resources.clone(),
            schedule: self.schedule.clone(),
            status: self.status,
            vault_ids: self.vault_ids.clone(),
        }
    }
}

#[derive(Clone)]
struct RunRecord {
    deployment_id: String,
    agent: Value,
}

impl RunRecord {
    fn project(&self, id: &str) -> DeploymentRun {
        DeploymentRun {
            id: id.to_string(),
            object_type: "deployment_run",
            agent: self.agent.clone(),
            created_at: OBJECT_AT.to_string(),
            deployment_id: self.deployment_id.clone(),
            error: None,
            session_id: None,
            trigger_context: json!({ "type": "manual" }),
        }
    }
}

/// The deployments + deployment-runs state.
#[derive(Default)]
pub struct DeploymentState {
    deployments: Mutex<BTreeMap<String, DeploymentRecord>>,
    runs: Mutex<BTreeMap<String, RunRecord>>,
    dep_seq: AtomicU64,
    run_seq: AtomicU64,
}

impl DeploymentState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Mount the deployments + deployment-runs routes.
pub fn deployments_router(state: Arc<DeploymentState>) -> Router {
    Router::new()
        .route(
            "/v1/deployments",
            post(create_deployment).get(list_deployments),
        )
        .route(
            "/v1/deployments/:id",
            get(retrieve_deployment).post(update_deployment),
        )
        .route("/v1/deployments/:id/archive", post(archive_deployment))
        .route("/v1/deployments/:id/pause", post(pause_deployment))
        .route("/v1/deployments/:id/unpause", post(unpause_deployment))
        .route("/v1/deployments/:id/run", post(run_deployment))
        .route("/v1/deployment_runs", get(list_runs))
        .route("/v1/deployment_runs/:id", get(retrieve_run))
        .with_state(state)
}

type WireError = (StatusCode, Json<ErrorResponse>);

fn not_found(what: &str) -> WireError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new(
            "not_found_error",
            format!("{what} not found"),
        )),
    )
}

fn bad_request(message: impl Into<String>) -> WireError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

/// Normalize `agent` into a `BetaManagedAgentsAgentReference`: a bare id string
/// becomes `{id, type:'agent', version:1}`; an object with an `id` passes through
/// (defaulting `version` to 1); anything else is a `400`.
fn normalize_agent(agent: &Value) -> Result<Value, WireError> {
    match agent {
        Value::String(s) => Ok(json!({ "id": s, "type": "agent", "version": 1 })),
        Value::Object(o) => {
            let id = o
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_request("agent object must carry an id"))?;
            let version = o.get("version").and_then(Value::as_u64).unwrap_or(1);
            Ok(json!({ "id": id, "type": "agent", "version": version }))
        }
        _ => Err(bad_request("agent must be a string id or an agent object")),
    }
}

fn value_array(body: &Value, key: &str) -> Vec<Value> {
    body.get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn value_str_array(body: &Value, key: &str) -> Vec<String> {
    body.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
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

async fn create_deployment(
    State(state): State<Arc<DeploymentState>>,
    ManagedJson(body): ManagedJson<Value>,
) -> Result<Json<Deployment>, WireError> {
    let agent = normalize_agent(
        body.get("agent")
            .ok_or_else(|| bad_request("agent is required"))?,
    )?;
    let environment_id = value_opt_string(&body, "environment_id")
        .ok_or_else(|| bad_request("environment_id is required"))?;
    let name = value_opt_string(&body, "name").ok_or_else(|| bad_request("name is required"))?;
    let record = DeploymentRecord {
        agent,
        environment_id,
        name,
        description: value_opt_string(&body, "description"),
        metadata: value_metadata(&body, "metadata"),
        initial_events: value_array(&body, "initial_events"),
        resources: value_array(&body, "resources"),
        schedule: body.get("schedule").filter(|v| !v.is_null()).cloned(),
        vault_ids: value_str_array(&body, "vault_ids"),
        status: "active",
        paused_reason: None,
        archived_at: None,
    };
    let n = state.dep_seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("deploy_{n:016}");
    let projected = record.project(&id);
    state.deployments.lock().unwrap().insert(id, record);
    Ok(Json(projected))
}

async fn retrieve_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
) -> Result<Json<Deployment>, WireError> {
    let store = state.deployments.lock().unwrap();
    let record = store.get(&id).ok_or_else(|| not_found("deployment"))?;
    Ok(Json(record.project(&id)))
}

async fn list_deployments(State(state): State<Arc<DeploymentState>>) -> Json<Page<Deployment>> {
    let store = state.deployments.lock().unwrap();
    let data = store.iter().map(|(id, r)| r.project(id)).collect();
    Json(Page::single(data))
}

async fn update_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<Value>,
) -> Result<Json<Deployment>, WireError> {
    let mut store = state.deployments.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(|| not_found("deployment"))?;
    if let Some(agent) = body.get("agent") {
        record.agent = normalize_agent(agent)?;
    }
    if let Some(env) = value_opt_string(&body, "environment_id") {
        record.environment_id = env;
    }
    if let Some(name) = value_opt_string(&body, "name") {
        record.name = name;
    }
    if body.get("description").is_some() {
        record.description = value_opt_string(&body, "description");
    }
    if body.get("metadata").is_some() {
        record.metadata = value_metadata(&body, "metadata");
    }
    if body.get("initial_events").is_some() {
        record.initial_events = value_array(&body, "initial_events");
    }
    if body.get("resources").is_some() {
        record.resources = value_array(&body, "resources");
    }
    if body.get("schedule").is_some() {
        record.schedule = body.get("schedule").filter(|v| !v.is_null()).cloned();
    }
    if body.get("vault_ids").is_some() {
        record.vault_ids = value_str_array(&body, "vault_ids");
    }
    Ok(Json(record.project(&id)))
}

async fn archive_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
) -> Result<Json<Deployment>, WireError> {
    let mut store = state.deployments.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(|| not_found("deployment"))?;
    record.archived_at = Some(OBJECT_AT.to_string());
    Ok(Json(record.project(&id)))
}

async fn pause_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
) -> Result<Json<Deployment>, WireError> {
    let mut store = state.deployments.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(|| not_found("deployment"))?;
    record.status = "paused";
    record.paused_reason = Some(json!({ "type": "manual" }));
    Ok(Json(record.project(&id)))
}

async fn unpause_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
) -> Result<Json<Deployment>, WireError> {
    let mut store = state.deployments.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(|| not_found("deployment"))?;
    record.status = "active";
    record.paused_reason = None;
    Ok(Json(record.project(&id)))
}

/// `POST /v1/deployments/:id/run` — trigger a manual run, minting a
/// `deployment_run` bound to the deployment's agent.
async fn run_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
) -> Result<Json<DeploymentRun>, WireError> {
    let agent = {
        let store = state.deployments.lock().unwrap();
        let record = store.get(&id).ok_or_else(|| not_found("deployment"))?;
        record.agent.clone()
    };
    let n = state.run_seq.fetch_add(1, Ordering::SeqCst);
    let run_id = format!("deprun_{n:016}");
    let record = RunRecord {
        deployment_id: id,
        agent,
    };
    let projected = record.project(&run_id);
    state.runs.lock().unwrap().insert(run_id, record);
    Ok(Json(projected))
}

async fn retrieve_run(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
) -> Result<Json<DeploymentRun>, WireError> {
    let store = state.runs.lock().unwrap();
    let record = store.get(&id).ok_or_else(|| not_found("deployment_run"))?;
    Ok(Json(record.project(&id)))
}

/// `GET /v1/deployment_runs?deployment_id=…` — the runs, optionally filtered by
/// deployment, ascending id order.
async fn list_runs(
    State(state): State<Arc<DeploymentState>>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Json<Page<DeploymentRun>> {
    let filter = q.get("deployment_id");
    let store = state.runs.lock().unwrap();
    let data = store
        .iter()
        .filter(|(_, r)| filter.is_none_or(|d| &r.deployment_id == d))
        .map(|(id, r)| r.project(id))
        .collect();
    Json(Page::single(data))
}
