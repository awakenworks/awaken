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
    /// RFC 3339 of the schedule's last fire (echoed into the schedule object).
    last_run_at: Option<String>,
    /// The next scheduled fire instant (epoch ms); lazily seeded on the first tick
    /// so a just-created deployment doesn't fire retroactively.
    next_fire_ms: Option<u64>,
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
            schedule: self.projected_schedule(),
            status: self.status,
            vault_ids: self.vault_ids.clone(),
        }
    }

    /// The schedule object echoed back, with `last_run_at` reflecting the most
    /// recent fire (the stored expression/timezone pass through unchanged).
    fn projected_schedule(&self) -> Option<Value> {
        let mut sched = self.schedule.clone()?;
        if let (Some(obj), Some(last)) = (sched.as_object_mut(), &self.last_run_at) {
            obj.insert("last_run_at".into(), Value::String(last.clone()));
        }
        Some(sched)
    }

    /// The parsed cron for an active, non-archived deployment; `None` when it has no
    /// schedule, is paused/archived, or the expression doesn't parse (already
    /// rejected at write time, so this is belt-and-suspenders).
    fn active_cron(&self) -> Option<crate::cron::Cron> {
        if self.status != "active" || self.archived_at.is_some() {
            return None;
        }
        let expr = self.schedule.as_ref()?.get("expression")?.as_str()?;
        crate::cron::Cron::parse(expr).ok()
    }
}

#[derive(Clone)]
struct RunRecord {
    deployment_id: String,
    agent: AgentReference,
    trigger: TriggerContext,
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
            trigger_context: self.trigger.clone(),
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

    /// Advance every active schedule to `now_ms`, minting a `deployment_run` (with a
    /// `Schedule` trigger context) for each occurrence that has come due since the
    /// last tick. The cursor is seeded to the first occurrence *after* the first tick
    /// so a freshly created deployment never fires retroactively; a slow tick that
    /// spans several occurrences fires each of them (catch-up). Returns the ids
    /// fired. This is the timed-trigger driver a background loop calls on an interval.
    pub fn tick(&self, now_ms: u64) -> Vec<String> {
        let mut fired = Vec::new();
        let mut deployments = self.deployments.lock().unwrap();
        let mut runs = self.runs.lock().unwrap();
        for (dep_id, record) in deployments.iter_mut() {
            let Some(cron) = record.active_cron() else {
                continue;
            };
            // Seed the cursor to the first occurrence after now on the first tick.
            let mut cursor = match record.next_fire_ms {
                Some(c) => c,
                None => match cron.next_after(now_ms) {
                    Some(c) => c,
                    None => continue,
                },
            };
            while cursor <= now_ms {
                let n = self.run_seq.fetch_add(1, Ordering::SeqCst);
                let run_id = format!("deprun_{n:016}");
                let scheduled_at = crate::cron::to_rfc3339(cursor);
                runs.insert(
                    run_id.clone(),
                    RunRecord {
                        deployment_id: dep_id.clone(),
                        agent: record.agent.clone(),
                        trigger: TriggerContext::Schedule {
                            scheduled_at: scheduled_at.clone(),
                        },
                    },
                );
                record.last_run_at = Some(scheduled_at);
                fired.push(run_id);
                cursor = match cron.next_after(cursor) {
                    Some(c) => c,
                    None => break,
                };
            }
            record.next_fire_ms = Some(cursor);
        }
        fired
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

/// Validate a schedule payload: when present, its `expression` must be a
/// well-formed 5-field cron (the SDK's `BetaManagedAgentsSchedule`). A malformed
/// schedule is rejected at write time rather than silently stored.
fn validate_schedule(schedule: &Option<Value>) -> Result<(), WireError> {
    let Some(sched) = schedule.as_ref().filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let expr = sched
        .get("expression")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "schedule requires a string `expression`",
                )),
            )
        })?;
    crate::cron::Cron::parse(expr).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!("invalid cron schedule: {e}"),
            )),
        )
    })?;
    Ok(())
}

async fn create_deployment(
    State(state): State<Arc<DeploymentState>>,
    ManagedJson(params): ManagedJson<DeploymentCreateParams>,
) -> Result<Json<Deployment>, WireError> {
    validate_schedule(&params.schedule)?;
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
        last_run_at: None,
        next_fire_ms: None,
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
    if let Some(schedule) = &params.schedule {
        validate_schedule(&Some(schedule.clone()))?;
    }
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
        record.next_fire_ms = None; // re-seed the cursor against the new schedule
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
        trigger: TriggerContext::Manual,
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

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-01-05 09:00:00 UTC (a Monday).
    const MON_0900: u64 = 1_767_603_600_000;

    fn cron_schedule(expr: &str) -> Value {
        serde_json::json!({ "type": "cron", "expression": expr, "timezone": "UTC" })
    }

    fn deployment(schedule: Option<Value>) -> DeploymentRecord {
        DeploymentRecord {
            agent: AgentReference::new("coder", 1),
            environment_id: "env_a".into(),
            name: "nightly".into(),
            description: None,
            metadata: BTreeMap::new(),
            initial_events: Vec::new(),
            resources: Vec::new(),
            schedule,
            vault_ids: Vec::new(),
            status: "active",
            paused_reason: None,
            archived_at: None,
            last_run_at: None,
            next_fire_ms: None,
        }
    }

    #[test]
    fn validate_schedule_rejects_a_malformed_cron() {
        assert!(validate_schedule(&None).is_ok(), "no schedule is fine");
        assert!(validate_schedule(&Some(cron_schedule("0 9 * * 1-5"))).is_ok());
        assert!(validate_schedule(&Some(cron_schedule("not a cron"))).is_err());
        assert!(
            validate_schedule(&Some(serde_json::json!({ "timezone": "UTC" }))).is_err(),
            "a schedule without an expression is rejected"
        );
    }

    #[test]
    fn tick_fires_due_occurrences_with_a_schedule_trigger() {
        let state = DeploymentState::new();
        state.deployments.lock().unwrap().insert(
            "deploy_x".into(),
            deployment(Some(cron_schedule("*/15 * * * *"))),
        );

        // First tick seeds the cursor to the next occurrence AFTER now — no
        // retroactive fire.
        assert!(state.tick(MON_0900).is_empty(), "no retroactive fire");
        // A later tick spanning two 15-minute occurrences fires both (catch-up).
        let fired = state.tick(MON_0900 + 31 * 60_000);
        assert_eq!(fired.len(), 2, "09:15 and 09:30 both come due");

        let runs = state.runs.lock().unwrap();
        let run = runs.get(&fired[0]).expect("run recorded");
        match &run.trigger {
            TriggerContext::Schedule { scheduled_at } => {
                assert_eq!(scheduled_at, "2026-01-05T09:15:00Z");
            }
            TriggerContext::Manual => panic!("a scheduled fire must carry a Schedule trigger"),
        }
        assert_eq!(run.deployment_id, "deploy_x");
        // A paused deployment stops firing.
        drop(runs);
        state
            .deployments
            .lock()
            .unwrap()
            .get_mut("deploy_x")
            .unwrap()
            .status = "paused";
        assert!(
            state.tick(MON_0900 + 120 * 60_000).is_empty(),
            "a paused deployment does not fire"
        );
    }

    #[test]
    fn last_run_at_is_echoed_into_the_projected_schedule() {
        let state = DeploymentState::new();
        state.deployments.lock().unwrap().insert(
            "deploy_y".into(),
            deployment(Some(cron_schedule("*/15 * * * *"))),
        );
        state.tick(MON_0900);
        state.tick(MON_0900 + 16 * 60_000);
        let store = state.deployments.lock().unwrap();
        let projected = store.get("deploy_y").unwrap().project("deploy_y");
        let sched = projected.schedule.expect("schedule present");
        assert_eq!(sched["last_run_at"], "2026-01-05T09:15:00Z");
    }
}
