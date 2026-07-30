//! The Managed **deployments** family (`/v1/deployments`) + **deployment runs**
//! (`/v1/deployment_runs`), the official `@anthropic-ai/sdk` `beta.deployments.*`
//! and `beta.deploymentRuns.*` surfaces. A deployment binds an agent to an
//! environment with initial events + a schedule; `run` triggers a
//! `deployment_run`; `pause`/`unpause` toggle the schedule; `archive` soft-deletes.
//!
//! The aggregate is restored through the neutral Deployment repository; its
//! process-local projection is only the synchronized working set. Exact scheduled
//! occurrences are atomically claimed by the repository before Session launch.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, FixedOffset};
use chrono_tz::Tz;
use serde::Deserialize;
use serde::Serialize;

use awaken_deployment_contract::{
    DeploymentRecord as StoredDeployment, DeploymentRepository, DeploymentRepositoryError,
    DeploymentRunRecord as StoredDeploymentRun,
};
use awaken_session_contract::ManagedLifecycleFact;

use crate::ManagedRateLimiter;
use crate::routes::agents_registry::{ManagedAgentError, ManagedAgentRepository};
use crate::routes::{ManagedJson, WorkspaceScope};
use crate::types::agent::{AgentReference, AgentStatus};
use crate::types::deployment::{
    Deployment, DeploymentCreateParams, DeploymentInitialEvent, DeploymentRun,
    DeploymentUpdateParams, PausedReason, RunError, Schedule, TriggerContext,
};
use crate::types::resource::ResourceInput;
use crate::types::{ErrorResponse, Page, PageQuery, paginate};

#[cfg(test)]
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn parsed_schedule(schedule: &Schedule) -> Option<(crate::cron::Cron, Tz)> {
    Some((
        crate::cron::Cron::parse(schedule.expression()).ok()?,
        schedule.timezone().parse().ok()?,
    ))
}

fn upcoming_occurrences(schedule: &Schedule, after_ms: u64) -> Vec<String> {
    let Some((cron, timezone)) = parsed_schedule(schedule) else {
        return Vec::new();
    };
    let mut cursor = after_ms;
    (0..5)
        .filter_map(|_| {
            cursor = cron.next_after_in(cursor, timezone)?;
            Some(crate::cron::to_rfc3339(cursor))
        })
        .collect()
}

fn next_occurrence(schedule: &Schedule, after_ms: u64) -> Option<u64> {
    let (cron, timezone) = parsed_schedule(schedule)?;
    cron.next_after_in(after_ms, timezone)
}

const MAX_SCHEDULED_DEPLOYMENTS: usize = 1_000;
const MIN_JITTER_MS: u64 = 5_000;
const MAX_JITTER_MS: u64 = 9 * 60_000;

/// Stable execution delay for one exact cron occurrence. The window is 15% of
/// the interval, bounded to the documented 5 seconds–9 minutes. Stability avoids
/// changing a pending fire's due instant when a process restarts.
fn execution_jitter_ms(deployment_id: &str, scheduled_ms: u64, interval_ms: u64) -> u64 {
    let window = interval_ms
        .saturating_mul(15)
        .checked_div(100)
        .unwrap_or_default()
        .clamp(MIN_JITTER_MS, MAX_JITTER_MS);
    if window == MIN_JITTER_MS {
        return window;
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in deployment_id.bytes().chain(scheduled_ms.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    MIN_JITTER_MS + hash % (window - MIN_JITTER_MS + 1)
}

#[derive(Clone, Serialize, Deserialize)]
struct DeploymentRecord {
    created_at: String,
    updated_at: String,
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
    status: String,
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
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            description: self.description.clone(),
            environment_id: self.environment_id.clone(),
            initial_events: self.initial_events.clone(),
            metadata: self.metadata.clone(),
            name: self.name.clone(),
            paused_reason: self.paused_reason.clone(),
            resources: self.resources.clone(),
            schedule: self.projected_schedule(),
            status: if self.status == "active" {
                "active"
            } else {
                "paused"
            },
            vault_ids: self.vault_ids.clone(),
        }
    }

    /// The schedule object echoed back, with `last_run_at` reflecting the most
    /// recent fire (the stored expression/timezone pass through unchanged).
    fn projected_schedule(&self) -> Option<Schedule> {
        self.schedule.as_ref().map(|schedule| {
            let upcoming = if self.archived_at.is_some() {
                Vec::new()
            } else {
                upcoming_occurrences(schedule, now_ms())
            };
            schedule.with_runtime(self.last_run_at.clone(), upcoming)
        })
    }

    /// The parsed cron for an active, non-archived deployment; `None` when it has no
    /// schedule, is paused/archived, or the expression doesn't parse (already
    /// rejected at write time, so this is belt-and-suspenders).
    fn active_cron(&self) -> Option<(crate::cron::Cron, Tz)> {
        if self.status != "active" || self.archived_at.is_some() {
            return None;
        }
        parsed_schedule(self.schedule.as_ref()?)
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

#[derive(Clone, Serialize, Deserialize)]
struct RunRecord {
    created_at: String,
    deployment_id: String,
    workspace_id: String,
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
            created_at: self.created_at.clone(),
            deployment_id: self.deployment_id.clone(),
            error: self.error.clone(),
            session_id: self.session_id.clone(),
            trigger_context: self.trigger.clone(),
        }
    }
}

/// The deployments + deployment-runs state.
pub struct DeploymentState {
    deployments: Mutex<BTreeMap<String, DeploymentRecord>>,
    runs: Mutex<BTreeMap<String, RunRecord>>,
    dep_seq: AtomicU64,
    run_seq: AtomicU64,
    launcher: Mutex<Option<Arc<dyn DeploymentSessionLauncher>>>,
    rate_limiter: Mutex<Option<Arc<ManagedRateLimiter>>>,
    repository: Option<Arc<dyn DeploymentRepository>>,
    agent_repository: Mutex<Option<Arc<dyn ManagedAgentRepository>>>,
    scheduled_limit: usize,
}

impl Default for DeploymentState {
    fn default() -> Self {
        Self {
            deployments: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(BTreeMap::new()),
            dep_seq: AtomicU64::new(0),
            run_seq: AtomicU64::new(0),
            launcher: Mutex::new(None),
            rate_limiter: Mutex::new(None),
            repository: None,
            agent_repository: Mutex::new(None),
            scheduled_limit: MAX_SCHEDULED_DEPLOYMENTS,
        }
    }
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

/// Exact terminal outcome of Session creation. The enum makes the SDK invariant
/// structural: exactly one of `session_id` and `error` is non-null. Failures after
/// a Session was created are Session lifecycle, not deployment-run truth.
#[derive(Debug, Clone)]
pub enum DeploymentLaunchOutcome {
    Created { session_id: String },
    Failed { error: RunError },
}

#[async_trait::async_trait]
pub trait DeploymentSessionLauncher: Send + Sync {
    async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome;
}

#[path = "deployments/scheduling.rs"]
mod scheduling;
#[cfg(test)]
use scheduling::stored_deployment;

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

fn request_scope(scope: &Option<Extension<WorkspaceScope>>) -> String {
    scope.as_ref().map_or_else(
        || crate::state::DEFAULT_SCOPE.to_string(),
        |workspace| workspace.0.0.clone(),
    )
}

/// Exact `DeploymentRunListParams`. Keeping query admission typed prevents
/// unsupported fields and malformed filter values from being silently ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentRunListParams {
    #[serde(flatten)]
    page: PageQuery,
    /// The generated SDK currently serializes its beta selector as `beta` on
    /// list requests. The route-wide beta middleware remains authoritative; this
    /// field is admitted only so the typed query matches the official client.
    #[serde(default, rename = "beta")]
    _beta: Option<String>,
    #[serde(default, rename = "created_at[gt]")]
    created_at_gt: Option<DateTime<FixedOffset>>,
    #[serde(default, rename = "created_at[gte]")]
    created_at_gte: Option<DateTime<FixedOffset>>,
    #[serde(default, rename = "created_at[lt]")]
    created_at_lt: Option<DateTime<FixedOffset>>,
    #[serde(default, rename = "created_at[lte]")]
    created_at_lte: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    deployment_id: Option<String>,
    #[serde(default)]
    has_error: Option<bool>,
    #[serde(default)]
    trigger_type: Option<TriggerType>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentListParams {
    #[serde(flatten)]
    page: PageQuery,
    #[serde(default, rename = "beta")]
    _beta: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default, rename = "created_at[gte]")]
    created_at_gte: Option<DateTime<FixedOffset>>,
    #[serde(default, rename = "created_at[lte]")]
    created_at_lte: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    include_archived: bool,
    #[serde(default)]
    status: Option<DeploymentStatus>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeploymentStatus {
    Active,
    Paused,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TriggerType {
    Schedule,
    Manual,
}

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
    schedule.timezone().parse::<Tz>().map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                format!("invalid IANA timezone: {error}"),
            )),
        )
    })?;
    Ok(())
}

fn invalid(message: impl Into<String>) -> WireError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

fn repository_unavailable(error: impl std::fmt::Display) -> WireError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse::new("api_error", error.to_string())),
    )
}

fn agent_resolution_error(error: ManagedAgentError) -> WireError {
    match error {
        ManagedAgentError::NotFound => not_found("agent"),
        ManagedAgentError::Invalid(message) | ManagedAgentError::Conflict(message) => {
            invalid(message)
        }
        ManagedAgentError::Storage(message) => repository_unavailable(message),
    }
}

fn terminal() -> WireError {
    (
        StatusCode::CONFLICT,
        Json(ErrorResponse::new(
            "invalid_request_error",
            "archived deployment is terminal and cannot be modified",
        )),
    )
}

fn scheduled_count(store: &BTreeMap<String, DeploymentRecord>) -> usize {
    store
        .values()
        .filter(|record| record.archived_at.is_none() && record.schedule.is_some())
        .count()
}

fn ensure_scheduled_capacity(
    store: &BTreeMap<String, DeploymentRecord>,
    limit: usize,
) -> Result<(), WireError> {
    if scheduled_count(store) >= limit {
        return Err(invalid(format!(
            "an organization supports at most {limit} scheduled deployments"
        )));
    }
    Ok(())
}

fn resume_schedule(record: &mut DeploymentRecord, now_ms: u64) {
    record.status = "active".into();
    record.paused_reason = None;
    record.next_fire_ms = record
        .schedule
        .as_ref()
        .and_then(|schedule| next_occurrence(schedule, now_ms));
    record.updated_at = crate::cron::to_rfc3339(now_ms);
}

fn deployment_page(query: &PageQuery) -> Result<PageQuery, WireError> {
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(invalid("limit must be between 1 and 100"));
    }
    Ok(PageQuery {
        limit: Some(limit),
        page: query.page.clone(),
    })
}

/// SDK write-boundary invariants shared by create and the fully materialized
/// update candidate. Validation happens before replacing the aggregate so a
/// rejected patch has no partial effect.
fn validate_deployment(record: &DeploymentRecord) -> Result<(), WireError> {
    if record.name.trim().is_empty() {
        return Err(invalid("deployment name must be non-empty"));
    }
    crate::types::initial_event::validate_initial_events(
        &record.initial_events,
        &crate::types::initial_event::InitialEventPolicy {
            min_count: 1,
            max_count: 50,
            allow_system_message: true,
            max_outcomes: None,
            outcome_iterations: Some(1..=20),
        },
    )
    .map_err(invalid)?;
    if record.metadata.len() > 16
        || record
            .metadata
            .iter()
            .any(|(key, value)| key.chars().count() > 64 || value.chars().count() > 512)
    {
        return Err(invalid(
            "metadata allows at most 16 pairs, 64-character keys, and 512-character values",
        ));
    }
    if record.resources.len() > 500 {
        return Err(invalid("resources allows at most 500 entries"));
    }
    if record.vault_ids.len() > 50 {
        return Err(invalid("vault_ids allows at most 50 entries"));
    }
    Ok(())
}

async fn create_deployment(
    State(state): State<Arc<DeploymentState>>,
    scope: Option<Extension<WorkspaceScope>>,
    ManagedJson(params): ManagedJson<DeploymentCreateParams>,
) -> Result<Json<Deployment>, WireError> {
    validate_schedule(params.schedule.as_ref())?;
    let next_fire_ms = params
        .schedule
        .as_ref()
        .and_then(|schedule| next_occurrence(schedule, now_ms()));
    let workspace_id = scope
        .map(|Extension(scope)| scope.0)
        .unwrap_or_else(|| crate::state::DEFAULT_SCOPE.to_string());
    let agent = state.resolve_agent(&workspace_id, &params.agent).await?;
    let record = DeploymentRecord {
        created_at: crate::cron::to_rfc3339(now_ms()),
        updated_at: crate::cron::to_rfc3339(now_ms()),
        workspace_id,
        agent,
        environment_id: params.environment_id,
        name: params.name,
        description: params.description,
        metadata: params.metadata,
        initial_events: params.initial_events,
        resources: params.resources,
        schedule: params.schedule,
        vault_ids: params.vault_ids,
        status: "active".into(),
        paused_reason: None,
        archived_at: None,
        last_run_at: None,
        next_fire_ms,
    };
    validate_deployment(&record)?;
    let n = state.dep_seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("depl_{n:016}");
    let projected = record.project(&id);
    {
        let mut store = state.deployments.lock().unwrap();
        if record.schedule.is_some() {
            ensure_scheduled_capacity(&store, state.scheduled_limit)?;
        }
        store.insert(id.clone(), record.clone());
    }
    if let Err(error) = state
        .persist_deployment_event(&id, &record, "deployment.created")
        .await
    {
        state.deployments.lock().unwrap().remove(&id);
        return Err(repository_unavailable(error));
    }
    Ok(Json(projected))
}

async fn retrieve_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Deployment>, WireError> {
    let scope = request_scope(&scope);
    let store = state.deployments.lock().unwrap();
    let record = store
        .get(&id)
        .filter(|record| record.workspace_id == scope)
        .ok_or_else(|| not_found("deployment"))?;
    Ok(Json(record.project(&id)))
}

async fn list_deployments(
    State(state): State<Arc<DeploymentState>>,
    Query(query): Query<DeploymentListParams>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Page<Deployment>>, WireError> {
    let scope = request_scope(&scope);
    if query.include_archived && query.status.is_some() {
        return Err(invalid(
            "include_archived and status filters cannot be combined",
        ));
    }
    let page = deployment_page(&query.page)?;
    let store = state.deployments.lock().unwrap();
    let data: Vec<Deployment> = store
        .iter()
        .filter(|(_, record)| record.workspace_id == scope)
        .filter(|(_, record)| query.include_archived || record.archived_at.is_none())
        .filter(|(_, record)| {
            query
                .agent_id
                .as_ref()
                .is_none_or(|agent| &record.agent.id == agent)
        })
        .filter(|(_, record)| {
            query.status.is_none_or(|status| {
                matches!(
                    (record.status.as_str(), status),
                    ("active", DeploymentStatus::Active) | ("paused", DeploymentStatus::Paused)
                )
            })
        })
        .map(|(id, record)| record.project(id))
        .filter(|deployment| {
            let created = DateTime::parse_from_rfc3339(&deployment.created_at)
                .expect("stored deployment timestamp is valid RFC 3339");
            query.created_at_gte.is_none_or(|bound| created >= bound)
                && query.created_at_lte.is_none_or(|bound| created <= bound)
        })
        .collect();
    Ok(Json(paginate(data, &page, |d| d.id.as_str())))
}

async fn update_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
    ManagedJson(params): ManagedJson<DeploymentUpdateParams>,
) -> Result<Json<Deployment>, WireError> {
    if let Some(Some(schedule)) = &params.schedule {
        validate_schedule(Some(schedule))?;
    }
    let scope = request_scope(&scope);
    let current = state
        .deployments
        .lock()
        .unwrap()
        .get(&id)
        .filter(|record| record.workspace_id == scope)
        .ok_or_else(|| not_found("deployment"))?
        .clone();
    if current.archived_at.is_some() {
        return Err(terminal());
    }
    let mut candidate = current.clone();
    if let Some(agent) = &params.agent {
        candidate.agent = state.resolve_agent(&scope, agent).await?;
    }
    if let Some(env) = params.environment_id {
        candidate.environment_id = env;
    }
    if let Some(name) = params.name {
        candidate.name = name;
    }
    if let Some(description) = params.description {
        candidate.description = description;
    }
    if let Some(metadata) = params.metadata {
        match metadata {
            None => candidate.metadata.clear(),
            Some(patch) => {
                for (key, value) in patch {
                    match value {
                        Some(value) => {
                            candidate.metadata.insert(key, value);
                        }
                        None => {
                            candidate.metadata.remove(&key);
                        }
                    }
                }
            }
        }
    }
    if let Some(initial_events) = params.initial_events {
        candidate.initial_events = initial_events;
    }
    if let Some(resources) = params.resources {
        candidate.resources = resources.unwrap_or_default();
    }
    if let Some(schedule) = params.schedule {
        candidate.schedule = schedule;
        candidate.next_fire_ms = candidate
            .schedule
            .as_ref()
            .and_then(|schedule| next_occurrence(schedule, now_ms()));
    }
    if let Some(vault_ids) = params.vault_ids {
        candidate.vault_ids = vault_ids.unwrap_or_default();
    }
    validate_deployment(&candidate)?;
    if current.schedule.is_none() && candidate.schedule.is_some() {
        ensure_scheduled_capacity(&state.deployments.lock().unwrap(), state.scheduled_limit)?;
    }
    candidate.updated_at = crate::cron::to_rfc3339(now_ms());
    let projected = candidate.project(&id);
    state
        .persist_deployment_event(&id, &candidate, "deployment.updated")
        .await
        .map_err(repository_unavailable)?;
    state.deployments.lock().unwrap().insert(id, candidate);
    Ok(Json(projected))
}

async fn archive_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Deployment>, WireError> {
    let scope = request_scope(&scope);
    let mut candidate = state
        .deployments
        .lock()
        .unwrap()
        .get(&id)
        .filter(|record| record.workspace_id == scope)
        .ok_or_else(|| not_found("deployment"))?
        .clone();
    if candidate.archived_at.is_some() {
        return Ok(Json(candidate.project(&id)));
    }
    candidate.archived_at = Some(crate::cron::to_rfc3339(now_ms()));
    candidate.updated_at = crate::cron::to_rfc3339(now_ms());
    state
        .persist_deployment_event(&id, &candidate, "deployment.archived")
        .await
        .map_err(repository_unavailable)?;
    state
        .deployments
        .lock()
        .unwrap()
        .insert(id.clone(), candidate.clone());
    Ok(Json(candidate.project(&id)))
}

async fn pause_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Deployment>, WireError> {
    let scope = request_scope(&scope);
    let mut candidate = state
        .deployments
        .lock()
        .unwrap()
        .get(&id)
        .filter(|record| record.workspace_id == scope)
        .ok_or_else(|| not_found("deployment"))?
        .clone();
    if candidate.archived_at.is_some() {
        return Err(terminal());
    }
    candidate.status = "paused".into();
    candidate.paused_reason = Some(PausedReason::Manual);
    candidate.updated_at = crate::cron::to_rfc3339(now_ms());
    state
        .persist_deployment_event(&id, &candidate, "deployment.paused")
        .await
        .map_err(repository_unavailable)?;
    state
        .deployments
        .lock()
        .unwrap()
        .insert(id.clone(), candidate.clone());
    Ok(Json(candidate.project(&id)))
}

async fn unpause_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Deployment>, WireError> {
    let scope = request_scope(&scope);
    let mut candidate = state
        .deployments
        .lock()
        .unwrap()
        .get(&id)
        .filter(|record| record.workspace_id == scope)
        .ok_or_else(|| not_found("deployment"))?
        .clone();
    if candidate.archived_at.is_some() {
        return Err(terminal());
    }
    let now = now_ms();
    resume_schedule(&mut candidate, now);
    state
        .persist_deployment_event(&id, &candidate, "deployment.unpaused")
        .await
        .map_err(repository_unavailable)?;
    state
        .deployments
        .lock()
        .unwrap()
        .insert(id.clone(), candidate.clone());
    Ok(Json(candidate.project(&id)))
}

/// `POST /v1/deployments/:id/run` — trigger a manual run, minting a
/// `deployment_run` bound to the deployment's agent.
async fn run_deployment(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<DeploymentRun>, WireError> {
    let scope = request_scope(&scope);
    let launch = {
        let store = state.deployments.lock().unwrap();
        let record = store
            .get(&id)
            .filter(|record| record.workspace_id == scope)
            .ok_or_else(|| not_found("deployment"))?;
        if record.archived_at.is_some() {
            return Err(terminal());
        }
        record.launch(&id)
    };
    let n = state.run_seq.fetch_add(1, Ordering::SeqCst);
    let run_id = format!("drun_{n:016}");
    let record = RunRecord {
        created_at: crate::cron::to_rfc3339(now_ms()),
        deployment_id: id,
        workspace_id: launch.workspace_id.clone(),
        agent: launch.agent.clone(),
        trigger: TriggerContext::Manual,
        session_id: None,
        error: None,
    };
    state
        .runs
        .lock()
        .unwrap()
        .insert(run_id.clone(), record.clone());
    if let Err(error) = state
        .persist_run_event(&run_id, &record, "deployment_run.started")
        .await
    {
        state.runs.lock().unwrap().remove(&run_id);
        return Err(repository_unavailable(error));
    }
    Ok(Json(
        state
            .launch_run(&run_id, launch)
            .await
            .map_err(repository_unavailable)?,
    ))
}

async fn retrieve_run(
    State(state): State<Arc<DeploymentState>>,
    Path(id): Path<String>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<DeploymentRun>, WireError> {
    let scope = request_scope(&scope);
    let store = state.runs.lock().unwrap();
    let record = store
        .get(&id)
        .filter(|record| record.workspace_id == scope)
        .ok_or_else(|| not_found("deployment_run"))?;
    Ok(Json(record.project(&id)))
}

/// `GET /v1/deployment_runs?deployment_id=…` — the runs, optionally filtered by
/// deployment, ascending id order.
async fn list_runs(
    State(state): State<Arc<DeploymentState>>,
    Query(query): Query<DeploymentRunListParams>,
    scope: Option<Extension<WorkspaceScope>>,
) -> Result<Json<Page<DeploymentRun>>, WireError> {
    let scope = request_scope(&scope);
    let page = deployment_page(&query.page)?;
    let store = state.runs.lock().unwrap();
    let data: Vec<DeploymentRun> = store
        .iter()
        .filter(|(_, record)| record.workspace_id == scope)
        .filter(|(_, r)| {
            query
                .deployment_id
                .as_ref()
                .is_none_or(|deployment| &r.deployment_id == deployment)
        })
        .filter(|(_, r)| {
            query
                .has_error
                .is_none_or(|expected| r.error.is_some() == expected)
        })
        .filter(|(_, r)| {
            query.trigger_type.is_none_or(|expected| {
                matches!(
                    (&r.trigger, expected),
                    (TriggerContext::Manual, TriggerType::Manual)
                        | (TriggerContext::Schedule { .. }, TriggerType::Schedule)
                )
            })
        })
        .map(|(id, r)| r.project(id))
        .filter(|run| {
            let created = DateTime::parse_from_rfc3339(&run.created_at)
                .expect("stored deployment-run timestamp is valid RFC 3339");
            query.created_at_gt.is_none_or(|bound| created > bound)
                && query.created_at_gte.is_none_or(|bound| created >= bound)
                && query.created_at_lt.is_none_or(|bound| created < bound)
                && query.created_at_lte.is_none_or(|bound| created <= bound)
        })
        .collect();
    Ok(Json(paginate(data, &page, |r| r.id.as_str())))
}

/// Adapter from Deployment's narrow launch port to the canonical Managed
/// Session create command. It owns the `Arc` needed to enqueue initial Events.
pub struct ManagedDeploymentSessionLauncher(Arc<crate::ManagedState>);

impl ManagedDeploymentSessionLauncher {
    #[must_use]
    pub fn new(state: Arc<crate::ManagedState>) -> Self {
        Self(state)
    }
}

#[async_trait::async_trait]
impl DeploymentSessionLauncher for ManagedDeploymentSessionLauncher {
    async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
        if request.environment_id != "env_local"
            && let Some(environment) = self.0.deployment_environment(&request.environment_id).await
        {
            match environment {
                None => {
                    return DeploymentLaunchOutcome::Failed {
                        error: RunError::EnvironmentNotFoundError {
                            message: format!(
                                "environment `{}` no longer exists",
                                request.environment_id
                            ),
                        },
                    };
                }
                Some(environment) if environment.archived_at.is_some() => {
                    return DeploymentLaunchOutcome::Failed {
                        error: RunError::EnvironmentArchivedError {
                            message: format!(
                                "environment `{}` is archived",
                                request.environment_id
                            ),
                        },
                    };
                }
                Some(_) => {}
            }
        }
        if self
            .0
            .deployment_agent_unavailable(&request.workspace_id, &request.agent.id)
        {
            return DeploymentLaunchOutcome::Failed {
                error: RunError::AgentArchivedError {
                    message: format!("agent `{}` is archived", request.agent.id),
                },
            };
        }
        if let Some(delegate) = self
            .0
            .deployment_unavailable_delegate(&request.workspace_id, &request.agent.id)
        {
            return DeploymentLaunchOutcome::Failed {
                error: RunError::AgentArchivedError {
                    message: format!("subagent `{delegate}` is archived"),
                },
            };
        }
        // Admission already decoded the exact deployment-event subset. Lower it
        // into the ordinary Session create command so validation, persistence and
        // initial execution have one authority. A Deployment must never create an
        // empty Session and then drive a second best-effort send-events path.
        let initial_events = request.initial_events.into_iter().map(Into::into).collect();
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
            initial_events,
            application_contribution_required: false,
            environment_id: Some(request.environment_id),
            title: None,
            metadata,
            mcp_servers: Vec::new(),
            vault_ids: request.vault_ids,
            resources: request.resources,
        };
        let session = match self
            .0
            .create_session_with_initial_events(create, Some(request.workspace_id))
            .await
        {
            Ok(session) => session,
            Err(error) => {
                let error = match error {
                    crate::StateError::VaultNotFound(id) => RunError::VaultNotFoundError {
                        message: format!("vault `{id}` not found"),
                    },
                    crate::StateError::Run(error) if error.code == "mcp_egress_blocked" => {
                        RunError::McpEgressBlockedError {
                            message: error.message,
                        }
                    }
                    error => RunError::SessionCreationRejectedError {
                        message: error.to_string(),
                    },
                };
                return DeploymentLaunchOutcome::Failed { error };
            }
        };
        DeploymentLaunchOutcome::Created {
            session_id: session.id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ModelConfig;
    use crate::types::agent::{Agent, AgentCreateParams, AgentListParams, AgentUpdateParams};

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
            created_at: OBJECT_AT.into(),
            updated_at: OBJECT_AT.into(),
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
            status: "active".into(),
            paused_reason: None,
            archived_at: None,
            last_run_at: None,
            next_fire_ms: None,
        }
    }

    fn jitter_due(deployment_id: &str, scheduled_ms: u64, interval_ms: u64) -> u64 {
        scheduled_ms + execution_jitter_ms(deployment_id, scheduled_ms, interval_ms)
    }

    fn create_params(schedule: bool) -> DeploymentCreateParams {
        let mut value = serde_json::json!({
            "agent": "coder",
            "environment_id": "env_a",
            "name": "scheduled",
            "initial_events": [{
                "type": "user.message",
                "content": [{"type": "text", "text": "go"}]
            }]
        });
        if schedule {
            value["schedule"] = serde_json::json!({
                "type": "cron", "expression": "*/15 * * * *", "timezone": "UTC"
            });
        }
        serde_json::from_value(value).unwrap()
    }

    struct ResolvingAgentRepository {
        selected: Result<(u64, AgentStatus), ManagedAgentError>,
        requested_versions: Mutex<Vec<Option<u64>>>,
    }

    impl ResolvingAgentRepository {
        fn published(version: u64) -> Self {
            Self {
                selected: Ok((version, AgentStatus::Published)),
                requested_versions: Mutex::new(Vec::new()),
            }
        }

        fn projected(id: &str, version: u64, status: AgentStatus) -> Agent {
            Agent {
                id: id.into(),
                object_type: "agent",
                archived_at: (status == AgentStatus::Archived).then(|| OBJECT_AT.into()),
                disabled_at: (status == AgentStatus::Disabled).then(|| OBJECT_AT.into()),
                status,
                created_at: OBJECT_AT.into(),
                updated_at: OBJECT_AT.into(),
                name: "agent".into(),
                description: None,
                model: ModelConfig::new("claude-sonnet-5"),
                system: None,
                metadata: BTreeMap::new(),
                mcp_servers: Vec::new(),
                skills: Vec::new(),
                tools: Vec::new(),
                multiagent: None,
                version,
            }
        }
    }

    #[async_trait::async_trait]
    impl ManagedAgentRepository for ResolvingAgentRepository {
        async fn create(
            &self,
            _workspace_id: &str,
            _params: AgentCreateParams,
        ) -> Result<Agent, ManagedAgentError> {
            unreachable!()
        }

        async fn retrieve(
            &self,
            _workspace_id: &str,
            id: &str,
            version: Option<u64>,
        ) -> Result<Agent, ManagedAgentError> {
            self.requested_versions.lock().unwrap().push(version);
            match &self.selected {
                Ok((selected, status)) => Ok(Self::projected(id, *selected, *status)),
                Err(ManagedAgentError::NotFound) => Err(ManagedAgentError::NotFound),
                Err(error) => panic!("unexpected test error: {error}"),
            }
        }

        async fn list(
            &self,
            _workspace_id: &str,
            _params: &AgentListParams,
        ) -> Result<Vec<Agent>, ManagedAgentError> {
            unreachable!()
        }

        async fn update(
            &self,
            _workspace_id: &str,
            _id: &str,
            _params: AgentUpdateParams,
        ) -> Result<Agent, ManagedAgentError> {
            unreachable!()
        }

        async fn disable(
            &self,
            _workspace_id: &str,
            _id: &str,
        ) -> Result<Agent, ManagedAgentError> {
            unreachable!()
        }

        async fn archive(
            &self,
            _workspace_id: &str,
            _id: &str,
        ) -> Result<Agent, ManagedAgentError> {
            unreachable!()
        }

        async fn versions(
            &self,
            _workspace_id: &str,
            _id: &str,
        ) -> Result<Vec<Agent>, ManagedAgentError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn deployment_resolves_and_freezes_the_authoritative_agent_version() {
        // Cause/effect decision table derived from the official Agent input union:
        // G1 bare id -> query latest (None) and freeze the returned concrete version;
        // G2 object with version -> query and freeze that exact version;
        // G3 disabled/archived selection -> reject before storing a Deployment;
        // G4 unknown Agent -> workspace-scoped 404.
        let latest = Arc::new(ResolvingAgentRepository::published(7));
        let latest_state = DeploymentState::new();
        latest_state.bind_agent_repository(latest.clone());
        let bare: crate::types::AgentRef =
            serde_json::from_value(serde_json::json!("agent_a")).unwrap();
        assert_eq!(
            latest_state
                .resolve_agent("workspace_a", &bare)
                .await
                .unwrap()
                .version,
            7,
            "G1"
        );
        assert_eq!(*latest.requested_versions.lock().unwrap(), vec![None], "G1");

        let pinned = Arc::new(ResolvingAgentRepository::published(3));
        let pinned_state = DeploymentState::new();
        pinned_state.bind_agent_repository(pinned.clone());
        let reference: crate::types::AgentRef = serde_json::from_value(serde_json::json!({
            "type": "agent", "id": "agent_a", "version": 3
        }))
        .unwrap();
        assert_eq!(
            pinned_state
                .resolve_agent("workspace_a", &reference)
                .await
                .unwrap()
                .version,
            3,
            "G2"
        );
        assert_eq!(
            *pinned.requested_versions.lock().unwrap(),
            vec![Some(3)],
            "G2"
        );

        for status in [AgentStatus::Disabled, AgentStatus::Archived] {
            let repository = Arc::new(ResolvingAgentRepository {
                selected: Ok((2, status)),
                requested_versions: Mutex::new(Vec::new()),
            });
            let state = DeploymentState::new();
            state.bind_agent_repository(repository);
            assert_eq!(
                state
                    .resolve_agent("workspace_a", &bare)
                    .await
                    .unwrap_err()
                    .0,
                StatusCode::BAD_REQUEST,
                "G3"
            );
        }

        let missing = Arc::new(ResolvingAgentRepository {
            selected: Err(ManagedAgentError::NotFound),
            requested_versions: Mutex::new(Vec::new()),
        });
        let state = DeploymentState::new();
        state.bind_agent_repository(missing);
        assert_eq!(
            state
                .resolve_agent("workspace_a", &bare)
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND,
            "G4"
        );
    }

    #[tokio::test]
    async fn missing_or_archived_primary_agent_archives_without_a_run() {
        // Official scheduled-primary decision table:
        // P1 current primary Agent published -> ordinary occurrence claim/run;
        // P2 current primary Agent archived or missing -> archive every live
        // Deployment for that Agent and create no DeploymentRun or Session.
        // P1 is covered by the durable-claim test; this test covers both P2 causes.
        for selected in [
            Ok((1, AgentStatus::Archived)),
            Err(ManagedAgentError::NotFound),
        ] {
            let state = Arc::new(DeploymentState::new());
            state.bind_agent_repository(Arc::new(ResolvingAgentRepository {
                selected,
                requested_versions: Mutex::new(Vec::new()),
            }));
            let id = "depl_primary".to_string();
            let mut record = deployment(Some(cron_schedule("*/15 * * * *")));
            record.next_fire_ms = Some(MON_0900);
            state.deployments.lock().unwrap().insert(id.clone(), record);
            let due = jitter_due(&id, MON_0900, 15 * 60_000);

            assert!(state.tick_and_launch(due).await.unwrap().is_empty(), "P2");
            assert!(state.runs.lock().unwrap().is_empty(), "P2");
            assert!(
                state.deployments.lock().unwrap()[&id].archived_at.is_some(),
                "P2"
            );
        }
    }

    #[tokio::test]
    async fn durable_repository_restores_scope_and_claims_each_occurrence_once() {
        // Cause/effect decision table:
        // R1 committed Deployment + restart -> the same owner-scoped projection;
        // R2 another Workspace -> 404 (no ownership disclosure);
        // R3 two replicas observe one due instant -> one durable claim, one launch;
        // R4 losing replica -> no duplicate DeploymentRun and no launch side effect.
        struct CountingLauncher(Arc<std::sync::atomic::AtomicUsize>);
        #[async_trait::async_trait]
        impl DeploymentSessionLauncher for CountingLauncher {
            async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
                self.0.fetch_add(1, Ordering::SeqCst);
                DeploymentLaunchOutcome::Created {
                    session_id: format!("sesn_{}", request.deployment_id),
                }
            }
        }

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "awaken-deployment-state-{}-{unique}.db",
            std::process::id()
        ));
        let repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path.to_string_lossy())
                .unwrap(),
        );
        let first = Arc::new(
            DeploymentState::with_repository(repository.clone())
                .await
                .unwrap(),
        );
        let created = create_deployment(
            State(first.clone()),
            Some(Extension(WorkspaceScope("workspace_a".into()))),
            ManagedJson(create_params(true)),
        )
        .await
        .unwrap()
        .0;
        drop(first);

        let left = Arc::new(
            DeploymentState::with_repository(Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(&path.to_string_lossy())
                    .unwrap(),
            ))
            .await
            .unwrap(),
        );
        assert_eq!(
            retrieve_deployment(
                State(left.clone()),
                Path(created.id.clone()),
                Some(Extension(WorkspaceScope("workspace_a".into()))),
            )
            .await
            .unwrap()
            .0
            .id,
            created.id,
            "R1"
        );
        assert_eq!(
            retrieve_deployment(
                State(left.clone()),
                Path(created.id.clone()),
                Some(Extension(WorkspaceScope("workspace_b".into()))),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::NOT_FOUND,
            "R2"
        );

        let scheduled_at = MON_0900;
        let persisted = {
            let mut deployments = left.deployments.lock().unwrap();
            let record = deployments.get_mut(&created.id).unwrap();
            record.next_fire_ms = Some(scheduled_at);
            record.clone()
        };
        repository
            .upsert_deployment(stored_deployment(&created.id, &persisted).unwrap(), None)
            .await
            .unwrap();
        let right = Arc::new(
            DeploymentState::with_repository(Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(&path.to_string_lossy())
                    .unwrap(),
            ))
            .await
            .unwrap(),
        );
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        left.bind_launcher(Arc::new(CountingLauncher(launches.clone())));
        right.bind_launcher(Arc::new(CountingLauncher(launches.clone())));
        let due = jitter_due(&created.id, scheduled_at, 15 * 60_000);
        let (left_runs, right_runs) =
            tokio::join!(left.tick_and_launch(due), right.tick_and_launch(due));
        assert_eq!(
            left_runs.unwrap().len() + right_runs.unwrap().len(),
            1,
            "R3/R4"
        );
        assert_eq!(launches.load(Ordering::SeqCst), 1, "R3/R4");
        let restored = DeploymentState::with_repository(Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path.to_string_lossy())
                .unwrap(),
        ))
        .await
        .unwrap();
        assert_eq!(restored.runs.lock().unwrap().len(), 1, "R3/R4");
        let event_types = awaken_session_contract::ManagedSessionRepository::pending_lifecycle(
            repository.as_ref(),
        )
        .await
        .into_iter()
        .map(|fact| fact.event_type)
        .collect::<std::collections::BTreeSet<_>>();
        assert!(event_types.contains("deployment.created"), "R1 event");
        assert!(event_types.contains("deployment_run.started"), "R3 event");
        assert!(
            event_types.contains("deployment_run.succeeded"),
            "R3 terminal event"
        );
        drop(restored);
        drop(repository);
        let _ = std::fs::remove_file(path);
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
    fn execution_jitter_is_stable_and_obeys_all_interval_bounds() {
        // Jitter cause/effect decision table:
        // J1 15% interval <5s -> 5s; J2 between bounds -> [5s,15%];
        // J3 15% >9m -> <=9m; J4 same deployment/occurrence -> same delay.
        let short = execution_jitter_ms("dep", MON_0900, 1_000);
        assert_eq!(short, MIN_JITTER_MS, "J1");
        let minute = execution_jitter_ms("dep", MON_0900, 60_000);
        assert!((MIN_JITTER_MS..=9_000).contains(&minute), "J2");
        let daily = execution_jitter_ms("dep", MON_0900, 24 * 60 * 60_000);
        assert!((MIN_JITTER_MS..=MAX_JITTER_MS).contains(&daily), "J3");
        assert_eq!(
            daily,
            execution_jitter_ms("dep", MON_0900, 24 * 60 * 60_000),
            "J4"
        );
    }

    #[tokio::test]
    async fn scheduled_capacity_is_atomic_across_create_update_and_archive() {
        // Scheduled-capacity graph (test cap=1; production cap=1,000):
        // C1 unscheduled never consumes; C2 first scheduled create consumes;
        // C3 scheduled create or unscheduled->scheduled update at cap rejects
        // atomically; C4 archive frees one slot; C5 scheduled->scheduled update
        // does not consume a second slot.
        let state = Arc::new(DeploymentState::with_scheduled_limit(1));
        let first = create_deployment(State(state.clone()), None, ManagedJson(create_params(true)))
            .await
            .expect("C2")
            .0;
        let unscheduled = create_deployment(
            State(state.clone()),
            None,
            ManagedJson(create_params(false)),
        )
        .await
        .expect("C1")
        .0;
        let rejected =
            create_deployment(State(state.clone()), None, ManagedJson(create_params(true)))
                .await
                .expect_err("C3");
        assert_eq!(rejected.0, StatusCode::BAD_REQUEST, "C3");
        assert_eq!(state.deployments.lock().unwrap().len(), 2, "C3 atomic");

        let add_schedule: DeploymentUpdateParams = serde_json::from_value(serde_json::json!({
            "schedule": {"type":"cron", "expression":"*/15 * * * *", "timezone":"UTC"}
        }))
        .unwrap();
        assert_eq!(
            update_deployment(
                State(state.clone()),
                Path(unscheduled.id.clone()),
                None,
                ManagedJson(add_schedule.clone()),
            )
            .await
            .expect_err("C3")
            .0,
            StatusCode::BAD_REQUEST
        );
        assert!(
            state.deployments.lock().unwrap()[&unscheduled.id]
                .schedule
                .is_none(),
            "C3 update atomic"
        );
        let _ = archive_deployment(State(state.clone()), Path(first.id), None)
            .await
            .expect("C4");
        let scheduled = update_deployment(
            State(state.clone()),
            Path(unscheduled.id.clone()),
            None,
            ManagedJson(add_schedule.clone()),
        )
        .await
        .expect("C4")
        .0;
        assert!(scheduled.schedule.is_some(), "C4");
        assert!(
            update_deployment(
                State(state),
                Path(unscheduled.id),
                None,
                ManagedJson(add_schedule),
            )
            .await
            .is_ok(),
            "C5"
        );
    }

    #[test]
    fn unpause_skips_missed_occurrences_and_resumes_from_the_next_one() {
        // U1 paused with an old cursor -> no execution; U2 unpause at 09:31 ->
        // cursor becomes exact 09:45, so 09:15/09:30 are never backfilled.
        let mut record = deployment(Some(cron_schedule("*/15 * * * *")));
        record.status = "paused".into();
        record.paused_reason = Some(PausedReason::Manual);
        record.next_fire_ms = Some(MON_0900 + 15 * 60_000);
        resume_schedule(&mut record, MON_0900 + 31 * 60_000);
        assert_eq!(record.status, "active", "U2");
        assert_eq!(record.next_fire_ms, Some(MON_0900 + 45 * 60_000), "U2");
    }

    #[tokio::test]
    async fn archived_deployment_is_an_idempotent_terminal_state() {
        // Terminal table: A1 first archive succeeds; A2 repeated archive is
        // idempotent; A3 update/pause/unpause/manual-run after archive reject and
        // create no run or launcher side effect.
        let state = Arc::new(DeploymentState::new());
        let created =
            create_deployment(State(state.clone()), None, ManagedJson(create_params(true)))
                .await
                .unwrap()
                .0;
        let _ = archive_deployment(State(state.clone()), Path(created.id.clone()), None)
            .await
            .expect("A1");
        let _ = archive_deployment(State(state.clone()), Path(created.id.clone()), None)
            .await
            .expect("A2");
        let update: DeploymentUpdateParams =
            serde_json::from_value(serde_json::json!({"name":"no"})).unwrap();
        assert_eq!(
            update_deployment(
                State(state.clone()),
                Path(created.id.clone()),
                None,
                ManagedJson(update),
            )
            .await
            .expect_err("A3")
            .0,
            StatusCode::CONFLICT
        );
        assert!(
            pause_deployment(State(state.clone()), Path(created.id.clone()), None)
                .await
                .is_err()
        );
        assert!(
            unpause_deployment(State(state.clone()), Path(created.id.clone()), None)
                .await
                .is_err()
        );
        assert!(
            run_deployment(State(state.clone()), Path(created.id), None)
                .await
                .is_err()
        );
        assert!(state.runs.lock().unwrap().is_empty(), "A3");
    }

    #[tokio::test]
    async fn primary_agent_archive_cascades_only_to_its_owned_deployments() {
        // Agent/deployment cascade table: P1 same Workspace + primary Agent ->
        // archive immediately and create no run; P2 another Agent or P3 another
        // Workspace -> unchanged; P4 repeated notification -> zero new archives.
        let state = DeploymentState::new();
        let mut same = deployment(Some(cron_schedule("*/15 * * * *")));
        same.workspace_id = "workspace_a".into();
        same.agent = AgentReference::new("agent_primary", 1);
        let mut other_agent = same.clone();
        other_agent.agent = AgentReference::new("agent_other", 1);
        let mut other_workspace = same.clone();
        other_workspace.workspace_id = "workspace_b".into();
        let mut store = state.deployments.lock().unwrap();
        store.insert("same".into(), same);
        store.insert("other_agent".into(), other_agent);
        store.insert("other_workspace".into(), other_workspace);
        drop(store);

        assert_eq!(
            state
                .archive_for_agent("workspace_a", "agent_primary")
                .await
                .unwrap(),
            1,
            "P1"
        );
        let store = state.deployments.lock().unwrap();
        assert!(store["same"].archived_at.is_some(), "P1");
        assert!(store["other_agent"].archived_at.is_none(), "P2");
        assert!(store["other_workspace"].archived_at.is_none(), "P3");
        drop(store);
        assert_eq!(
            state
                .archive_for_agent("workspace_a", "agent_primary")
                .await
                .unwrap(),
            0,
            "P4"
        );
        assert!(state.runs.lock().unwrap().is_empty(), "P1");
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
        // Jitter rule J1: the exact 09:15 occurrence stays pending until its stable
        // bounded delay, then creates exactly one run whose scheduled_at remains
        // the unjittered cron instant. J2 applies independently to 09:30.
        let first_due = jitter_due("deploy_x", MON_0900 + 15 * 60_000, 15 * 60_000);
        assert!(state.tick(first_due - 1).is_empty(), "J1 not early");
        let fired = state.tick(first_due);
        assert_eq!(fired.len(), 1, "J1 fires once at its jittered due time");

        let runs = state.runs.lock().unwrap();
        let run = runs.get(&fired[0]).expect("run recorded");
        match &run.trigger {
            TriggerContext::Schedule { scheduled_at } => {
                assert_eq!(scheduled_at, "2026-01-05T09:15:00Z");
            }
            TriggerContext::Manual => panic!("a scheduled fire must carry a Schedule trigger"),
        }
        assert_eq!(run.deployment_id, "deploy_x");
        drop(runs);
        let second_due = jitter_due("deploy_x", MON_0900 + 30 * 60_000, 15 * 60_000);
        assert_eq!(state.tick(second_due).len(), 1, "J2");
        // A paused deployment stops firing.
        state
            .deployments
            .lock()
            .unwrap()
            .get_mut("deploy_x")
            .unwrap()
            .status = "paused".into();
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
                DeploymentLaunchOutcome::Created {
                    session_id: format!("sesn_{}", request.deployment_id),
                }
            }
        }

        let state = DeploymentState::new();
        state.bind_launcher(Arc::new(Launcher));
        state.deployments.lock().unwrap().insert(
            "deploy_schedule".into(),
            deployment(Some(cron_schedule("*/15 * * * *"))),
        );
        assert!(state.tick_and_launch(MON_0900).await.unwrap().is_empty());
        let due = jitter_due("deploy_schedule", MON_0900 + 15 * 60_000, 15 * 60_000);
        let completed = state.tick_and_launch(due).await.unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(
            completed[0].session_id.as_deref(),
            Some("sesn_deploy_schedule")
        );
        assert!(completed[0].error.is_none());
    }

    /// Deployment-run failure cause graph:
    /// trigger + typed launch outcome -> append-only run -> optional schedule
    /// suspension. Manual failures remain operator-visible but never change the
    /// schedule; transient scheduled failures keep future fires active; persistent
    /// scheduled failures pause with the exact run-error discriminator.
    ///
    /// | Rule | trigger | launch error | run terminal state | deployment effect |
    /// |---|---|---|---|---|
    /// | F1 | schedule | environment archived | error only | auto-pause, exact reason |
    /// | F2 | schedule | rate limited | error only | remain active |
    /// | F3 | manual | environment archived | error only | remain active |
    #[tokio::test]
    async fn launch_failure_decision_table_controls_auto_pause_behavior() {
        struct Launcher {
            error: RunError,
        }
        #[async_trait::async_trait]
        impl DeploymentSessionLauncher for Launcher {
            async fn launch(&self, _request: DeploymentLaunch) -> DeploymentLaunchOutcome {
                DeploymentLaunchOutcome::Failed {
                    error: self.error.clone(),
                }
            }
        }

        async fn exercise(trigger: TriggerContext, error: RunError) -> (DeploymentRun, Deployment) {
            let state = DeploymentState::new();
            state.bind_launcher(Arc::new(Launcher { error }));
            state
                .deployments
                .lock()
                .unwrap()
                .insert("deploy_failure".into(), deployment(None));
            state.runs.lock().unwrap().insert(
                "deprun_failure".into(),
                RunRecord {
                    created_at: OBJECT_AT.into(),
                    deployment_id: "deploy_failure".into(),
                    workspace_id: "default".into(),
                    agent: AgentReference::new("coder", 1),
                    trigger,
                    session_id: None,
                    error: None,
                },
            );
            let launch = state
                .deployments
                .lock()
                .unwrap()
                .get("deploy_failure")
                .unwrap()
                .launch("deploy_failure");
            let run = state.launch_run("deprun_failure", launch).await.unwrap();
            let deployment = state
                .deployments
                .lock()
                .unwrap()
                .get("deploy_failure")
                .unwrap()
                .project("deploy_failure");
            (run, deployment)
        }

        let archived = RunError::EnvironmentArchivedError {
            message: "archived".into(),
        };
        let (run, deployment) = exercise(
            TriggerContext::Schedule {
                scheduled_at: OBJECT_AT.into(),
            },
            archived.clone(),
        )
        .await;
        assert_eq!(run.error, Some(archived.clone()), "F1 exact run error");
        assert!(run.session_id.is_none(), "F1 exactly one terminal branch");
        assert_eq!(deployment.status, "paused", "F1");
        assert_eq!(
            deployment.paused_reason,
            Some(PausedReason::Error {
                error: crate::types::deployment::PausedReasonError::EnvironmentArchivedError,
            }),
            "F1 exact paused reason"
        );

        let (run, deployment) = exercise(
            TriggerContext::Schedule {
                scheduled_at: OBJECT_AT.into(),
            },
            RunError::SessionRateLimitedError {
                message: "retry later".into(),
            },
        )
        .await;
        assert!(run.error.is_some(), "F2 failed run retained");
        assert_eq!(deployment.status, "active", "F2 keeps schedule alive");
        assert!(deployment.paused_reason.is_none(), "F2");

        let (run, deployment) = exercise(TriggerContext::Manual, archived).await;
        assert!(run.error.is_some(), "F3 failed run retained");
        assert_eq!(
            deployment.status, "active",
            "F3 manual run cannot auto-pause"
        );
    }

    #[tokio::test]
    async fn rate_limited_deployment_run_is_recorded_without_launch_or_retry() {
        use std::sync::atomic::AtomicUsize;

        struct CountingLauncher(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl DeploymentSessionLauncher for CountingLauncher {
            async fn launch(&self, _request: DeploymentLaunch) -> DeploymentLaunchOutcome {
                self.0.fetch_add(1, Ordering::SeqCst);
                DeploymentLaunchOutcome::Created {
                    session_id: "unexpected".into(),
                }
            }
        }

        // Rate-limit table: R1 the shared Create bucket is exhausted -> one
        // session_rate_limited_error run, zero launcher calls/retries, active
        // schedule; R2 is runtime-axis independent because Native/ACP selection
        // occurs only inside the launcher that admission never invokes.
        let limiter = Arc::new(ManagedRateLimiter::with_limits(
            "org_rate",
            crate::ManagedRateLimits {
                create_per_minute: 1,
                read_per_minute: 1,
            },
        ));
        assert!(limiter.admit_internal_session_create(), "exhaust bucket");
        let launches = Arc::new(AtomicUsize::new(0));
        let state = DeploymentState::new();
        state.bind_rate_limiter(limiter);
        state.bind_launcher(Arc::new(CountingLauncher(launches.clone())));
        let record = deployment(Some(cron_schedule("*/15 * * * *")));
        let launch = record.launch("deploy_rate");
        state
            .deployments
            .lock()
            .unwrap()
            .insert("deploy_rate".into(), record);
        state.runs.lock().unwrap().insert(
            "run_rate".into(),
            RunRecord {
                created_at: OBJECT_AT.into(),
                deployment_id: "deploy_rate".into(),
                workspace_id: "default".into(),
                agent: AgentReference::new("coder", 1),
                trigger: TriggerContext::Schedule {
                    scheduled_at: OBJECT_AT.into(),
                },
                session_id: None,
                error: None,
            },
        );
        let run = state.launch_run("run_rate", launch).await.unwrap();
        assert!(
            matches!(run.error, Some(RunError::SessionRateLimitedError { .. })),
            "R1"
        );
        assert!(run.session_id.is_none(), "R1 terminal XOR");
        assert_eq!(launches.load(Ordering::SeqCst), 0, "R1/R2 no retry");
        let deployment = state.deployments.lock().unwrap()["deploy_rate"].clone();
        assert_eq!(
            deployment.status, "active",
            "R1 next occurrence remains eligible"
        );
        assert!(deployment.paused_reason.is_none(), "R1");
    }

    #[test]
    fn last_run_at_is_echoed_into_the_projected_schedule() {
        let state = DeploymentState::new();
        state.deployments.lock().unwrap().insert(
            "deploy_y".into(),
            deployment(Some(cron_schedule("*/15 * * * *"))),
        );
        state.tick(MON_0900);
        let due = jitter_due("deploy_y", MON_0900 + 15 * 60_000, 15 * 60_000);
        state.tick(due);
        let store = state.deployments.lock().unwrap();
        let projected = store.get("deploy_y").unwrap().project("deploy_y");
        let Schedule::Cron { last_run_at, .. } = projected.schedule.expect("schedule present");
        assert_eq!(last_run_at.as_deref(), Some("2026-01-05T09:15:00Z"));
    }

    /// Schedule projection cause graph:
    /// parsed cron + IANA timezone + lifecycle state -> computed future UTC
    /// occurrences. Pausing suppresses execution but preserves the preview;
    /// archiving clears it because no future fire is possible.
    ///
    /// | lifecycle | upcoming behavior |
    /// |---|---|
    /// | active | next five timezone-aware occurrences |
    /// | paused | same next five hypothetical occurrences |
    /// | archived | empty |
    #[test]
    fn projected_schedule_decision_table_tracks_lifecycle_behavior() {
        let mut record = deployment(Some(cron_schedule("*/15 * * * *")));
        let Schedule::Cron {
            upcoming_runs_at, ..
        } = record.project("deploy_projection").schedule.unwrap();
        assert_eq!(upcoming_runs_at.len(), 5, "active preview");
        assert!(
            upcoming_runs_at.windows(2).all(|pair| pair[0] < pair[1]),
            "preview is strictly ordered"
        );

        record.status = "paused".into();
        record.paused_reason = Some(PausedReason::Manual);
        let Schedule::Cron {
            upcoming_runs_at, ..
        } = record.project("deploy_projection").schedule.unwrap();
        assert_eq!(upcoming_runs_at.len(), 5, "paused preview remains visible");

        record.archived_at = Some(OBJECT_AT.into());
        let Schedule::Cron {
            upcoming_runs_at, ..
        } = record.project("deploy_projection").schedule.unwrap();
        assert!(upcoming_runs_at.is_empty(), "archived preview is empty");
        let wire = serde_json::to_value(record.project("deploy_projection")).unwrap();
        assert_eq!(wire["schedule"]["upcoming_runs_at"], serde_json::json!([]));
        assert!(
            wire["schedule"]["last_run_at"].is_null(),
            "official response retains nullable runtime fields"
        );
    }
}
