//! The Managed **environments** family (`/v1/environments`) + the **work queue**
//! (`/v1/environments/:id/work…`), the official `@anthropic-ai/sdk`
//! `beta.environments.*` and `beta.environments.work.*` surfaces. An environment
//! is where a self-hosted worker runs sessions; the work queue is how the platform
//! hands work to that worker (poll → ack → heartbeat → stop).
//!
//! Open-tier semantics: the API shape is complete and usable, but a single-machine
//! build leases work to **one** worker at a time — `poll` hands out a queued item
//! only when no item in the environment is already `active`. Multi-worker
//! fan-out (many concurrent leases) is the managed scaling boundary; here the
//! queue is one in-process store. Every new environment is seeded with one
//! `healthcheck` work item so the queue is exercisable end to end.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::env_registry::{EnvRegistry, EnvUpdate, InMemoryEnvRegistry};
use crate::routes::ManagedJson;
use crate::types::environment::{
    DeletedEnvironment, Environment, EnvironmentCreateParams, EnvironmentUpdateParams, Work,
    WorkHeartbeat, WorkQueueStats, WorkUpdateParams,
};
use crate::types::{ErrorResponse, Page, PageQuery, paginate};
use crate::work_queue::{InMemoryWorkQueue, WorkQueue};

/// The self-hosted environment registry + work queue, both behind ports so a
/// durable backend (sqlite/postgres) serves standalone and distributed deployments
/// unchanged; the default is in-memory.
pub struct EnvironmentState {
    envs: Arc<dyn EnvRegistry>,
    work: Arc<dyn WorkQueue>,
}

impl Default for EnvironmentState {
    fn default() -> Self {
        Self {
            envs: Arc::new(InMemoryEnvRegistry::new()),
            work: Arc::new(InMemoryWorkQueue::new()),
        }
    }
}

impl EnvironmentState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install durable registry + work-queue backends (sqlite/postgres) in place of
    /// the in-memory defaults; the same routes then serve any deployment mode.
    #[must_use]
    pub fn with_stores(envs: Arc<dyn EnvRegistry>, work: Arc<dyn WorkQueue>) -> Self {
        Self { envs, work }
    }

    /// Whether the local bwrap sandbox must deny egress for `env_id`. bwrap is a
    /// binary (on/off) enforcer, so any restricted policy collapses to full deny;
    /// `unrestricted`, absent networking (incl. `self_hosted`), or an unknown
    /// environment share the host network.
    pub async fn deny_egress(&self, env_id: &str) -> bool {
        self.envs
            .get(env_id)
            .await
            .is_some_and(|rec| rec.network_policy().is_restricted())
    }

    /// Whether `env_id` is a self-hosted environment. Sessions assigned to one are
    /// dispatched through the work queue for an external worker to run.
    pub async fn is_self_hosted(&self, env_id: &str) -> bool {
        self.envs
            .get(env_id)
            .await
            .is_some_and(|rec| rec.is_self_hosted())
    }

    /// Enqueue a `session` work item for `session_id` on `env_id`'s queue — the way
    /// the control plane dispatches a session assigned to a self-hosted environment,
    /// so a worker polling the environment can claim and run it. Returns the work id.
    pub async fn enqueue_session_work(&self, env_id: &str, session_id: &str) -> String {
        self.work.enqueue_session(env_id, session_id).await
    }
}

/// Mount the environments + work routes.
pub fn environments_router(state: Arc<EnvironmentState>) -> Router {
    Router::new()
        .route("/v1/environments", post(create_env).get(list_envs))
        .route(
            "/v1/environments/{id}",
            get(retrieve_env).post(update_env).delete(delete_env),
        )
        .route("/v1/environments/{id}/archive", post(archive_env))
        .route("/v1/environments/{id}/work", get(list_work))
        .route("/v1/environments/{id}/work/poll", get(poll_work))
        .route("/v1/environments/{id}/work/stats", get(work_stats))
        .route(
            "/v1/environments/{id}/work/{wid}",
            get(retrieve_work).post(update_work),
        )
        .route("/v1/environments/{id}/work/{wid}/ack", post(ack_work))
        .route(
            "/v1/environments/{id}/work/{wid}/heartbeat",
            post(heartbeat_work),
        )
        .route("/v1/environments/{id}/work/{wid}/stop", post(stop_work))
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

// ---- Environment routes ----------------------------------------------------

async fn create_env(
    State(state): State<Arc<EnvironmentState>>,
    ManagedJson(params): ManagedJson<EnvironmentCreateParams>,
) -> Result<Json<Environment>, WireError> {
    let config = params
        .config
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!({ "type": "self_hosted" }));
    // No `scope` on the wire: ownership is credential-implicit (authz enforces the
    // workspace) and any awaken tenancy is an ingress concern.
    let item = state
        .envs
        .create(
            params.name,
            params.description.unwrap_or_default(),
            params.metadata,
            config,
        )
        .await;
    // Seed one healthcheck work item so the queue is exercisable end to end.
    state.work.enqueue_healthcheck(&item.id).await;
    Ok(Json(item.project()))
}

async fn retrieve_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Environment>, WireError> {
    let item = state
        .envs
        .get(&id)
        .await
        .ok_or_else(|| not_found("environment"))?;
    Ok(Json(item.project()))
}

async fn list_envs(
    State(state): State<Arc<EnvironmentState>>,
    Query(page): Query<PageQuery>,
) -> Json<Page<Environment>> {
    let data: Vec<Environment> = state
        .envs
        .list_active()
        .await
        .iter()
        .map(|e| e.project())
        .collect();
    Json(paginate(data, &page, |e| e.id.as_str()))
}

async fn update_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
    ManagedJson(params): ManagedJson<EnvironmentUpdateParams>,
) -> Result<Json<Environment>, WireError> {
    let patch = EnvUpdate {
        name: params.name,
        description: params.description,
        config: params.config,
        metadata: params.metadata,
    };
    let item = state
        .envs
        .update(&id, patch)
        .await
        .ok_or_else(|| not_found("environment"))?;
    Ok(Json(item.project()))
}

async fn delete_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<DeletedEnvironment>, WireError> {
    if !state.envs.delete(&id).await {
        return Err(not_found("environment"));
    }
    state.work.remove_env(&id).await;
    Ok(Json(DeletedEnvironment {
        id,
        object_type: "environment_deleted",
    }))
}

async fn archive_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Environment>, WireError> {
    let item = state
        .envs
        .archive(&id)
        .await
        .ok_or_else(|| not_found("environment"))?;
    Ok(Json(item.project()))
}

// ---- Work routes -----------------------------------------------------------

async fn require_env(state: &EnvironmentState, id: &str) -> Result<(), WireError> {
    if state.envs.exists(id).await {
        Ok(())
    } else {
        Err(not_found("environment"))
    }
}

/// `GET /v1/environments/:id/work` — the environment's work items.
async fn list_work(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Page<Work>>, WireError> {
    require_env(&state, &id).await?;
    let data: Vec<Work> = state
        .work
        .list(&id)
        .await
        .iter()
        .map(|w| w.project())
        .collect();
    Ok(Json(paginate(data, &page, |w| w.id.as_str())))
}

/// `GET /v1/environments/:id/work/poll` — lease the next queued item to the
/// single worker. Open-tier cap: returns `null` when an item is already `active`
/// in this environment (one lease at a time) or the queue is empty.
async fn poll_work(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
    Query(poll): Query<PollParams>,
) -> Result<Json<Option<Work>>, WireError> {
    require_env(&state, &id).await?;
    // The `worker_id` the SDK poller sends identifies the caller for the
    // `workers_polling` liveness count; absent, the poll is anonymous ("").
    let worker_id = poll.worker_id.unwrap_or_default();
    Ok(Json(
        state
            .work
            .claim(&id, &worker_id, now_ms())
            .await
            .map(|w| w.project()),
    ))
}

/// The poll query: the worker's identity for the `workers_polling` liveness count.
#[derive(serde::Deserialize)]
struct PollParams {
    worker_id: Option<String>,
}

/// `GET /v1/environments/:id/work/stats` — the queue's depth + pending count.
async fn work_stats(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkQueueStats>, WireError> {
    require_env(&state, &id).await?;
    Ok(Json(state.work.stats(&id, now_ms()).await))
}

async fn retrieve_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .get(&id, &wid)
        .await
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(work.project()))
}

async fn update_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
    ManagedJson(params): ManagedJson<WorkUpdateParams>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .update_metadata(&id, &wid, params.metadata.unwrap_or_default())
        .await
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(work.project()))
}

/// `POST …/work/:wid/ack` — the worker acknowledges it picked up the item.
async fn ack_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .ack(&id, &wid)
        .await
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(work.project()))
}

/// `POST …/work/:wid/heartbeat` — extend the lease; returns the TTL.
async fn heartbeat_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<WorkHeartbeat>, WireError> {
    require_env(&state, &id).await?;
    let hb = state
        .work
        .heartbeat(&id, &wid, now_ms())
        .await
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(hb))
}

/// Wall-clock now in epoch ms — read only at this HTTP edge and passed into the
/// (clock-free) work queue, so the queue's lease/poll bookkeeping is deterministic
/// under test while production uses real time.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `POST …/work/:wid/stop` — request the worker stop the item.
async fn stop_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .stop(&id, &wid)
        .await
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(work.project()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[tokio::test]
    async fn deny_egress_reads_the_typed_policy_per_environment() {
        let state = EnvironmentState::new();
        let open = state
            .envs
            .create(
                "o".into(),
                String::new(),
                BTreeMap::new(),
                json!({ "networking": { "type": "unrestricted" } }),
            )
            .await;
        let closed = state
            .envs
            .create(
                "c".into(),
                String::new(),
                BTreeMap::new(),
                json!({ "networking": { "type": "none" } }),
            )
            .await;
        assert!(!state.deny_egress(&open.id).await);
        assert!(state.deny_egress(&closed.id).await);
        // Unknown environment shares the host network (no record → false).
        assert!(!state.deny_egress("env_missing").await);
    }

    #[tokio::test]
    async fn with_stores_selects_self_hosted_and_handles_missing() {
        let state = EnvironmentState::with_stores(
            Arc::new(InMemoryEnvRegistry::new()),
            Arc::new(InMemoryWorkQueue::new()),
        );
        let e = state
            .envs
            .create(
                "e".into(),
                String::new(),
                BTreeMap::new(),
                json!({ "type": "self_hosted" }),
            )
            .await;
        assert!(state.is_self_hosted(&e.id).await);
        assert!(
            !state.is_self_hosted("missing").await,
            "unknown env is not self-hosted"
        );
    }
}
