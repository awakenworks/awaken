//! Secret-free HTTP coordination around IAM's canonical desktop OAuth flow.

use std::sync::Arc;

use awaken_api_contract::ApiError;
use axum::extract::{FromRef, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use super::{Problem, req_id};

/// Secret-free state of the one product-coordinated Cloud login operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CloudLoginStatusView {
    pub state: CloudLoginState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorize_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CloudLoginState {
    SignInRequired,
    Authorizing,
    Authenticated,
    Failed,
}

/// Product application port around IAM's canonical desktop OAuth adapter.
/// Implementations coordinate concurrency only; OAuth state and credentials
/// remain owned by IAM.
#[async_trait::async_trait]
pub trait CloudLoginApplication: Send + Sync {
    async fn status(&self) -> CloudLoginStatusView;
    async fn start(&self) -> CloudLoginStatusView;
    async fn logout(&self) -> Result<(), String>;
}

#[derive(Clone)]
pub(super) struct CloudLoginHandle(pub Option<Arc<dyn CloudLoginApplication>>);

pub(super) fn cloud_login_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    CloudLoginHandle: FromRef<S>,
{
    Router::new().route(
        "/v1/config/cloud-login",
        get(get_cloud_login)
            .post(start_cloud_login)
            .delete(logout_cloud_login),
    )
}

fn cloud_login_unavailable(headers: &HeaderMap) -> Response {
    Problem(ApiError::new(
        404,
        "cloud_login_unavailable",
        "Cloud login unavailable",
        "this deployment does not expose the Cloud login operation",
        req_id(headers),
    ))
    .into_response()
}

async fn get_cloud_login(State(handle): State<CloudLoginHandle>, headers: HeaderMap) -> Response {
    match handle.0 {
        Some(application) => Json(application.status().await).into_response(),
        None => cloud_login_unavailable(&headers),
    }
}

async fn start_cloud_login(State(handle): State<CloudLoginHandle>, headers: HeaderMap) -> Response {
    match handle.0 {
        Some(application) => Json(application.start().await).into_response(),
        None => cloud_login_unavailable(&headers),
    }
}

async fn logout_cloud_login(
    State(handle): State<CloudLoginHandle>,
    headers: HeaderMap,
) -> Response {
    let Some(application) = handle.0 else {
        return cloud_login_unavailable(&headers);
    };
    match application.logout().await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => Problem(ApiError::new(
            503,
            "cloud_logout_failed",
            "Cloud logout failed",
            "the canonical Cloud credential owner could not complete logout",
            req_id(&headers),
        ))
        .into_response(),
    }
}
