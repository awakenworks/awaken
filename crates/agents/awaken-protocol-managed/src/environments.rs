//! The Managed **environments** family (`/v1/environments`) + the **work queue**
//! (`/v1/environments/:id/work…`), the official `@anthropic-ai/sdk`
//! `beta.environments.*` and `beta.environments.work.*` surfaces. An environment
//! is where a self-hosted worker runs sessions; the work queue is how the platform
//! hands work to that worker (poll → ack → heartbeat → stop).
//!
//! Open-tier semantics: the API shape is complete and usable, but a single-machine
//! build leases work to **one** worker at a time — `poll` hands out a queued item
//! only when no item in the environment is already `active`. Multi-worker
//! fan-out (many concurrent leases) is the BuSL/managed scaling boundary; here the
//! queue is one in-process store. Every new environment is seeded with one
//! `healthcheck` work item so the queue is exercisable end to end.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_provisioning_contract::NetworkPolicy;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::types::ErrorResponse;
use crate::pagination::Page;
use crate::router::ManagedJson;

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
const HEARTBEAT_TTL_SECONDS: u64 = 60;

#[derive(Clone)]
struct EnvRecord {
    name: String,
    description: String,
    metadata: BTreeMap<String, String>,
    /// `BetaCloudConfig | BetaSelfHostedConfig` (defaults to `{type:self_hosted}`).
    config: Value,
    archived_at: Option<String>,
}

impl EnvRecord {
    /// Project to the official `BetaEnvironment` shape. The ownership coordinate
    /// (organization / workspace) is credential-implicit and never a data-plane
    /// field, so the environment carries no `scope` — workspace scoping is enforced
    /// by the authz layer from the credential, and any awaken tenancy (Project) is
    /// an ingress concern (`/projects/{slug}/…`), not part of this object.
    fn project(&self, id: &str) -> Value {
        json!({
            "id": id,
            "type": "environment",
            "archived_at": self.archived_at,
            "created_at": OBJECT_AT,
            "updated_at": OBJECT_AT,
            "name": self.name,
            "description": self.description,
            "metadata": self.metadata,
            "config": self.config,
        })
    }

    /// Map this environment's Anthropic `BetaEnvironment.networking` wire config
    /// onto the neutral [`NetworkPolicy`] the sandbox understands (the ACL edge):
    /// `unrestricted → Unrestricted`, `limited{hosts} → Allowlist`, `none → None`.
    /// Absent networking (incl. `self_hosted`) or an unknown type is `Unrestricted`
    /// — the host network is shared unless a policy explicitly restricts it.
    fn network_policy(&self) -> NetworkPolicy {
        let Some(net) = self.config.get("networking") else {
            return NetworkPolicy::Unrestricted;
        };
        match net.get("type").and_then(Value::as_str) {
            Some("none") => NetworkPolicy::None,
            Some("limited") => {
                let hosts = net
                    .get("allowed_hosts")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|h| h.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                NetworkPolicy::Allowlist { hosts }
            }
            _ => NetworkPolicy::Unrestricted,
        }
    }
}

#[derive(Clone)]
struct WorkRecord {
    environment_id: String,
    /// `BetaSessionWorkData | BetaHealthCheckWorkData`.
    data: Value,
    metadata: BTreeMap<String, String>,
    /// `queued` | `starting` | `active` | `stopping` | `stopped`.
    state: &'static str,
    acknowledged_at: Option<String>,
    latest_heartbeat_at: Option<String>,
    started_at: Option<String>,
    stop_requested_at: Option<String>,
    stopped_at: Option<String>,
}

impl WorkRecord {
    fn project(&self, id: &str) -> Value {
        json!({
            "id": id,
            "type": "work",
            "environment_id": self.environment_id,
            "data": self.data,
            "metadata": self.metadata,
            "state": self.state,
            "secret": null,
            "acknowledged_at": self.acknowledged_at,
            "latest_heartbeat_at": self.latest_heartbeat_at,
            "created_at": OBJECT_AT,
            "started_at": self.started_at,
            "stop_requested_at": self.stop_requested_at,
            "stopped_at": self.stopped_at,
        })
    }
}

/// The environments + work-queue state.
#[derive(Default)]
pub struct EnvironmentState {
    envs: Mutex<BTreeMap<String, EnvRecord>>,
    works: Mutex<BTreeMap<String, WorkRecord>>,
    env_seq: AtomicU64,
    work_seq: AtomicU64,
}

impl EnvironmentState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the local bwrap sandbox must deny egress for `env_id`. bwrap is a
    /// binary (on/off) enforcer, so any restricted policy collapses to full deny:
    /// `limited` (an allowlist bwrap cannot honor → fails closed) and `none` deny;
    /// `unrestricted`, absent networking (incl. `self_hosted`), or an unknown
    /// environment share the host network.
    #[must_use]
    pub fn deny_egress(&self, env_id: &str) -> bool {
        let envs = self.envs.lock().unwrap();
        envs.get(env_id)
            .is_some_and(|rec| rec.network_policy().is_restricted())
    }
}

/// Mount the environments + work routes.
pub fn environments_router(state: Arc<EnvironmentState>) -> Router {
    Router::new()
        .route("/v1/environments", post(create_env).get(list_envs))
        .route(
            "/v1/environments/:id",
            get(retrieve_env).post(update_env).delete(delete_env),
        )
        .route("/v1/environments/:id/archive", post(archive_env))
        .route("/v1/environments/:id/work", get(list_work))
        .route("/v1/environments/:id/work/poll", get(poll_work))
        .route("/v1/environments/:id/work/stats", get(work_stats))
        .route(
            "/v1/environments/:id/work/:wid",
            get(retrieve_work).post(update_work),
        )
        .route("/v1/environments/:id/work/:wid/ack", post(ack_work))
        .route(
            "/v1/environments/:id/work/:wid/heartbeat",
            post(heartbeat_work),
        )
        .route("/v1/environments/:id/work/:wid/stop", post(stop_work))
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

fn metadata_of(body: &Value) -> BTreeMap<String, String> {
    body.get("metadata")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

// ---- Environment routes ----------------------------------------------------

async fn create_env(
    State(state): State<Arc<EnvironmentState>>,
    ManagedJson(body): ManagedJson<Value>,
) -> Result<Json<Value>, WireError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_request("name is required"))?
        .to_string();
    let config = body
        .get("config")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!({ "type": "self_hosted" }));
    let record = EnvRecord {
        name,
        description: body
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        metadata: metadata_of(&body),
        config,
        // No `scope` on the wire: ownership is credential-implicit (authz enforces
        // the workspace from the credential) and any awaken tenancy is an ingress
        // concern — a `scope` sent in the body is ignored, like any non-official field.
        archived_at: None,
    };
    let n = state.env_seq.fetch_add(1, Ordering::SeqCst);
    let id = format!("env_{n:016}");
    // Seed one healthcheck work item so the queue is exercisable.
    let w = state.work_seq.fetch_add(1, Ordering::SeqCst);
    let work_id = format!("work_{w:016}");
    state.works.lock().unwrap().insert(
        work_id.clone(),
        WorkRecord {
            environment_id: id.clone(),
            data: json!({ "id": work_id, "type": "healthcheck" }),
            metadata: BTreeMap::new(),
            state: "queued",
            acknowledged_at: None,
            latest_heartbeat_at: None,
            started_at: None,
            stop_requested_at: None,
            stopped_at: None,
        },
    );
    let projected = record.project(&id);
    state.envs.lock().unwrap().insert(id, record);
    Ok(Json(projected))
}

async fn retrieve_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, WireError> {
    let envs = state.envs.lock().unwrap();
    let record = envs.get(&id).ok_or_else(|| not_found("environment"))?;
    Ok(Json(record.project(&id)))
}

async fn list_envs(State(state): State<Arc<EnvironmentState>>) -> Json<Page<Value>> {
    let envs = state.envs.lock().unwrap();
    let data = envs
        .iter()
        .filter(|(_, e)| e.archived_at.is_none())
        .map(|(id, e)| e.project(id))
        .collect();
    Json(Page::single(data))
}

async fn update_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
    ManagedJson(body): ManagedJson<Value>,
) -> Result<Json<Value>, WireError> {
    let mut envs = state.envs.lock().unwrap();
    let record = envs.get_mut(&id).ok_or_else(|| not_found("environment"))?;
    if let Some(name) = body.get("name").and_then(Value::as_str) {
        record.name = name.to_string();
    }
    if let Some(desc) = body.get("description") {
        record.description = desc.as_str().unwrap_or_default().to_string();
    }
    if let Some(config) = body.get("config").filter(|v| !v.is_null()) {
        record.config = config.clone();
    }
    if let Some(patch) = body.get("metadata").and_then(Value::as_object) {
        for (k, v) in patch {
            match v {
                Value::Null => {
                    record.metadata.remove(k);
                }
                Value::String(s) => {
                    record.metadata.insert(k.clone(), s.clone());
                }
                _ => {}
            }
        }
    }
    Ok(Json(record.project(&id)))
}

async fn delete_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, WireError> {
    if state.envs.lock().unwrap().remove(&id).is_none() {
        return Err(not_found("environment"));
    }
    state
        .works
        .lock()
        .unwrap()
        .retain(|_, w| w.environment_id != id);
    Ok(Json(json!({ "id": id, "type": "environment_deleted" })))
}

async fn archive_env(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, WireError> {
    let mut envs = state.envs.lock().unwrap();
    let record = envs.get_mut(&id).ok_or_else(|| not_found("environment"))?;
    record.archived_at = Some(OBJECT_AT.to_string());
    Ok(Json(record.project(&id)))
}

// ---- Work routes -----------------------------------------------------------

fn require_env(state: &EnvironmentState, id: &str) -> Result<(), WireError> {
    if state.envs.lock().unwrap().contains_key(id) {
        Ok(())
    } else {
        Err(not_found("environment"))
    }
}

/// `GET /v1/environments/:id/work` — the environment's work items.
async fn list_work(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Page<Value>>, WireError> {
    require_env(&state, &id)?;
    let works = state.works.lock().unwrap();
    let data = works
        .iter()
        .filter(|(_, w)| w.environment_id == id)
        .map(|(wid, w)| w.project(wid))
        .collect();
    Ok(Json(Page::single(data)))
}

/// `GET /v1/environments/:id/work/poll` — lease the next queued item to the
/// single worker. Open-tier cap: returns `null` when an item is already `active`
/// in this environment (one lease at a time) or the queue is empty.
async fn poll_work(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, WireError> {
    require_env(&state, &id)?;
    let mut works = state.works.lock().unwrap();
    // Single active lease per environment (the open-tier single-worker cap).
    let has_active = works
        .values()
        .any(|w| w.environment_id == id && w.state == "active");
    if has_active {
        return Ok(Json(Value::Null));
    }
    // Lease the oldest queued item (ascending id == enqueue order).
    let next = works
        .iter()
        .filter(|(_, w)| w.environment_id == id && w.state == "queued")
        .map(|(wid, _)| wid.clone())
        .min();
    match next {
        Some(wid) => {
            let work = works.get_mut(&wid).expect("just found");
            work.state = "active";
            work.started_at = Some(OBJECT_AT.to_string());
            Ok(Json(work.project(&wid)))
        }
        None => Ok(Json(Value::Null)),
    }
}

/// `GET /v1/environments/:id/work/stats` — the queue's depth + pending count.
async fn work_stats(
    State(state): State<Arc<EnvironmentState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, WireError> {
    require_env(&state, &id)?;
    let works = state.works.lock().unwrap();
    let in_env: Vec<&WorkRecord> = works.values().filter(|w| w.environment_id == id).collect();
    let queued = in_env.iter().filter(|w| w.state == "queued").count();
    let pending = in_env
        .iter()
        .filter(|w| matches!(w.state, "queued" | "starting" | "active"))
        .count();
    let workers_polling = i64::from(in_env.iter().any(|w| w.state == "active"));
    Ok(Json(json!({
        "type": "work_queue_stats",
        "depth": queued,
        "pending": pending,
        "oldest_queued_at": if queued > 0 { Value::String(OBJECT_AT.to_string()) } else { Value::Null },
        "workers_polling": workers_polling,
    })))
}

/// Find the work item under `id`/`wid` or 404 (both the env and the membership).
fn work_belongs(state: &EnvironmentState, id: &str, wid: &str) -> Result<(), WireError> {
    require_env(state, id)?;
    let works = state.works.lock().unwrap();
    match works.get(wid) {
        Some(w) if w.environment_id == id => Ok(()),
        _ => Err(not_found("work")),
    }
}

async fn retrieve_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Value>, WireError> {
    work_belongs(&state, &id, &wid)?;
    let works = state.works.lock().unwrap();
    Ok(Json(works[&wid].project(&wid)))
}

async fn update_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
    ManagedJson(body): ManagedJson<Value>,
) -> Result<Json<Value>, WireError> {
    work_belongs(&state, &id, &wid)?;
    let mut works = state.works.lock().unwrap();
    let work = works.get_mut(&wid).expect("belongs");
    if let Some(patch) = body.get("metadata").and_then(Value::as_object) {
        for (k, v) in patch {
            if let Some(s) = v.as_str() {
                work.metadata.insert(k.clone(), s.to_string());
            }
        }
    }
    Ok(Json(work.project(&wid)))
}

/// `POST …/work/:wid/ack` — the worker acknowledges it picked up the item.
async fn ack_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Value>, WireError> {
    work_belongs(&state, &id, &wid)?;
    let mut works = state.works.lock().unwrap();
    let work = works.get_mut(&wid).expect("belongs");
    work.acknowledged_at = Some(OBJECT_AT.to_string());
    if work.state == "queued" {
        work.state = "starting";
    }
    Ok(Json(work.project(&wid)))
}

/// `POST …/work/:wid/heartbeat` — extend the lease; returns the TTL.
async fn heartbeat_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Value>, WireError> {
    work_belongs(&state, &id, &wid)?;
    let mut works = state.works.lock().unwrap();
    let work = works.get_mut(&wid).expect("belongs");
    work.latest_heartbeat_at = Some(OBJECT_AT.to_string());
    Ok(Json(json!({
        "type": "work_heartbeat",
        "last_heartbeat": OBJECT_AT,
        "lease_extended": true,
        "state": work.state,
        "ttl_seconds": HEARTBEAT_TTL_SECONDS,
    })))
}

/// `POST …/work/:wid/stop` — request the worker stop the item.
async fn stop_work(
    State(state): State<Arc<EnvironmentState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Value>, WireError> {
    work_belongs(&state, &id, &wid)?;
    let mut works = state.works.lock().unwrap();
    let work = works.get_mut(&wid).expect("belongs");
    work.stop_requested_at = Some(OBJECT_AT.to_string());
    work.stopped_at = Some(OBJECT_AT.to_string());
    work.state = "stopped";
    Ok(Json(work.project(&wid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(config: Value) -> EnvRecord {
        EnvRecord {
            name: "e".into(),
            description: String::new(),
            metadata: BTreeMap::new(),
            config,
            archived_at: None,
        }
    }

    #[test]
    fn wire_networking_maps_to_neutral_policy_and_egress() {
        // unrestricted → shares host network
        let open = env_with(json!({ "networking": { "type": "unrestricted" } }));
        assert_eq!(open.network_policy(), NetworkPolicy::Unrestricted);
        assert!(!open.network_policy().is_restricted());

        // limited{allowed_hosts} → typed Allowlist, denies under bwrap (fail-closed)
        let limited = env_with(json!({
            "networking": { "type": "limited", "allowed_hosts": ["api.anthropic.com"] }
        }));
        assert_eq!(
            limited.network_policy(),
            NetworkPolicy::Allowlist {
                hosts: vec!["api.anthropic.com".to_string()],
            }
        );
        assert!(limited.network_policy().is_restricted());

        // none → no egress
        let none = env_with(json!({ "networking": { "type": "none" } }));
        assert_eq!(none.network_policy(), NetworkPolicy::None);
        assert!(none.network_policy().is_restricted());

        // absent networking / self_hosted / unknown → Unrestricted (shares host)
        let self_hosted = env_with(json!({ "type": "self_hosted" }));
        assert_eq!(self_hosted.network_policy(), NetworkPolicy::Unrestricted);
        assert!(!self_hosted.network_policy().is_restricted());
    }

    #[test]
    fn deny_egress_reads_the_typed_policy_per_environment() {
        let state = EnvironmentState::new();
        state.envs.lock().unwrap().insert(
            "env_open".into(),
            env_with(json!({ "networking": { "type": "unrestricted" } })),
        );
        state.envs.lock().unwrap().insert(
            "env_closed".into(),
            env_with(json!({ "networking": { "type": "none" } })),
        );
        assert!(!state.deny_egress("env_open"));
        assert!(state.deny_egress("env_closed"));
        // Unknown environment shares the host network (no record → false).
        assert!(!state.deny_egress("env_missing"));
    }
}
