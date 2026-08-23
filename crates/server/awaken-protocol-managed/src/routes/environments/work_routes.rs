//! Coordinator-owned Environment WorkQueue HTTP adapter.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;

use super::{WireError, bad_request, not_found};
use crate::routes::ManagedJson;
use crate::types::environment::{
    Work, WorkHeartbeat, WorkQueueStats, WorkSecret, WorkStopParams, WorkUpdateParams,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate};
use crate::work_queue::{HeartbeatResult, LeaseHeartbeat};
use awaken_environment_execution_application::{
    EnvironmentExecutionApplication, EnvironmentExecutionError,
};

fn map_execution_error(error: EnvironmentExecutionError) -> WireError {
    match error {
        EnvironmentExecutionError::EnvironmentNotFound => not_found("environment"),
        EnvironmentExecutionError::Registration(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("api_error", error.to_string())),
        ),
        EnvironmentExecutionError::WorkQueue(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("api_error", error.to_string())),
        ),
    }
}

/// `GET /v1/environments/:id/work` — the environment's work items.
pub(super) async fn list_work(
    State(state): State<Arc<EnvironmentExecutionApplication>>,
    Path(id): Path<String>,
    Query(page): Query<PageQuery>,
) -> Result<Json<PageCursor<Work>>, WireError> {
    let data: Vec<Work> = state
        .list_work(&id)
        .await
        .map_err(map_execution_error)?
        .iter()
        .map(crate::work_queue::project_work)
        .collect();
    Ok(Json(paginate(data, &page, |w| w.id.as_str())))
}

/// `GET /v1/environments/:id/work/poll` — lease the next queued item to the
/// single worker. Open-tier cap: returns `null` when an item is already `active`
/// in this environment (one lease at a time) or the queue is empty.
pub(super) async fn poll_work(
    State(state): State<Arc<EnvironmentExecutionApplication>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Json<Option<Work>>, WireError> {
    let poll = parse_poll_params(raw.as_deref())?;
    // The official SDK sends worker identity in `Anthropic-Worker-ID`, not in
    // the query string. Long polling repeatedly drives the same authoritative
    // atomic claim; it does not introduce a second queue or lease registry.
    let lease_owner = work_lease_owner(&headers)?;
    // Worker ID is an optional observation label in the raw Work API. The
    // higher-level WorkPoller supplies it; the generated `work.poll()` method
    // does not require it. When omitted, the opaque credential owner is also the
    // least-privilege liveness coordinate rather than a fabricated public ID.
    let poller_id = worker_id(&headers).unwrap_or(&lease_owner);
    let started = tokio::time::Instant::now();
    loop {
        let claimed = state
            .claim_work(
                &id,
                &lease_owner,
                poller_id,
                now_ms(),
                poll.reclaim_older_than_ms,
            )
            .await
            .map_err(map_execution_error)?;
        if let Some(work) = claimed {
            let secret = match &work.data {
                awaken_session_contract::work_queue::WorkPayload::Session { .. } => {
                    environment_credential(&headers)
                        .map(encode_work_secret)
                        .transpose()?
                }
                awaken_session_contract::work_queue::WorkPayload::HealthCheck { .. } => None,
            };
            return Ok(Json(Some(crate::work_queue::project_work_with_secret(
                &work, secret,
            ))));
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

fn encode_work_secret(sessions_token: &str) -> Result<String, WireError> {
    let payload = serde_json::to_vec(&WorkSecret {
        sessions_token: sessions_token.to_string(),
        api_base_url: None,
    })
    .map_err(|error| bad_request(format!("could not encode Work secret: {error}")))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload))
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
    State(state): State<Arc<EnvironmentExecutionApplication>>,
    Path(id): Path<String>,
) -> Result<Json<WorkQueueStats>, WireError> {
    let s = state
        .work_stats(&id, now_ms())
        .await
        .map_err(map_execution_error)?;
    Ok(Json(WorkQueueStats {
        object_type: crate::types::environment::WorkQueueStatsObjectType::WorkQueueStats,
        depth: s.depth,
        pending: s.pending,
        oldest_queued_at: s.oldest_queued_at.into(),
        workers_polling: Some(s.workers_polling).into(),
    }))
}

pub(super) async fn retrieve_work(
    State(state): State<Arc<EnvironmentExecutionApplication>>,
    Path((id, wid)): Path<(String, String)>,
) -> Result<Json<Work>, WireError> {
    let work = state
        .get_work(&id, &wid)
        .await
        .map_err(map_execution_error)?
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

pub(super) async fn update_work(
    State(state): State<Arc<EnvironmentExecutionApplication>>,
    Path((id, wid)): Path<(String, String)>,
    ManagedJson(params): ManagedJson<WorkUpdateParams>,
) -> Result<Json<Work>, WireError> {
    let work = state
        .update_work_metadata(&id, &wid, params.metadata)
        .await
        .map_err(map_execution_error)?
        .ok_or_else(|| not_found("work"))?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

/// `POST …/work/:wid/ack` — the worker acknowledges it picked up the item.
pub(super) async fn ack_work(
    State(state): State<Arc<EnvironmentExecutionApplication>>,
    Path((id, wid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Work>, WireError> {
    let owner = work_lease_owner(&headers)?;
    let result = state
        .acknowledge_work(&id, &wid, &owner)
        .await
        .map_err(map_execution_error)?;
    let work = worker_mutation(result)?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

/// `POST …/work/:wid/heartbeat` — extend the lease; returns the TTL.
pub(super) async fn heartbeat_work(
    State(state): State<Arc<EnvironmentExecutionApplication>>,
    Path((id, wid)): Path<(String, String)>,
    headers: HeaderMap,
    Query(params): Query<HeartbeatParams>,
) -> Result<Json<WorkHeartbeat>, WireError> {
    let command = LeaseHeartbeat {
        condition: crate::work_queue::HeartbeatCondition::from_wire(
            params.expected_last_heartbeat.as_deref(),
        ),
        desired_ttl_seconds: params.desired_ttl_seconds,
    };
    let owner = work_lease_owner(&headers)?;
    let hb = match state
        .heartbeat_work(&id, &wid, &owner, now_ms(), command)
        .await
        .map_err(map_execution_error)?
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
        object_type: crate::types::environment::WorkHeartbeatObjectType::WorkHeartbeat,
        last_heartbeat: hb.last_heartbeat,
        lease_extended: hb.lease_extended,
        state: crate::work_queue::project_state(hb.state),
        ttl_seconds: hb.ttl_seconds,
    }))
}

/// Resolve the one WorkQueue lease owner from the official Managed wire.
///
/// The SDK's WorkPoller sends `Anthropic-Worker-ID` only on `poll`; its
/// `ack`/`stop` calls carry the same Environment Key as a bearer instead. The
/// generated raw Work client also permits no Worker ID and uses its API-key
/// credential throughout. The credential is therefore the stable authority.
/// Hashing keeps the secret out of durable queue rows. Raw/manual callers without
/// a bearer retain the documented Worker header path, and both paths still enter
/// the same atomic WorkQueue owner comparison.
fn work_lease_owner(headers: &HeaderMap) -> Result<String, WireError> {
    if let Some(credential) = environment_credential(headers) {
        return Ok(format!(
            "managed-environment:{}",
            super::super::sha256_identity("managed-work-environment-key-v1", &[credential])
        ));
    }
    worker_id(headers).map(str::to_owned).ok_or_else(|| {
        bad_request(
            "Environment bearer or Anthropic-Worker-ID is required for Work lease operations",
        )
    })
}

fn worker_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("anthropic-worker-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn environment_credential(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .and_then(|authorization| authorization.split_once(' '))
        .filter(|(scheme, credential)| {
            scheme.eq_ignore_ascii_case("bearer") && !credential.trim().is_empty()
        })
        .map(|(_, credential)| credential.trim());
    bearer.or_else(|| {
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
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
    State(state): State<Arc<EnvironmentExecutionApplication>>,
    Path((id, wid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Work>, WireError> {
    // The official SDK sends `{}` or `{force}`; retain the earlier empty-body
    // transport as the sole backwards-compatible alias and validate every
    // non-empty body against the exact 0.120 DTO.
    let _params = if body.is_empty() {
        WorkStopParams::default()
    } else {
        serde_json::from_slice::<WorkStopParams>(&body)
            .map_err(|error| bad_request(format!("invalid Work stop request: {error}")))?
    };
    let owner = work_lease_owner(&headers)?;
    let result = state
        .stop_work(&id, &wid, &owner)
        .await
        .map_err(map_execution_error)?;
    if matches!(
        result,
        awaken_session_contract::work_queue::WorkMutationResult::PreconditionFailed
    ) {
        let current = state
            .get_work(&id, &wid)
            .await
            .map_err(map_execution_error)?;
        if current.is_some_and(|work| {
            work.state == awaken_session_contract::work_queue::WorkState::Stopped
        }) {
            return Err((
                StatusCode::CONFLICT,
                Json(ErrorResponse::new(
                    "conflict_error",
                    "Work is already stopped",
                )),
            ));
        }
    }
    let work = worker_mutation(result)?;
    Ok(Json(crate::work_queue::project_work(&work)))
}

fn worker_mutation(
    result: awaken_session_contract::work_queue::WorkMutationResult,
) -> Result<awaken_session_contract::work_queue::WorkItem, WireError> {
    match result {
        awaken_session_contract::work_queue::WorkMutationResult::Accepted(work) => Ok(*work),
        awaken_session_contract::work_queue::WorkMutationResult::PreconditionFailed => Err((
            StatusCode::PRECONDITION_FAILED,
            Json(ErrorResponse::new(
                "precondition_error",
                "Worker does not own the current Work lease",
            )),
        )),
        awaken_session_contract::work_queue::WorkMutationResult::NotFound => Err(not_found("work")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_failures_preserve_work_http_taxonomy() {
        // Cause/effect graph: C1 current Environment absent; C2 registration
        // authority unavailable; C3 WorkQueue unavailable. Effects: E1 404 only
        // for definitive absence, E2/E3 503 and never empty/404/precondition.
        // Decision rows M1=C1->404, M2=C2->503, M3=C3->503.
        for (rule, error, expected) in [
            (
                "M1",
                EnvironmentExecutionError::EnvironmentNotFound,
                StatusCode::NOT_FOUND,
            ),
            (
                "M2",
                EnvironmentExecutionError::Registration(
                    awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError::Storage(
                        "catalog outage".into(),
                    ),
                ),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "M3",
                EnvironmentExecutionError::WorkQueue(
                    awaken_session_contract::work_queue::WorkQueueError::Storage(
                        "queue outage".into(),
                    ),
                ),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            assert_eq!(map_execution_error(error).0, expected, "{rule}");
        }
    }

    #[test]
    fn poll_secret_is_the_exact_sdk_base64url_dto() {
        // Cause/effect graph: C1 a Session Work poll carries the already-authenticated
        // Environment credential; C2 list/retrieve use the ordinary projector.
        // Effects: E1 poll encodes exactly BetaWorkSecret with sessions_token and no
        // leaked lease owner; E2 non-poll projections remain null. Decision table:
        // R1 C1->E1; R2 C2->E2. No signer/store/credential authority is duplicated.
        let encoded = encode_work_secret("environment-key").expect("R1");
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .expect("R1 base64url");
        let secret: WorkSecret = serde_json::from_slice(&decoded).expect("R1 typed JSON");
        assert_eq!(secret.sessions_token, "environment-key", "R1/E1");
        assert_eq!(secret.api_base_url, None, "R1/E1");
    }
}
