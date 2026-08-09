//! Coordinator-owned HTTP interface for durable Session operations.

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_runtime_host::{HostError, HostErrorKind, SharedHost, block_text};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

fn respond(result: Result<Value, HostError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => {
            let status = match error.kind {
                HostErrorKind::BadRequest => StatusCode::BAD_REQUEST,
                HostErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
                HostErrorKind::Conflict => StatusCode::CONFLICT,
            };
            (status, Json(json!({ "error": error.message })))
        }
    }
}

/// The durable operations router. Mounted on every server; each route fails
/// closed with 400 when the server is not in durable mode.
pub fn durable_ops_router(host: Arc<SharedHost>) -> Router {
    Router::new()
        .route(
            "/v1/durable/threads/{thread}/submit_background",
            post(submit_background),
        )
        .route("/v1/durable/threads/{thread}/cancel", post(cancel))
        .route("/v1/durable/threads/{thread}/pause", post(pause))
        .route("/v1/durable/threads/{thread}/resume", post(resume))
        .route("/v1/durable/threads/{thread}/wake", post(wake))
        .route("/v1/durable/threads/{thread}/deliver", post(deliver))
        .route("/v1/durable/threads/{thread}/supersede", post(supersede))
        .route("/v1/durable/threads/{thread}/superseded", get(superseded))
        .route("/v1/durable/threads/{thread}/dispatches", get(dispatches))
        .route("/v1/durable/threads/{thread}/messages", get(messages))
        .route("/v1/durable/threads/{thread}/reconcile", post(reconcile))
        .route("/v1/durable/threads/{thread}/reap", post(reap))
        .route(
            "/v1/durable/threads/{thread}/dead-letters",
            get(dead_letters),
        )
        .route(
            "/v1/durable/threads/{thread}/dead-letters/purge",
            post(purge),
        )
        .with_state(host)
}

async fn submit_background(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let text = body
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| HostError::bad_request("`text` is required"))?
                .to_string();
            let agent = body.get("agent").and_then(|v| v.as_str());
            let message = Message::text(
                MessageId(awaken_runtime::fresh_process_id("ops-user")),
                Role::User,
                text,
            );
            let run_id = host
                .submit_background_async(agent, &thread, vec![message])
                .await?;
            Ok(json!({ "run_id": run_id, "queued": true }))
        }
        .await,
    )
}

/// Cancel a run by id via the durable live-control seam (ADR-0018): live channel
/// first, then a durable cancel of a queued/awaiting dispatch. `{ run_id }`.
async fn cancel(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let run_id = body
                .get("run_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| HostError::bad_request("`run_id` is required"))?;
            host.cancel_durable(&thread, run_id).await?;
            Ok(json!({ "cancelled": true, "run_id": run_id }))
        }
        .await,
    )
}

/// Cooperatively pause an active run by id. The request fails closed when the
/// run has no live owner; an accepted pause becomes a durable awaiting ticket.
async fn pause(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let run_id = host
                .pause_durable(&thread, body.get("run_id").and_then(|v| v.as_str()))
                .await?;
            Ok(json!({ "paused": true, "run_id": run_id }))
        }
        .await,
    )
}

/// RunResume exactly a committed `ManualPause` ticket with `{ text }`.
async fn resume(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let text = body
                .get("text")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| HostError::bad_request("`text` is required"))?
                .to_string();
            let run_id = host.stage_manual_resume(&thread, text).await?;
            Ok(json!({ "resumed": true, "run_id": run_id }))
        }
        .await,
    )
}

/// Wake a live run by id via the durable live-control seam (ADR-0018). Live-only
/// and fail-closed: no live subscriber → 400. `{ run_id }`.
async fn wake(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let run_id = body
                .get("run_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| HostError::bad_request("`run_id` is required"))?;
            host.wake_durable(&thread, run_id).await?;
            Ok(json!({ "woken": true, "run_id": run_id }))
        }
        .await,
    )
}

/// Stage a durable cross-thread decision for the thread's awaiting run; the daemon
/// relays it from the outbox and wakes the run (ADR-0017). `{ allow: bool }`.
async fn deliver(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let allow = body.get("allow").and_then(|v| v.as_bool()).unwrap_or(true);
            let run_id = host.stage_decision(&thread, allow).await?;
            Ok(json!({ "run_id": run_id, "staged": true }))
        }
        .await,
    )
}

async fn supersede(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let text = body
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| HostError::bad_request("`text` is required"))?
                .to_string();
            let agent = body.get("agent").and_then(|v| v.as_str());
            let message = Message::text(
                MessageId(awaken_runtime::fresh_process_id("ops-user")),
                Role::User,
                text,
            );
            let turn = host.supersede_run(agent, &thread, vec![message]).await?;
            let superseded = host.superseded(&thread).await?;
            Ok(json!({
                "state": format!("{:?}", turn.state),
                "superseded": superseded,
            }))
        }
        .await,
    )
}

/// Committed truth for `thread` — the observation channel for out-of-band work
/// (e.g. a daemon-drained run), which never flows through a session's in-memory
/// event log. Returns each committed message's role and text.
async fn messages(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    let committed = host.committed_messages(&thread).await;
    let out: Vec<Value> = committed
        .iter()
        .map(|m| json!({ "role": format!("{:?}", m.role), "text": block_text(&m.content) }))
        .collect();
    (StatusCode::OK, Json(json!({ "messages": out })))
}

async fn superseded(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(
        host.superseded(&thread)
            .await
            .map(|ids| json!({ "superseded": ids })),
    )
}

/// An operational snapshot of the thread's dispatch queue (ADR-0025): every row in
/// enqueue order with its status and attempt count.
async fn dispatches(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(host.list_dispatches(&thread).await.map(|rows| {
        let out: Vec<Value> = rows
            .into_iter()
            .map(|(run_id, status, attempts, sandbox_bound)| {
                json!({
                    "run_id": run_id,
                    "status": status,
                    "attempts": attempts,
                    "sandbox_bound": sandbox_bound,
                })
            })
            .collect();
        json!({ "dispatches": out })
    }))
}

async fn reconcile(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(
        host.reconcile(&thread)
            .await
            .map(|ids| json!({ "recovered": ids })),
    )
}

async fn reap(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let max_attempts = query
        .get("max_attempts")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    // As-of clock for the reap. Defaults to now; a caller may pass a cutoff to
    // reap dispatches whose lease has expired by that time.
    let now = query
        .get("now_ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or_else(now_ms);
    respond(
        host.reap(&thread, max_attempts, now)
            .await
            .map(|n| json!({ "dead_lettered": n })),
    )
}

async fn dead_letters(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(
        host.dead_letters(&thread)
            .await
            .map(|ids| json!({ "dead_letters": ids })),
    )
}

async fn purge(
    State(host): State<Arc<SharedHost>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(
        host.purge_dead_letters(&thread)
            .await
            .map(|n| json!({ "purged": n })),
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
