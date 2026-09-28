//! Coordinator projection of Worker fleet readiness for a hosted product plane.
//!
//! A product plane asks, before it offers work to an AI workforce, whether any
//! Worker can run the capabilities that work requires. Only the Coordinator
//! holds that answer: it owns the Worker directory. This surface hands back the
//! secret-free projection and nothing else — no Worker identity, credential, or
//! address crosses it.
//!
//! The route is mounted on the service-authenticated product surface beside the
//! other unscoped `/v1` reads, not on the Managed data plane, because a fleet
//! is a deployment-wide fact rather than a Workspace-scoped resource.

use std::sync::Arc;

use awaken_worker_registry::{WorkerDirectory, fleet_readiness};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReadinessQuery {
    /// Comma-joined capability list. Absent and empty both mean "can any
    /// Worker take work at all", which is a weaker question than naming a
    /// capability, not a malformed one.
    capabilities: Option<String>,
}

/// `GET /v1/workers/readiness` over the live Worker directory.
/// `clock` is the Coordinator's wall clock; registration liveness is judged
/// on it.
pub fn router(
    directory: Arc<dyn WorkerDirectory>,
    clock: Arc<dyn awaken_lease_contract::WallClock>,
) -> Router {
    Router::new()
        .route("/v1/workers/readiness", get(readiness))
        .with_state((directory, clock))
}

async fn readiness(
    State((directory, clock)): State<(
        Arc<dyn WorkerDirectory>,
        Arc<dyn awaken_lease_contract::WallClock>,
    )>,
    Query(query): Query<ReadinessQuery>,
) -> Response {
    let required = parse_capabilities(query.capabilities.as_deref());
    let workers = match directory.list().await {
        Ok(workers) => workers,
        // The directory is the authority, so an unreadable directory is a
        // retryable dependency failure, never an empty fleet: answering "no
        // Workers" here would let a caller conclude the fleet is gone.
        Err(error) => {
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("worker directory is unavailable: {error}"),
            );
        }
    };
    let now_ms = awaken_lease_contract::WallClockExt::now_ms(clock.as_ref());
    Json(fleet_readiness(&workers, &required, now_ms)).into_response()
}

/// Split the wire form into capabilities. Blank entries are dropped rather
/// than rejected: a trailing comma from a caller that joined an empty tail is
/// not a different question.
fn parse_capabilities(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn problem(status: StatusCode, detail: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({
            "type": "about:blank",
            "title": status.canonical_reason().unwrap_or("Worker fleet error"),
            "status": status.as_u16(),
            "detail": detail.into(),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
