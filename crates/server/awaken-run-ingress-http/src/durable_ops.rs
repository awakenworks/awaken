//! Coordinator-owned HTTP interface for durable Session operations.

use awaken_agent_contract::agent::content::extract_text;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_run_ingress::{ApplicationError, ApplicationErrorKind, DurableRunOperations};
use awaken_session_contract::{
    RunErrorKind, SessionRunBackgroundApplication, SessionRunReplacementApplication,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// The durable operations router. Mounted on every server; each route fails
/// closed with 400 when the server is not in durable mode.
pub fn durable_ops_router(application: Arc<dyn DurableRunOperations>) -> Router {
    durable_ops_router_with_session_commands(
        application.clone(),
        BackgroundApplication::Legacy(application.clone()),
        SupersedeApplication::Legacy(application),
    )
}

/// Compose the operational query/control verbs with the canonical Session
/// replacement application. Only this constructor may expose newest-wins for a
/// Session root; the legacy constructor remains available to isolated queue
/// adapters and tests whose Threads are not Session aggregates.
pub fn durable_ops_router_with_session_admission(
    application: Arc<dyn DurableRunOperations>,
    background: Arc<dyn SessionRunBackgroundApplication>,
    supersede: Arc<dyn SessionRunReplacementApplication>,
) -> Router {
    durable_ops_router_with_session_commands(
        application.clone(),
        BackgroundApplication::Session(background),
        SupersedeApplication::Session {
            application: supersede,
            operations: application,
        },
    )
}

#[derive(Clone)]
enum BackgroundApplication {
    Legacy(Arc<dyn DurableRunOperations>),
    Session(Arc<dyn SessionRunBackgroundApplication>),
}

#[derive(Clone)]
enum SupersedeApplication {
    Legacy(Arc<dyn DurableRunOperations>),
    Session {
        application: Arc<dyn SessionRunReplacementApplication>,
        operations: Arc<dyn DurableRunOperations>,
    },
}

fn durable_ops_router_with_session_commands(
    application: Arc<dyn DurableRunOperations>,
    background_application: BackgroundApplication,
    supersede_application: SupersedeApplication,
) -> Router {
    Router::new()
        .route("/v1/durable/threads/{thread}/cancel", post(cancel))
        .route("/v1/durable/threads/{thread}/pause", post(pause))
        .route("/v1/durable/threads/{thread}/resume", post(resume))
        .route("/v1/durable/threads/{thread}/wake", post(wake))
        .route("/v1/durable/threads/{thread}/deliver", post(deliver))
        .route("/v1/durable/threads/{thread}/superseded", get(superseded))
        .route("/v1/durable/threads/{thread}/dispatches", get(dispatches))
        .route("/v1/durable/threads/{thread}/messages", get(messages))
        .route("/v1/durable/threads/{thread}/reconcile", post(reconcile))
        .route(
            "/v1/durable/threads/{thread}/quarantine-retry-exhausted",
            post(quarantine_retry_exhausted),
        )
        .route(
            "/v1/durable/threads/{thread}/dead-letters",
            get(dead_letters),
        )
        .route(
            "/v1/durable/threads/{thread}/dead-letters/{run_id}/requeue",
            post(requeue_dead_letter),
        )
        .route(
            "/v1/durable/threads/{thread}/dead-letters/purge",
            post(purge),
        )
        .with_state(application)
        .merge(
            Router::new()
                .route(
                    "/v1/durable/threads/{thread}/submit_background",
                    post(submit_background),
                )
                .with_state(background_application),
        )
        .merge(
            Router::new()
                .route("/v1/durable/threads/{thread}/supersede", post(supersede))
                .with_state(supersede_application),
        )
}

async fn submit_background(
    State(application): State<BackgroundApplication>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let text = body
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ApplicationError::invalid("`text` is required"))?
                .to_string();
            let agent = body.get("agent").and_then(|v| v.as_str());
            let message = Message::text(
                MessageId(awaken_runtime::fresh_process_id("ops-user")),
                Role::User,
                text,
            );
            let operation_id = message.id.0.clone();
            let run_id = match application {
                BackgroundApplication::Legacy(application) => {
                    application
                        .submit_background(agent, &thread, vec![message])
                        .await?
                }
                BackgroundApplication::Session(application) => {
                    application
                        .submit_session_run_background(
                            &operation_id,
                            &thread,
                            agent.map(str::to_string),
                            vec![message],
                            awaken_observability::current_traceparent(),
                        )
                        .await
                        .map_err(map_run_error)?
                        .0
                }
            };
            Ok(json!({ "run_id": run_id, "queued": true }))
        }
        .await,
    )
}

fn map_run_error(error: awaken_session_contract::RunError) -> ApplicationError {
    match error.kind {
        RunErrorKind::BadRequest => ApplicationError::invalid(error.message),
        RunErrorKind::Unavailable => ApplicationError::unavailable(error.message),
        RunErrorKind::Internal => ApplicationError::internal(error.message),
    }
}

/// Cancel a run by id via the durable live-control seam (ADR-0018): live channel
/// first, then a durable cancel of a queued/awaiting dispatch. `{ run_id }`.
async fn cancel(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let run_id = body
                .get("run_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ApplicationError::invalid("`run_id` is required"))?;
            application.cancel(&thread, run_id).await?;
            Ok(json!({ "cancelled": true, "run_id": run_id }))
        }
        .await,
    )
}

/// Cooperatively pause an active run by id. The request fails closed when the
/// run has no live owner; an accepted pause becomes a durable awaiting ticket.
async fn pause(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let run_id = application
                .pause(&thread, body.get("run_id").and_then(|v| v.as_str()))
                .await?;
            Ok(json!({ "paused": true, "run_id": run_id }))
        }
        .await,
    )
}

/// RunResume exactly a committed `ManualPause` ticket with `{ text }`.
async fn resume(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let text = body
                .get("text")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ApplicationError::invalid("`text` is required"))?
                .to_string();
            let run_id = application.resume(&thread, text).await?;
            Ok(json!({ "resumed": true, "run_id": run_id }))
        }
        .await,
    )
}

/// Wake a live run by id via the durable live-control seam (ADR-0018). Live-only
/// and fail-closed: no live subscriber → 400. `{ run_id }`.
async fn wake(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let run_id = body
                .get("run_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ApplicationError::invalid("`run_id` is required"))?;
            application.wake(&thread, run_id).await?;
            Ok(json!({ "woken": true, "run_id": run_id }))
        }
        .await,
    )
}

/// Stage a durable cross-thread decision for the thread's awaiting run; the daemon
/// relays it from the outbox and wakes the run (ADR-0017). `{ allow: bool }`.
async fn deliver(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let allow = body.get("allow").and_then(|v| v.as_bool()).unwrap_or(true);
            let run_id = application.deliver(&thread, allow).await?;
            Ok(json!({ "run_id": run_id, "staged": true }))
        }
        .await,
    )
}

async fn supersede(
    State(application): State<SupersedeApplication>,
    Path(thread): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    respond(
        async {
            let text = body
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ApplicationError::invalid("`text` is required"))?
                .to_string();
            let agent = body.get("agent").and_then(|v| v.as_str());
            let message = Message::text(
                MessageId(awaken_runtime::fresh_process_id("ops-user")),
                Role::User,
                text,
            );
            match application {
                SupersedeApplication::Legacy(application) => {
                    let outcome = application.supersede(agent, &thread, vec![message]).await?;
                    Ok(json!({
                        "state": outcome.state,
                        "superseded": outcome.superseded,
                    }))
                }
                SupersedeApplication::Session {
                    application,
                    operations,
                } => {
                    let operation_id = message.id.0.clone();
                    let outcome = application
                        .supersede_session_run(
                            &operation_id,
                            &thread,
                            agent.map(str::to_string),
                            vec![message],
                        )
                        .await
                        .map_err(map_run_error)?;
                    let superseded = operations.superseded(&thread).await?;
                    Ok(json!({
                        "state": format!("{:?}", outcome.state()),
                        "superseded": superseded,
                    }))
                }
            }
        }
        .await,
    )
}

/// Committed truth for `thread` — the observation channel for out-of-band work
/// (e.g. a daemon-drained run), which never flows through a session's in-memory
/// event log. Returns each committed message's role and text.
async fn messages(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(application.messages(&thread).await.map(|committed| {
        let out: Vec<Value> = committed
            .iter()
            .map(|m| json!({ "role": format!("{:?}", m.role), "text": extract_text(&m.content) }))
            .collect();
        json!({ "messages": out })
    }))
}

async fn superseded(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(
        application
            .superseded(&thread)
            .await
            .map(|ids| json!({ "superseded": ids })),
    )
}

/// An operational snapshot of the thread's dispatch queue (ADR-0025): every row in
/// enqueue order with its status and attempt count.
async fn dispatches(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(application.dispatches(&thread).await.map(|rows| {
        let out: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                json!({
                    "run_id": row.run_id,
                    "status": row.status,
                    "attempts": row.attempts,
                    "sandbox_bound": row.sandbox_bound,
                })
            })
            .collect();
        json!({ "dispatches": out })
    }))
}

async fn reconcile(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(
        application
            .reconcile(&thread)
            .await
            .map(|ids| json!({ "recovered": ids })),
    )
}

async fn quarantine_retry_exhausted(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let max_attempts = query
        .get("max_attempts")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    // Explicit operator-selected cutoff. Automatic retry exhaustion does not
    // call this route; it claims and commits terminal Run truth.
    let now = query
        .get("now_ms")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or_else(now_ms);
    respond(
        application
            .quarantine_retry_exhausted(&thread, max_attempts, now)
            .await
            .map(|n| json!({ "quarantined": n })),
    )
}

async fn dead_letters(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(
        application
            .dead_letters(&thread)
            .await
            .map(|ids| json!({ "dead_letters": ids })),
    )
}

async fn requeue_dead_letter(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path((thread, run_id)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    respond(
        application
            .requeue_dead_letter(&thread, &run_id)
            .await
            .and_then(|requeued| {
                requeued
                    .then(|| json!({ "run_id": run_id, "requeued": true }))
                    .ok_or_else(|| {
                        ApplicationError::conflict(
                            "the run is not dead-lettered in the requested thread",
                        )
                    })
            }),
    )
}

async fn purge(
    State(application): State<Arc<dyn DurableRunOperations>>,
    Path(thread): Path<String>,
) -> (StatusCode, Json<Value>) {
    respond(
        application
            .purge_dead_letters(&thread)
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

fn respond(result: Result<Value, ApplicationError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => {
            let status = match error.kind {
                ApplicationErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
                ApplicationErrorKind::Conflict => StatusCode::CONFLICT,
                ApplicationErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
                ApplicationErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            };
            (status, Json(json!({ "error": error.message })))
        }
    }
}
