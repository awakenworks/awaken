//! Coordinator-owned Environment WorkQueue HTTP adapter.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};

use super::{EnvironmentExecutionState, WireError, bad_request, not_found};
use crate::routes::ManagedJson;
use crate::types::environment::{Work, WorkHeartbeat, WorkQueueStats, WorkUpdateParams};
use crate::types::{ErrorResponse, Page, PageQuery, paginate};
use crate::work_queue::{HeartbeatResult, LeaseHeartbeat};

fn map_work_queue_error(error: crate::work_queue::WorkQueueError) -> WireError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse::new("api_error", error.to_string())),
    )
}

async fn require_env(state: &EnvironmentExecutionState, id: &str) -> Result<(), WireError> {
    let registration = state
        .execution_source
        .current_registration(id)
        .await
        .map_err(|error| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new("api_error", error.to_string())),
            )
        })?;
    if registration.is_some() {
        Ok(())
    } else {
        Err(not_found("environment"))
    }
}

/// `GET /v1/environments/:id/work` — the environment's work items.
pub(super) async fn list_work(
    State(state): State<Arc<EnvironmentExecutionState>>,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> Result<Json<Page<Work>>, WireError> {
    require_env(&state, &id).await?;
    let data: Vec<Work> = state
        .work
        .list(&id)
        .await
        .map_err(map_work_queue_error)?
        .iter()
        .map(crate::work_queue::project_work)
        .collect();
    Ok(Json(paginate(data, &page, |w| w.id.as_str())))
}

/// `GET /v1/environments/:id/work/poll` — lease the next queued item to the
/// single worker. Open-tier cap: returns `null` when an item is already `active`
/// in this environment (one lease at a time) or the queue is empty.
pub(super) async fn poll_work(
    State(state): State<Arc<EnvironmentExecutionState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Json<Option<Work>>, WireError> {
    require_env(&state, &id).await?;
    let poll = parse_poll_params(raw.as_deref())?;
    // The official SDK sends worker identity in `Anthropic-Worker-ID`, not in
    // the query string. Long polling repeatedly drives the same authoritative
    // atomic claim; it does not introduce a second queue or lease registry.
    let worker_id = worker_id(&headers);
    let started = tokio::time::Instant::now();
    loop {
        let claimed = state
            .work
            .claim_with_reclaim(&id, worker_id, now_ms(), poll.reclaim_older_than_ms)
            .await
            .map_err(map_work_queue_error)?;
        if let Some(work) = claimed {
            return Ok(Json(Some(crate::work_queue::project_work(&work))));
        }
        let Some(wait) = poll.block_ms else {
            return Ok(Json(None));
        };
        if started.elapsed() >= wait {
            return Ok(Json(None));
        }
        tokio::time::sleep(
            wait.saturating_sub(started.elapsed())
                .min(std::time::Duration::from_millis(20)),
        )
        .await;
    }
}

/// Parsed poll timing. `None` means the caller explicitly sent `block_ms=null`
/// (serialized by the official SDK as an empty query value); omission uses the
/// documented 999 ms default.
struct PollParams {
    block_ms: Option<std::time::Duration>,
    reclaim_older_than_ms: Option<u64>,
}

fn parse_poll_params(raw: Option<&str>) -> Result<PollParams, WireError> {
    let mut block_ms = Some(std::time::Duration::from_millis(999));
    let mut reclaim_older_than_ms = None;
    for (key, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "block_ms" if value.is_empty() => block_ms = None,
            "block_ms" => {
                let millis = value.parse::<u64>().map_err(|_| {
                    bad_request("block_ms must be null or an integer from 1 through 999")
                })?;
                if !(1..=999).contains(&millis) {
                    return Err(bad_request(
                        "block_ms must be null or an integer from 1 through 999",
                    ));
                }
                block_ms = Some(std::time::Duration::from_millis(millis));
            }
            "reclaim_older_than_ms" if value.is_empty() => reclaim_older_than_ms = None,
            "reclaim_older_than_ms" => {
                reclaim_older_than_ms = Some(value.parse::<u64>().map_err(|_| {
                    bad_request("reclaim_older_than_ms must be a non-negative integer")
                })?);
            }
            _ => {}
        }
    }
    Ok(PollParams {
        block_ms,
        reclaim_older_than_ms,
    })
}

#[derive(serde::Deserialize)]
pub(super) struct HeartbeatParams {
    desired_ttl_seconds: Option<u64>,
    expected_last_heartbeat: Option<String>,
}

/// `GET /v1/environments/:id/work/stats` — the queue's depth + pending count.
pub(super) async fn work_stats(
    State(state): State<Arc<EnvironmentExecutionState>>,
    Path(id): Path<String>,
) -> Result<Json<WorkQueueStats>, WireError> {
    require_env(&state, &id).await?;
    let s = state
        .work
        .stats(&id, now_ms())
        .await
        .map_err(map_work_queue_error)?;
    Ok(Json(WorkQueueStats {
        object_type: "work_queue_stats",
        depth: s.depth,
        pending: s.pending,
        oldest_queued_at: s.oldest_queued_at,
        workers_polling: s.workers_polling,
    }))
}

pub(super) async fn retrieve_work(
    State(state): State<Arc<EnvironmentExecutionState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .get(&id, &wid)
        .await
        .map_err(map_work_queue_error)?
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

pub(super) async fn update_work(
    State(state): State<Arc<EnvironmentExecutionState>>,
    Path((id, wid)): Path<(String, String)>,
    ManagedJson(params): ManagedJson<WorkUpdateParams>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .update_metadata(&id, &wid, params.metadata.unwrap_or_default())
        .await
        .map_err(map_work_queue_error)?
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

/// `POST …/work/:wid/ack` — the worker acknowledges it picked up the item.
pub(super) async fn ack_work(
    State(state): State<Arc<EnvironmentExecutionState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .ack(&id, &wid)
        .await
        .map_err(map_work_queue_error)?
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

/// `POST …/work/:wid/heartbeat` — extend the lease; returns the TTL.
pub(super) async fn heartbeat_work(
    State(state): State<Arc<EnvironmentExecutionState>>,
    Path((id, wid)): Path<(String, String)>,
    headers: HeaderMap,
    Query(params): Query<HeartbeatParams>,
) -> Result<Json<WorkHeartbeat>, WireError> {
    require_env(&state, &id).await?;
    let command = LeaseHeartbeat {
        condition: crate::work_queue::HeartbeatCondition::from_wire(
            params.expected_last_heartbeat.as_deref(),
        ),
        desired_ttl_seconds: params.desired_ttl_seconds,
    };
    let hb = match state
        .work
        .heartbeat(&id, &wid, worker_id(&headers), now_ms(), command)
        .await
        .map_err(map_work_queue_error)?
    {
        HeartbeatResult::Accepted(receipt) => receipt,
        HeartbeatResult::PreconditionFailed => {
            return Err((
                StatusCode::PRECONDITION_FAILED,
                Json(ErrorResponse::new(
                    "precondition_error",
                    "expected_last_heartbeat does not match",
                )),
            ));
        }
        HeartbeatResult::NotFound => return Err(not_found("work")),
    };
    Ok(Json(WorkHeartbeat {
        object_type: "work_heartbeat",
        last_heartbeat: hb.last_heartbeat,
        lease_extended: hb.lease_extended,
        state: hb.state,
        ttl_seconds: hb.ttl_seconds,
    }))
}

/// The Managed worker identity carried consistently on poll and worker-owned
/// lease mutations. It is compared atomically with the claim owner by WorkQueue.
fn worker_id(headers: &HeaderMap) -> &str {
    headers
        .get("anthropic-worker-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
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
pub(super) async fn stop_work(
    State(state): State<Arc<EnvironmentExecutionState>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    require_env(&state, &id).await?;
    let work = state
        .work
        .stop(&id, &wid)
        .await
        .map_err(map_work_queue_error)?
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}
