//! Authenticated, read-only Control-to-Coordinator Worker observation boundary.
//!
//! Worker registration and heartbeat authority remains the Coordinator-owned
//! [`WorkerDirectory`](awaken_worker_registry::WorkerDirectory). Split Control receives only this secret-free projection;
//! it never opens the registry database or gains mutation capability.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_worker_registry::{RegisteredWorker, RegistryError, WorkerObservationSource};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};

pub const WORKER_OBSERVATIONS_PATH: &str = "/internal/v1/workers/observations";
pub const WORKER_OBSERVE_PERMISSION: &str = "worker:observe";
const ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct BoundaryState {
    source: Arc<dyn WorkerObservationSource>,
    authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
}

pub fn router(
    source: Arc<dyn WorkerObservationSource>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    Ok(router_with_authenticator(
        source,
        awaken_service_auth_contract::static_token_authenticator(bearer_token)?,
    ))
}

pub fn router_with_authenticator(
    source: Arc<dyn WorkerObservationSource>,
    authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
) -> Router {
    Router::new()
        .route(WORKER_OBSERVATIONS_PATH, get(handle_list))
        .with_state(BoundaryState {
            source,
            authenticator,
        })
}

async fn handle_list(
    State(state): State<BoundaryState>,
    headers: HeaderMap,
) -> axum::response::Response {
    match state.authenticator.authenticate(
        headers
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes()),
        awaken_service_auth_contract::ServiceAuthorizationRequirement::new(
            awaken_service_auth_contract::COORDINATOR_SERVICE_AUDIENCE,
            WORKER_OBSERVE_PERMISSION,
        ),
    ) {
        Ok(_) => {}
        Err(awaken_service_auth_contract::ServiceAuthError::Unauthorized) => {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(awaken_service_auth_contract::ServiceAuthError::Forbidden) => {
            return StatusCode::FORBIDDEN.into_response();
        }
        Err(awaken_service_auth_contract::ServiceAuthError::Unavailable(_)) => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    match state.source.list().await {
        Ok(workers) => (StatusCode::OK, Json(Ok::<_, String>(workers))).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Err::<Vec<RegisteredWorker>, _>(error.to_string())),
        )
            .into_response(),
    }
}

/// Read-only remote adapter used by a split Control process.
#[derive(Clone)]
pub struct HttpWorkerObservationSource {
    endpoint: String,
    token_source: Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>,
    client: reqwest::Client,
}

impl HttpWorkerObservationSource {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, String> {
        Self::with_token_source(
            base_url,
            awaken_service_auth_contract::static_token_source(bearer_token)?,
        )
    }

    pub fn with_token_source(
        base_url: impl Into<String>,
        token_source: Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>,
    ) -> Result<Self, String> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let parsed = reqwest::Url::parse(&base_url)
            .map_err(|error| format!("invalid Coordinator observation URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(
                "Coordinator observations require an http(s) base URL and non-empty bearer token"
                    .into(),
            );
        }
        awaken_service_auth_contract::resolve_service_bearer_token(token_source.as_ref())?;
        Ok(Self {
            endpoint: format!("{base_url}{WORKER_OBSERVATIONS_PATH}"),
            token_source,
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .map_err(|error| error.to_string())?,
        })
    }
}

#[async_trait]
impl WorkerObservationSource for HttpWorkerObservationSource {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        let mut last_error = "Coordinator Worker-observation service unavailable".to_owned();
        for attempt in 0..ATTEMPTS {
            let bearer_token = awaken_service_auth_contract::resolve_service_bearer_token(
                self.token_source.as_ref(),
            )
            .map_err(RegistryError::Persistence)?;
            match self
                .client
                .get(&self.endpoint)
                .bearer_auth(bearer_token.as_ref())
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json::<Result<Vec<RegisteredWorker>, String>>()
                        .await
                        .map_err(|error| {
                            RegistryError::Persistence(format!(
                                "decode Coordinator Worker observations: {error}"
                            ))
                        })?
                        .map_err(RegistryError::Persistence);
                }
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    let rotated = awaken_service_auth_contract::service_bearer_token_rotated(
                        self.token_source.as_ref(),
                        bearer_token.as_ref(),
                    )
                    .map_err(RegistryError::Persistence)?;
                    if attempt + 1 < ATTEMPTS && rotated {
                        continue;
                    }
                    return Err(RegistryError::Persistence(
                        "Coordinator Worker-observation credentials were rejected".into(),
                    ));
                }
                Ok(response) if response.status().is_client_error() => {
                    return Err(RegistryError::Persistence(format!(
                        "Coordinator Worker-observation request rejected: {}",
                        response.status()
                    )));
                }
                Ok(response) => {
                    last_error = format!(
                        "Coordinator Worker-observation service returned {}",
                        response.status()
                    );
                }
                Err(error) => {
                    last_error = format!("call Coordinator Worker-observation service: {error}");
                }
            }
            if attempt + 1 < ATTEMPTS {
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
        Err(RegistryError::Persistence(last_error))
    }
}

#[cfg(test)]
mod tests {
    use awaken_worker_registry::WorkerObservationSource;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    struct FixedSource;

    struct UnavailableAuthenticator;

    impl awaken_service_auth_contract::ServiceRequestAuthenticator for UnavailableAuthenticator {
        fn authenticate(
            &self,
            _authorization: Option<&[u8]>,
            _requirement: awaken_service_auth_contract::ServiceAuthorizationRequirement<'_>,
        ) -> Result<
            awaken_service_auth_contract::AuthenticatedService,
            awaken_service_auth_contract::ServiceAuthError,
        > {
            Err(awaken_service_auth_contract::ServiceAuthError::Unavailable(
                "token projection unavailable".into(),
            ))
        }
    }

    #[async_trait]
    impl WorkerObservationSource for FixedSource {
        async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn boundary_and_remote_adapter_preserve_auth_and_read_only_projection() {
        // Cause/effect graph: private token + Coordinator source -> authenticated
        // GET -> serialized secret-free observations -> Control read port.
        // Decision table: R1 valid token + healthy source -> exact projection;
        // R2 missing/wrong token -> 401 before source access; R3 malformed URL or
        // empty token -> adapter construction fails; R4 observation source
        // failure -> 503 and bounded client retry; R5 authenticator dependency
        // failure -> 503 (not a false credential rejection). This test owns
        // R1-R3/R5; R4 uses the same generic service-error branch covered by the
        // Control service boundary suite.
        let app = router(Arc::new(FixedSource), "secret").expect("observation router");
        let unauthorized = app
            .clone()
            .oneshot(
                Request::get(WORKER_OBSERVATIONS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED, "R2");

        let auth_unavailable =
            router_with_authenticator(Arc::new(FixedSource), Arc::new(UnavailableAuthenticator))
                .oneshot(
                    Request::get(WORKER_OBSERVATIONS_PATH)
                        .header("authorization", "Bearer secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        assert_eq!(
            auth_unavailable.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "R5"
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let source = HttpWorkerObservationSource::new(format!("http://{address}"), "secret")
            .expect("R1 client");
        assert!(source.list().await.expect("R1 projection").is_empty());

        assert!(
            HttpWorkerObservationSource::new("ftp://coordinator", "secret").is_err(),
            "R3"
        );
        assert!(
            HttpWorkerObservationSource::new("http://coordinator", "").is_err(),
            "R3"
        );
    }
}
