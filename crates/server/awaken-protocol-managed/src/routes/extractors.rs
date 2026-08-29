//! Managed HTTP extractors that preserve the Anthropic error envelope.

use axum::Json;
use axum::extract::multipart::MultipartRejection;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Multipart, Query, Request};
use axum::http::{StatusCode, request::Parts};

use crate::types::ErrorResponse;

type ManagedRejection = (StatusCode, Json<ErrorResponse>);

/// Decode a Managed JSON body without exposing Axum's plain-text/422 rejection.
pub(crate) struct ManagedJson<T>(pub(crate) T);

pub(super) fn managed_json_message(detail: String) -> String {
    if detail.contains("resources[") {
        format!("invalid resource: {detail}")
    } else {
        detail
    }
}

impl<S, T> FromRequest<S> for ManagedJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ManagedRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(invalid_request(managed_json_message(rejection.body_text()))),
        }
    }
}

/// Decode a Managed query without exposing Axum's default plain-text rejection.
///
/// Query syntax is part of the same public anti-corruption boundary as JSON:
/// malformed enums, numbers, or unknown fields must remain SDK-decodable
/// `invalid_request_error` responses on every Managed resource family.
pub(crate) struct ManagedQuery<T>(pub(crate) T);

impl<S, T> FromRequestParts<S> for ManagedQuery<T>
where
    Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
    S: Send + Sync,
{
    type Rejection = ManagedRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(invalid_request(rejection.body_text())),
        }
    }
}

/// Start a Managed multipart stream without exposing Axum's plain-text
/// missing/invalid-boundary rejection. Per-field stream failures remain the
/// consuming resource handler's responsibility because they can occur after
/// one or more fields have been decoded.
pub(crate) struct ManagedMultipart(pub(crate) Multipart);

impl<S> FromRequest<S> for ManagedMultipart
where
    Multipart: FromRequest<S, Rejection = MultipartRejection>,
    S: Send + Sync,
{
    type Rejection = ManagedRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Multipart::from_request(req, state).await {
            Ok(multipart) => Ok(Self(multipart)),
            Err(rejection) => Err(invalid_request(rejection.body_text())),
        }
    }
}

fn invalid_request(message: String) -> ManagedRejection {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use http_body_util::BodyExt;
    use serde::Deserialize;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::{ManagedMultipart, ManagedQuery};

    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Order {
        Asc,
        Desc,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ClosedQuery {
        order: Order,
        limit: u16,
    }

    async fn query(ManagedQuery(query): ManagedQuery<ClosedQuery>) -> axum::Json<Value> {
        let order = match query.order {
            Order::Asc => "asc",
            Order::Desc => "desc",
        };
        axum::Json(json!({ "order": order, "limit": query.limit }))
    }

    async fn multipart(ManagedMultipart(_multipart): ManagedMultipart) {}

    async fn call(uri: &str) -> (axum::http::StatusCode, Value) {
        let response = Router::new()
            .route("/", get(query))
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("router response");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("response body")
            .to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).expect("JSON response"),
        )
    }

    // Test design: shared_managed_query_boundary_is_typed_and_sdk_decodable
    // Cause/effect graph: query syntax -> typed decoding -> either handler input
    // or the Anthropic error anti-corruption boundary. No resource handler owns
    // an alternate rejection path.
    // Decision table:
    // | order | limit | extra | status | envelope/effect |
    // | valid | u16   | no    | 200    | typed handler input |
    // | other | u16   | no    | 400    | invalid_request_error |
    // | valid | text  | no    | 400    | invalid_request_error |
    // | valid | u16   | yes   | 400    | invalid_request_error |
    #[tokio::test]
    async fn shared_managed_query_boundary_is_typed_and_sdk_decodable() {
        let (status, body) = call("/?order=asc&limit=10").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body, json!({ "order": "asc", "limit": 10 }));

        for uri in [
            "/?order=newest&limit=10",
            "/?order=asc&limit=many",
            "/?order=asc&limit=10&extra=true",
        ] {
            let (status, body) = call(uri).await;
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(body["type"], "error", "{uri}");
            assert_eq!(body["error"]["type"], "invalid_request_error", "{uri}");
            assert!(
                !body["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .is_empty()
            );
        }
    }

    // Test design: malformed_multipart_boundary_is_sdk_decodable
    // Cause/effect graph: absent multipart boundary -> shared extractor rejection
    // -> Anthropic envelope; no upload handler or persistence port is reachable.
    // Decision table: valid boundary -> handler; absent/invalid boundary -> 400
    // invalid_request_error JSON rather than Axum plain text.
    #[tokio::test]
    async fn malformed_multipart_boundary_is_sdk_decodable() {
        let response = Router::new()
            .route("/", post(multipart))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "multipart/form-data")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("router response");
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &response
                .into_body()
                .collect()
                .await
                .expect("response body")
                .to_bytes(),
        )
        .expect("JSON response");
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("boundary"))
        );
    }
}
