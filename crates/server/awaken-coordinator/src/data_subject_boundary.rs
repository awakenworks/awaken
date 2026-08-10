//! Authenticated Control-to-Coordinator subject-content erasure boundary.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_runtime_contract::{ContentEraser, DataSubjectId, ErasureError};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

pub const ERASE_COORDINATOR_CONTENT_PATH: &str = "/internal/v1/data-subjects/erase";
pub const DATA_SUBJECT_ERASE_PERMISSION: &str = "data:erase";
const ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

/// The one Coordinator-owned erasure application. Captured telemetry and the
/// optional portable ACP session store remain independent adapters, while this
/// port gives Control one stable domain target and one summed receipt.
struct CoordinatorContentEraser {
    targets: Vec<Arc<dyn ContentEraser>>,
}

#[async_trait]
impl ContentEraser for CoordinatorContentEraser {
    async fn erase_subject(&self, subject: &DataSubjectId) -> Result<usize, ErasureError> {
        let mut removed = 0usize;
        for target in &self.targets {
            removed = removed
                .checked_add(target.erase_subject(subject).await?)
                .ok_or_else(|| ErasureError("Coordinator erasure receipt overflow".into()))?;
        }
        Ok(removed)
    }
}

/// Compose every subject-keyed content store owned by Coordinator. Both split
/// Coordinator and AllInOne call this factory; neither assembles a second fanout.
pub fn coordinator_content_eraser(
    captured_content: Arc<dyn ContentEraser>,
    acp_session_blob_root: Option<PathBuf>,
) -> Arc<dyn ContentEraser> {
    let mut targets = vec![captured_content];
    if let Some(root) = acp_session_blob_root {
        targets.push(Arc::new(awaken_run_executor_acp::FsSessionBlobStore::new(
            root,
        )));
    }
    Arc::new(CoordinatorContentEraser { targets })
}

#[derive(Clone, Serialize, Deserialize)]
struct EraseCommand {
    subject: DataSubjectId,
}

#[derive(Clone)]
struct BoundaryState {
    eraser: Arc<dyn ContentEraser>,
    authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
}

pub fn router(
    eraser: Arc<dyn ContentEraser>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    Ok(router_with_authenticator(
        eraser,
        awaken_service_auth_contract::static_token_authenticator(bearer_token)?,
    ))
}

pub fn router_with_authenticator(
    eraser: Arc<dyn ContentEraser>,
    authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
) -> Router {
    Router::new()
        .route(ERASE_COORDINATOR_CONTENT_PATH, post(handle_erase))
        .with_state(BoundaryState {
            eraser,
            authenticator,
        })
}

async fn handle_erase(
    State(state): State<BoundaryState>,
    headers: HeaderMap,
    Json(command): Json<EraseCommand>,
) -> axum::response::Response {
    match state.authenticator.authenticate(
        headers
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes()),
        awaken_service_auth_contract::ServiceAuthorizationRequirement::new(
            awaken_service_auth_contract::COORDINATOR_SERVICE_AUDIENCE,
            DATA_SUBJECT_ERASE_PERMISSION,
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
    match state.eraser.erase_subject(&command.subject).await {
        Ok(removed) => (StatusCode::OK, Json(Ok::<_, String>(removed))).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Err::<usize, _>(error.to_string())),
        )
            .into_response(),
    }
}

#[derive(Clone)]
pub struct HttpCoordinatorContentEraser {
    endpoint: String,
    token_source: Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>,
    client: reqwest::Client,
}

impl HttpCoordinatorContentEraser {
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
            .map_err(|error| format!("invalid Coordinator erasure URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(
                "Coordinator erasure requires an http(s) base URL without query or fragment".into(),
            );
        }
        awaken_service_auth_contract::resolve_service_bearer_token(token_source.as_ref())?;
        Ok(Self {
            endpoint: format!("{base_url}{ERASE_COORDINATOR_CONTENT_PATH}"),
            token_source,
            client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|error| error.to_string())?,
        })
    }

    async fn send(&self, subject: &DataSubjectId) -> Result<usize, ErasureError> {
        let command = EraseCommand {
            subject: subject.clone(),
        };
        let mut last_error = "Coordinator captured-content service unavailable".to_owned();
        for attempt in 0..ATTEMPTS {
            let bearer_token = awaken_service_auth_contract::resolve_service_bearer_token(
                self.token_source.as_ref(),
            )
            .map_err(ErasureError)?;
            match self
                .client
                .post(&self.endpoint)
                .bearer_auth(bearer_token.as_ref())
                .json(&command)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json::<Result<usize, String>>()
                        .await
                        .map_err(|error| ErasureError(format!("decode erasure response: {error}")))?
                        .map_err(ErasureError);
                }
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    let rotated = awaken_service_auth_contract::service_bearer_token_rotated(
                        self.token_source.as_ref(),
                        bearer_token.as_ref(),
                    )
                    .map_err(ErasureError)?;
                    if attempt + 1 < ATTEMPTS && rotated {
                        continue;
                    }
                    return Err(ErasureError(
                        "Coordinator erasure credentials were rejected".into(),
                    ));
                }
                Ok(response) if response.status().is_client_error() => {
                    return Err(ErasureError(format!(
                        "Coordinator erasure command rejected: {}",
                        response.status()
                    )));
                }
                Ok(response) => {
                    last_error =
                        format!("Coordinator erasure service returned {}", response.status())
                }
                Err(error) => last_error = format!("call Coordinator erasure service: {error}"),
            }
            if attempt + 1 < ATTEMPTS {
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
        Err(ErasureError(last_error))
    }
}

#[async_trait]
impl ContentEraser for HttpCoordinatorContentEraser {
    async fn erase_subject(&self, subject: &DataSubjectId) -> Result<usize, ErasureError> {
        self.send(subject).await
    }
}

#[cfg(test)]
mod tests {
    use awaken_captured_content_store::InMemoryCapturedContentStore;
    use awaken_run_executor_acp::{FsSessionBlobStore, SessionBlobStore, SessionHomeKey};
    use axum::body::Body;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn private_erasure_boundary_authenticates_fans_out_and_fences() {
        // Cause/effect decision table: R1 valid token + captured content + ACP
        // blob -> erase both and return two; R2 retry -> replay the same durable
        // receipt; R3 wrong token -> reject before mutation; R4 non-HTTP,
        // query-bearing, or empty-token client configuration -> fail before a
        // request. The Coordinator factory is the only fanout owner.
        let store = Arc::new(InMemoryCapturedContentStore::new());
        store.insert(
            DataSubjectId("dsub_a".into()),
            awaken_runtime_contract::Purpose::TelemetryContent,
            "content",
            1,
        );
        let blob_root = tempfile::tempdir().unwrap();
        let blobs = FsSessionBlobStore::new(blob_root.path().to_path_buf());
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("session"), b"opaque").unwrap();
        blobs
            .store(
                &SessionHomeKey {
                    data_subject_id: Some("dsub_a".into()),
                    thread_id: "thread".into(),
                    adapter: "codex".into(),
                },
                source.path(),
            )
            .await
            .unwrap();
        let eraser =
            coordinator_content_eraser(store.clone(), Some(blob_root.path().to_path_buf()));
        let app = router(eraser, "secret").unwrap();
        let request = |token: &'static str| {
            Request::post(ERASE_COORDINATOR_CONTENT_PATH)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"subject":"dsub_a"}"#))
                .unwrap()
        };

        let rejected = app.clone().oneshot(request("wrong")).await.unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED, "R3");
        assert_eq!(store.len(), 1, "R3");

        let erased = app.clone().oneshot(request("secret")).await.unwrap();
        assert_eq!(erased.status(), StatusCode::OK, "R1");
        let receipt: Result<usize, String> =
            serde_json::from_slice(&to_bytes(erased.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(receipt.unwrap(), 2, "R1");
        assert!(store.is_empty(), "R1");

        let replay = app.oneshot(request("secret")).await.unwrap();
        assert_eq!(replay.status(), StatusCode::OK, "R2");
        let receipt: Result<usize, String> =
            serde_json::from_slice(&to_bytes(replay.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(receipt.unwrap(), 2, "R2");

        assert!(
            HttpCoordinatorContentEraser::new("ftp://coordinator", "secret").is_err(),
            "R4"
        );
        assert!(
            HttpCoordinatorContentEraser::new("http://coordinator?scope=x", "secret").is_err(),
            "R4"
        );
        assert!(
            HttpCoordinatorContentEraser::new("http://coordinator", " ").is_err(),
            "R4"
        );
    }
}
