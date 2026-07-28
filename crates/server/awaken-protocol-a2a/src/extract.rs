//! A2A-specific HTTP request extractors.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Json, Request};
use axum::http::StatusCode;

use crate::types::ErrorResponse;

/// Decode JSON while preserving the A2A error envelope on malformed input.
pub(crate) struct A2aJson<T>(pub(crate) T);

impl<S, T> FromRequest<S> for A2aJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = (StatusCode, Json<ErrorResponse>);

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(-32600, rejection.body_text())),
            )),
        }
    }
}
