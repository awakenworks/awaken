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

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::routes::{ManagedJson, WorkspaceScope};
use crate::types::agent::AgentReference;
use crate::types::deployment::{
    Deployment, DeploymentCreateParams, DeploymentInitialEvent, DeploymentRun,
    DeploymentUpdateParams, PausedReason, RunError, Schedule, TriggerContext,
};
use crate::types::resource::ResourceInput;
use crate::types::{ErrorResponse, Page, PageQuery, paginate};

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

#[derive(Clone)]
struct DeploymentRecord {
    workspace_id: String,
    agent: AgentReference,
    environment_id: String,
    name: String,
    description: Option<String>,
    metadata: BTreeMap<String, String>,
    initial_events: Vec<DeploymentInitialEvent>,
    resources: Vec<ResourceInput>,
    schedule: Option<Schedule>,
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
    fn projected_schedule(&self) -> Option<Schedule> {
        self.schedule
            .as_ref()
            .map(|schedule| schedule.with_last_run_at(self.last_run_at.clone()))
    }

    /// The parsed cron for an active, non-archived deployment; `None` when it has no
    /// schedule, is paused/archived, or the expression doesn't parse (already
    /// rejected at write time, so this is belt-and-suspenders).
    fn active_cron(&self) -> Option<crate::cron::Cron> {
        if self.status != "active" || self.archived_at.is_some() {
            return None;
        }
        let expr = self.schedule.as_ref()?.expression();
        crate::cron::Cron::parse(expr).ok()
    }

    fn launch(&self, deployment_id: &str) -> DeploymentLaunch {
        DeploymentLaunch {
            deployment_id: deployment_id.to_string(),
            workspace_id: self.workspace_id.clone(),
            agent: self.agent.clone(),
            environment_id: self.environment_id.clone(),
            metadata: self.metadata.clone(),
            initial_events: self.initial_events.clone(),
            resources: self.resources.clone(),
            vault_ids: self.vault_ids.clone(),
        }
    }
}

#[derive(Clone)]
struct RunRecord {
    deployment_id: String,
    agent: AgentReference,
    trigger: TriggerContext,
    session_id: Option<String>,
    error: Option<RunError>,
}

impl RunRecord {
    fn project(&self, id: &str) -> DeploymentRun {
        DeploymentRun {
            id: id.to_string(),
            object_type: "deployment_run",
            agent: self.agent.clone(),
            created_at: OBJECT_AT.to_string(),
            deployment_id: self.deployment_id.clone(),
            error: self.error.clone(),
            session_id: self.session_id.clone(),
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
    launcher: Mutex<Option<Arc<dyn DeploymentSessionLauncher>>>,
}

/// Input passed from the deployment application service to the Session boundary.
#[derive(Debug, Clone)]
pub struct DeploymentLaunch {
    pub deployment_id: String,
    pub workspace_id: String,
    pub agent: AgentReference,
    pub environment_id: String,
    pub metadata: BTreeMap<String, String>,
    pub initial_events: Vec<DeploymentInitialEvent>,
    pub resources: Vec<ResourceInput>,
    pub vault_ids: Vec<String>,
}

/// A launch always reports whether a Session was created; an initial event may
/// still fail after creation, in which case both `session_id` and `error` are set.
#[derive(Debug, Clone, Default)]
pub struct DeploymentLaunchOutcome {
    pub session_id: Option<String>,
    pub error: Option<String>,
}

#[async_trait::async_trait]
pub trait DeploymentSessionLauncher: Send + Sync {
    async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome;
}

impl DeploymentState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the Session application service after both control and data planes
    /// have been assembled. The state is shared by the already-mounted router.
    pub fn bind_launcher(&self, launcher: Arc<dyn DeploymentSessionLauncher>) {
        *self.launcher.lock().unwrap() = Some(launcher);
    }

    async fn launch_run(&self, run_id: &str, launch: DeploymentLaunch) -> DeploymentRun {
        let launcher = self.launcher.lock().unwrap().clone();
        let outcome = match launcher {
            Some(launcher) => launcher.launch(launch).await,
            None => DeploymentLaunchOutcome {
                session_id: None,
                error: Some("deployment Session launcher is not bound".to_string()),
            },
        };
        let mut runs = self.runs.lock().unwrap();
        let record = runs
            .get_mut(run_id)
            .expect("deployment run was inserted before launch");
        record.session_id = outcome.session_id;
        record.error = outcome.error.map(|message| RunError {
            kind: "api_error".to_string(),
            message,
        });
        record.project(run_id)
    }

    /// Fire due schedule occurrences and launch each through the same Session port
    /// as a manual run. Returns the completed run projections for observability.
    pub async fn tick_and_launch(&self, now_ms: u64) -> Vec<DeploymentRun> {
        let run_ids = self.tick(now_ms);
        let launches: Vec<(String, DeploymentLaunch)> = {
            let runs = self.runs.lock().unwrap();
            let deployments = self.deployments.lock().unwrap();
            run_ids
                .into_iter()
                .filter_map(|run_id| {
                    let deployment_id = runs.get(&run_id)?.deployment_id.clone();
                    let launch = deployments.get(&deployment_id)?.launch(&deployment_id);
                    Some((run_id, launch))
                })
                .collect()
        };
        let mut completed = Vec::with_capacity(launches.len());
        for (run_id, launch) in launches {
            completed.push(self.launch_run(&run_id, launch).await);
        }
        completed
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
                        session_id: None,
                        error: None,
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
fn validate_schedule(schedule: Option<&Schedule>) -> Result<(), WireError> {
    let Some(schedule) = schedule else {
        return Ok(());
    };
    let expr = schedule.expression();
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
    scope: Option<Extension<WorkspaceScope>>,
    ManagedJson(params): ManagedJson<DeploymentCreateParams>,
) -> Result<Json<Deployment>, WireError> {
    validate_schedule(params.schedule.as_ref())?;
    let record = DeploymentRecord {
        workspace_id: scope
            .map(|Extension(scope)| scope.0)
            .unwrap_or_else(|| crate::state::DEFAULT_SCOPE.to_string()),
        agent: AgentReference::from_input(&params.agent),
        environment_id: params.environment_id,
        name: params.name,
        description: params.description,
        metadata: params.metadata,
        initial_events: params.initial_events,
        resources: params.resources,
        schedule: params.schedule,
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
    if let Some(Some(schedule)) = &params.schedule {
        validate_schedule(Some(schedule))?;
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
        record.description = description;
    }
    if let Some(metadata) = params.metadata {
        match metadata {
            None => record.metadata.clear(),
            Some(patch) => {
                for (key, value) in patch {
                    match value {
                        Some(value) => {
                            record.metadata.insert(key, value);
                        }
                        None => {
                            record.metadata.remove(&key);
                        }
                    }
                }
            }
        }
    }
    if let Some(initial_events) = params.initial_events {
        record.initial_events = initial_events;
    }
    if let Some(resources) = params.resources {
        record.resources = resources.unwrap_or_default();
    }
    if let Some(schedule) = params.schedule {
        record.schedule = schedule;
        record.next_fire_ms = None; // re-seed the cursor against the new schedule
    }
    if let Some(vault_ids) = params.vault_ids {
        record.vault_ids = vault_ids.unwrap_or_default();
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
    let launch = {
        let store = state.deployments.lock().unwrap();
        let record = store.get(&id).ok_or_else(|| not_found("deployment"))?;
        record.launch(&id)
    };
    let n = state.run_seq.fetch_add(1, Ordering::SeqCst);
    let run_id = format!("deprun_{n:016}");
    let record = RunRecord {
        deployment_id: id,
        agent: launch.agent.clone(),
        trigger: TriggerContext::Manual,
        session_id: None,
        error: None,
    };
    state.runs.lock().unwrap().insert(run_id.clone(), record);
    Ok(Json(state.launch_run(&run_id, launch).await))
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

#[async_trait::async_trait]
impl DeploymentSessionLauncher for crate::ManagedState {
    async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
        // Admission already decoded the exact deployment-event subset. Lower it
        // directly into the shared Session event command; no second JSON parser.
        let events = crate::types::SendEventsRequest {
            events: request.initial_events.into_iter().map(Into::into).collect(),
        };
        let mut metadata = request.metadata;
        metadata.insert(
            "awaken.deployment_id".to_string(),
            request.deployment_id.clone(),
        );
        let create = crate::types::SessionCreateParams {
            agent: crate::types::AgentRef::Object(crate::types::AgentRefObject {
                id: request.agent.id,
                kind: Some(crate::types::AgentRefKind::Agent),
                version: Some(request.agent.version as u32),
                system: None,
                tools: None,
                mcp_servers: None,
                skills: None,
                model: None,
            }),
            application_contribution_required: false,
            environment_id: Some(request.environment_id),
            title: None,
            metadata,
            mcp_servers: Vec::new(),
            vault_ids: request.vault_ids,
            resources: request.resources,
        };
        let session = match self
            .create_session(create, Some(request.workspace_id))
            .await
        {
            Ok(session) => session,
            Err(error) => {
                return DeploymentLaunchOutcome {
                    session_id: None,
                    error: Some(error.to_string()),
                };
            }
        };
        if events.events.is_empty() {
            return DeploymentLaunchOutcome {
                session_id: Some(session.id),
                error: None,
            };
        }
        match self.send_events(&session.id, events).await {
            Ok(_) => DeploymentLaunchOutcome {
                session_id: Some(session.id),
                error: None,
            },
            Err(error) => DeploymentLaunchOutcome {
                session_id: Some(session.id),
                error: Some(error.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-01-05 09:00:00 UTC (a Monday).
    const MON_0900: u64 = 1_767_603_600_000;

    fn cron_schedule(expr: &str) -> Schedule {
        Schedule::Cron {
            expression: expr.into(),
            timezone: "UTC".into(),
            last_run_at: None,
            upcoming_runs_at: Vec::new(),
        }
    }

    fn deployment(schedule: Option<Schedule>) -> DeploymentRecord {
        DeploymentRecord {
            workspace_id: "default".into(),
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
        // schedule JSON -> typed union -> cron semantic validation -> store/no store
        //
        // | tag  | required fields | expression | result |
        // |------|-----------------|------------|--------|
        // | none | -               | -          | accept |
        // | cron | all             | valid      | accept |
        // | cron | all             | invalid    | reject |
        // | cron | missing         | -          | decode reject |
        assert!(validate_schedule(None).is_ok(), "no schedule is fine");
        let valid = cron_schedule("0 9 * * 1-5");
        let invalid = cron_schedule("not a cron");
        assert!(validate_schedule(Some(&valid)).is_ok());
        assert!(validate_schedule(Some(&invalid)).is_err());
        assert!(
            serde_json::from_value::<Schedule>(
                serde_json::json!({ "type":"cron", "timezone": "UTC" })
            )
            .is_err(),
            "a schedule without an expression is rejected at admission"
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

    #[tokio::test]
    async fn scheduled_occurrence_launches_a_real_session_through_the_port() {
        struct Launcher;
        #[async_trait::async_trait]
        impl DeploymentSessionLauncher for Launcher {
            async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
                DeploymentLaunchOutcome {
                    session_id: Some(format!("sesn_{}", request.deployment_id)),
                    error: None,
                }
            }
        }

        let state = DeploymentState::new();
        state.bind_launcher(Arc::new(Launcher));
        state.deployments.lock().unwrap().insert(
            "deploy_schedule".into(),
            deployment(Some(cron_schedule("*/15 * * * *"))),
        );
        assert!(state.tick_and_launch(MON_0900).await.is_empty());
        let completed = state.tick_and_launch(MON_0900 + 16 * 60_000).await;
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0].session_id.as_deref(),
            Some("sesn_deploy_schedule")
        );
        assert!(completed[0].error.is_none());
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
        let Schedule::Cron { last_run_at, .. } = projected.schedule.expect("schedule present");
        assert_eq!(last_run_at.as_deref(), Some("2026-01-05T09:15:00Z"));
    }
}
