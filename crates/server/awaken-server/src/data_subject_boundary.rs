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
    bearer_token: Arc<str>,
}

pub fn router(
    eraser: Arc<dyn ContentEraser>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    let bearer_token = bearer_token.into();
    if bearer_token.trim().is_empty() {
        return Err("Coordinator content-erasure bearer token must not be empty".into());
    }
    Ok(Router::new()
        .route(ERASE_COORDINATOR_CONTENT_PATH, post(handle_erase))
        .with_state(BoundaryState {
            eraser,
            bearer_token: Arc::from(bearer_token),
        }))
}

async fn handle_erase(
    State(state): State<BoundaryState>,
    headers: HeaderMap,
    Json(command): Json<EraseCommand>,
) -> axum::response::Response {
    if !awaken_executable_agent_contract::service_bearer_token_matches(
        headers
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes()),
        &state.bearer_token,
    ) {
        return StatusCode::UNAUTHORIZED.into_response();
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
    bearer_token: String,
    client: reqwest::Client,
}

impl HttpCoordinatorContentEraser {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, String> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let bearer_token = bearer_token.into();
        reqwest::Url::parse(&base_url)
            .map_err(|error| format!("invalid Coordinator erasure URL: {error}"))?;
        if bearer_token.trim().is_empty() {
            return Err("Coordinator erasure bearer token is required".into());
        }
        Ok(Self {
            endpoint: format!("{base_url}{ERASE_COORDINATOR_CONTENT_PATH}"),
            bearer_token,
            client: reqwest::Client::new(),
        })
    }

    async fn send(&self, subject: &DataSubjectId) -> Result<usize, ErasureError> {
        let command = EraseCommand {
            subject: subject.clone(),
        };
        let mut last_error = "Coordinator captured-content service unavailable".to_owned();
        for attempt in 0..ATTEMPTS {
            match self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.bearer_token)
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
        // receipt; R3 wrong token -> reject before mutation. The Coordinator
        // factory is the only fanout owner.
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
    }
}
