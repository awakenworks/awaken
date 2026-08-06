//! Coordinator HTTP adapter for claim-fenced immutable File reads.

use std::sync::Arc;

use awaken_resource_contract::{FileContentSource, FileContentSourceError, content_id};
use awaken_run_ingress_contract::{DispatchQueue, RunClaim};
use awaken_worker_contract::{WorkerDirectory, WorkerIdentity};
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, WorkerUpstream, authenticate_worker_request,
    verify_claim_owner,
};
use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

const FILE_CONTENT_PATH: &str = "/v1/worker/resources/files/content";
const FILE_CONTENT_DIGEST_HEADER: &str = "x-awaken-file-content-digest";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContentRequest {
    claim: RunClaim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<WorkerIdentity>,
    workspace_id: String,
    file_id: String,
}

pub struct WorkerFileContentService {
    source: Arc<dyn FileContentSource<RunClaim>>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Option<Arc<dyn WorkerDirectory>>,
}

impl WorkerFileContentService {
    #[must_use]
    pub fn new(
        source: Arc<dyn FileContentSource<RunClaim>>,
        dispatch: Arc<dyn DispatchQueue>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            source,
            dispatch,
            authenticator,
            directory: None,
        }
    }

    #[must_use]
    pub fn with_worker_directory(mut self, directory: Arc<dyn WorkerDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }
}

/// Registered-Worker client for claim-bound immutable File content.
#[derive(Clone)]
pub struct HttpFileContentSource {
    upstream: WorkerUpstream,
}

impl HttpFileContentSource {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self { upstream }
    }
}

#[async_trait::async_trait]
impl FileContentSource<RunClaim> for HttpFileContentSource {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        claim: Option<&RunClaim>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError> {
        let claim = claim.ok_or_else(|| {
            FileContentSourceError::new("remote File materialization requires a dispatch claim")
        })?;
        if workspace_id.trim().is_empty() || file_id.trim().is_empty() {
            return Err(FileContentSourceError::new(
                "Workspace and File identities must not be empty",
            ));
        }
        let request = self
            .upstream
            .http_client()
            .post(format!("{}{FILE_CONTENT_PATH}", self.upstream.base_url()))
            .json(&FileContentRequest {
                claim: claim.clone(),
                identity: self.upstream.worker_identity().cloned(),
                workspace_id: workspace_id.to_owned(),
                file_id: file_id.to_owned(),
            });
        let request = self
            .upstream
            .authorize_request("POST", FILE_CONTENT_PATH, request)
            .map_err(FileContentSourceError::new)?;
        let response = request
            .send()
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(FileContentSourceError::new(format!(
                "File content authority returned HTTP {}",
                response.status()
            )));
        }
        let digest = response
            .headers()
            .get(FILE_CONTENT_DIGEST_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| FileContentSourceError::new("File response has no content digest"))?
            .to_string();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?
            .to_vec();
        let actual = content_id(&bytes);
        if actual != digest {
            return Err(FileContentSourceError::new(format!(
                "File response digest mismatch: expected {digest}, received {actual}"
            )));
        }
        Ok(Some((digest, bytes)))
    }
}

pub fn worker_file_content_router(service: Arc<WorkerFileContentService>) -> Router {
    Router::new()
        .route(FILE_CONTENT_PATH, post(read_file_content))
        .route_layer(axum::middleware::from_fn_with_state(
            service.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(service)
}

async fn read_file_content(
    State(service): State<Arc<WorkerFileContentService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<FileContentRequest>,
) -> Response<Body> {
    if request.workspace_id.trim().is_empty()
        || request.file_id.trim().is_empty()
        || verify_claim_owner(
            service.directory.as_deref(),
            &worker,
            request.identity.as_ref(),
            &request.claim,
            unix_now_ms(),
        )
        .await
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let guard = match service.dispatch.lock_commit_epoch(&request.claim).await {
        Ok(Some(guard)) if guard.is_live_at(unix_now_ms()) => guard,
        Ok(_) => return StatusCode::CONFLICT.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let dispatch = guard.request();
    let scope_matches = dispatch
        .execution_scope
        .as_ref()
        .is_some_and(|scope| scope.0.0 == request.workspace_id);
    let manifest = dispatch
        .session_resources
        .as_ref()
        .filter(|envelope| envelope.workspace_id == request.workspace_id)
        .and_then(|envelope| envelope.decode_manifest().ok());
    let file_is_frozen = manifest.as_ref().is_some_and(|manifest| {
        manifest.resources.inputs.iter().any(|input| {
            matches!(
                &input.source,
                awaken_session_contract::ResolvedInputSource::File { file_id }
                    if file_id.as_str() == request.file_id
            )
        })
    });
    if !scope_matches || !file_is_frozen {
        return StatusCode::FORBIDDEN.into_response();
    }
    match service
        .source
        .read(&request.workspace_id, &request.file_id, None)
        .await
    {
        Ok(Some((digest, bytes))) => Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
            .header(FILE_CONTENT_DIGEST_HEADER, digest)
            .body(Body::from(bytes))
            .expect("static File content response is valid"),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_content_wire_rejects_unknown_authority_fields() {
        // Wire cause/effect decision table: F1 exact claim, identity, workspace,
        // and File id => lossless request; F2 any unknown field => reject before
        // claim/resource checks. Rules W1 F1+!F2=>decode; W2 F1+F2=>fail closed.
        let request = FileContentRequest {
            claim: RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("run-file".into()),
                owner: "worker-file".into(),
                epoch: 4,
            },
            identity: None,
            workspace_id: "workspace".into(),
            file_id: "file".into(),
        };
        let encoded = serde_json::to_value(&request).expect("W1 encode");
        let decoded: FileContentRequest =
            serde_json::from_value(encoded.clone()).expect("W1 decode");
        assert_eq!(decoded.claim, request.claim, "W1 claim");
        assert_eq!(decoded.file_id, request.file_id, "W1 file");

        let mut unknown = encoded;
        unknown
            .as_object_mut()
            .expect("request object")
            .insert("authority_bypass".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<FileContentRequest>(unknown).is_err(),
            "W2"
        );
    }
}
