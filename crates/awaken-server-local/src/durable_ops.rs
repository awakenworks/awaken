//! The durable-ingress operations surface (slice E): the ADR-0009 follow-on verbs
//! exposed over HTTP, each operating on one thread's durable dispatch queue.
//!
//! - `supersede` (ADR-0022): a newest-wins turn over the thread's stale
//!   pending/parked work; the superseded runs are observable via `superseded`.
//! - `reconcile` (ADR-0011): reclaim and re-run any dispatch left runnable by a
//!   crash.
//! - `reap` / `dead-letters` / `dead-letters/purge` (ADR-0015): dead-letter a
//!   crashed dispatch that has exhausted its crash-retry budget, list the
//!   dead-lettered runs, and GC them.
//!
//! Every route needs durable ingress (`AWAKEN_INGRESS=durable`) and fails closed
//! with 400 otherwise. The deep dead-letter/recovery state machine is proven
//! deterministically at the store level (`awaken-run-ingress`'s `sqlite_dispatch`
//! tests) on this same SQLite stack; this surface makes the verbs operable and
//! observable end-to-end over HTTP.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::host::{BASE_SEQ, HostError, HostErrorKind, SharedHost};

/// The durable operations router. Mounted on every server; each route fails
/// closed with 400 when the server is not in durable mode.
pub fn durable_ops_router(host: Arc<SharedHost>) -> Router {
    Router::new()
        .route("/v1/durable/threads/:thread/supersede", post(supersede))
        .route("/v1/durable/threads/:thread/superseded", get(superseded))
        .route("/v1/durable/threads/:thread/reconcile", post(reconcile))
        .route("/v1/durable/threads/:thread/reap", post(reap))
        .route(
            "/v1/durable/threads/:thread/dead-letters",
            get(dead_letters),
        )
        .route(
            "/v1/durable/threads/:thread/dead-letters/purge",
            post(purge),
        )
        .with_state(host)
}

fn respond(result: Result<Value, HostError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => {
            let status = match error.kind {
                HostErrorKind::BadRequest => StatusCode::BAD_REQUEST,
                HostErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (status, Json(json!({ "error": error.message })))
        }
    }
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
                MessageId(format!(
                    "ops-user-{}",
                    BASE_SEQ.fetch_add(1, Ordering::SeqCst)
                )),
                Role::User,
                text,
            );
            let turn = host.supersede_turn(agent, &thread, vec![message]).await?;
            let superseded = host.superseded(&thread).await?;
            Ok(json!({
                "phase": format!("{:?}", turn.phase),
                "superseded": superseded,
            }))
        }
        .await,
    )
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
