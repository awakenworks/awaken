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
use serde_json::Value;

use crate::routes::ManagedJson;
use crate::types::agent::AgentReference;
use crate::types::deployment::{
    Deployment, DeploymentCreateParams, DeploymentRun, DeploymentUpdateParams, PausedReason,
    TriggerContext,
};
use crate::types::{ErrorResponse, Page, PageQuery, paginate};

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

#[derive(Clone)]
struct DeploymentRecord {
    agent: AgentReference,
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
    paused_reason: Option<PausedReason>,
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
    agent: AgentReference,
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
            trigger_context: TriggerContext::Manual,
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
            "/v1/deployments/{id}",
            get(retrieve_deployment).post(update_deployment),
        )
        .route("/v1/deployments/{id}/archive", post(archive_deployment))
        .route("/v1/deployments/{id}/pause", post(pause_deployment))
        .route("/v1/deployments/{id}/unpause", post(unpause_deployment))
        .route("/v1/deployments/{id}/run", post(run_deployment))
        .route("/v1/deployment_runs", get(list_runs))
        .route("/v1/deployment_runs/{id}", get(retrieve_run))
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

async fn create_deployment(
    State(state): State<Arc<DeploymentState>>,
    ManagedJson(params): ManagedJson<DeploymentCreateParams>,
) -> Result<Json<Deployment>, WireError> {
    let record = DeploymentRecord {
        agent: AgentReference::from_input(&params.agent),
        environment_id: params.environment_id,
        name: params.name,
        description: params.description,
        metadata: params.metadata,
        initial_events: params.initial_events,
        resources: params.resources,
        schedule: params.schedule.filter(|v| !v.is_null()),
        vault_ids: params.vault_ids,
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

async fn list_deployments(
    State(state): State<Arc<DeploymentState>>,
    Query(page): Query<PageQuery>,
) -> Json<Page<Deployment>> {
    let store = state.deployments.lock().unwrap();
    let data: Vec<Deployment> = store.iter().map(|(id, r)| r.project(id)).collect();
    Json(paginate(data, &page, |d| d.id.as_str()))
}

async fn update_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<DeploymentUpdateParams>,
) -> Result<Json<Deployment>, WireError> {
    let mut store = state.deployments.lock().unwrap();
    let record = store.get_mut(&id).ok_or_else(|| not_found("deployment"))?;
    if let Some(agent) = &params.agent {
        record.agent = AgentReference::from_input(agent);
    }
    if let Some(env) = params.environment_id {
        record.environment_id = env;
    }
    if let Some(name) = params.name {
        record.name = name;
    }
    if let Some(description) = params.description {
        record.description = Some(description);
    }
    if let Some(metadata) = params.metadata {
        record.metadata = metadata;
    }
    if let Some(initial_events) = params.initial_events {
        record.initial_events = initial_events;
    }
    if let Some(resources) = params.resources {
        record.resources = resources;
    }
    if let Some(schedule) = params.schedule {
        record.schedule = Some(schedule).filter(|v| !v.is_null());
    }
    if let Some(vault_ids) = params.vault_ids {
        record.vault_ids = vault_ids;
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
    record.paused_reason = Some(PausedReason::Manual);
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
    Query(page): Query<PageQuery>,
) -> Json<Page<DeploymentRun>> {
    let filter = q.get("deployment_id");
    let store = state.runs.lock().unwrap();
    let data: Vec<DeploymentRun> = store
        .iter()
        .filter(|(_, r)| filter.is_none_or(|d| &r.deployment_id == d))
        .map(|(id, r)| r.project(id))
        .collect();
    Json(paginate(data, &page, |r| r.id.as_str()))
}
