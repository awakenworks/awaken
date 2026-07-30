//! Authenticated Coordinator handler for the existing Deployment Session launch port.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;

use super::{DeploymentLaunch, DeploymentLaunchOutcome, DeploymentSessionLauncher};

pub const DEPLOYMENT_SESSION_LAUNCH_PATH: &str = "/internal/v1/deployment-sessions/launch";

#[derive(Clone)]
struct LaunchHttpState {
    launcher: Arc<dyn DeploymentSessionLauncher>,
    bearer_token: Arc<str>,
}

/// Coordinator's private adapter over the same launcher used in-process.
pub fn deployment_session_launch_router(
    launcher: Arc<dyn DeploymentSessionLauncher>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    let bearer_token = bearer_token.into();
    if bearer_token.trim().is_empty() {
        return Err("Deployment Session launch bearer token must not be empty".into());
    }
    Ok(Router::new()
        .route(DEPLOYMENT_SESSION_LAUNCH_PATH, post(launch))
        .with_state(LaunchHttpState {
            launcher,
            bearer_token: Arc::from(bearer_token),
        }))
}

async fn launch(
    State(state): State<LaunchHttpState>,
    headers: HeaderMap,
    Json(command): Json<DeploymentLaunch>,
) -> impl IntoResponse {
    if !authorized(&headers, &state.bearer_token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let outcome = state.launcher.launch(command).await;
    let status = if matches!(outcome, DeploymentLaunchOutcome::Unavailable { .. }) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    (status, Json(outcome)).into_response()
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|actual| actual.as_bytes() == expected.as_bytes())
}
