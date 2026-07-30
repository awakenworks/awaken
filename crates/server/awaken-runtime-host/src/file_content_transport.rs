//! Exact immutable File materialization across the Resource-to-Worker boundary.
//!
//! The Session manifest retains the public Workspace-scoped `FileId`. This
//! boundary resolves that identity through the authoritative File catalog and
//! content store while the requesting Worker's exact dispatch claim is live.
//! It exposes no authoring, listing, deletion, or mutable write-back operation.

use std::sync::Arc;

use awaken_file_store::FileStore;
use awaken_resource_contract::FileCatalog;
use awaken_run_ingress::{DispatchQueue, RunClaim, WorkerDirectory, WorkerIdentity};
use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::worker_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, WorkerUpstream, authenticate_worker_request,
};

const FILE_CONTENT_PATH: &str = "/v1/worker/resources/files/content";
const FILE_CONTENT_DIGEST_HEADER: &str = "x-awaken-file-content-digest";

/// Failure at the exact immutable File read boundary.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("file content source: {0}")]
pub struct FileContentSourceError(String);

impl FileContentSourceError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Resolve one public Workspace File identity to its immutable digest and bytes.
///
/// A remote implementation requires `claim`; the local store adapter deliberately
/// ignores it because no process trust boundary is crossed. This remains one
/// per-kind port rather than a generic Resource materializer.
#[async_trait::async_trait]
pub trait FileContentSource: Send + Sync {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        claim: Option<&RunClaim>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError>;
}

/// Local adapter over the authoritative logical catalog and immutable byte store.
pub struct StoreFileContentSource {
    catalog: Arc<dyn FileCatalog>,
    store: Arc<dyn FileStore>,
}

impl StoreFileContentSource {
    #[must_use]
    pub fn new(catalog: Arc<dyn FileCatalog>, store: Arc<dyn FileStore>) -> Self {
        Self { catalog, store }
    }
}

#[async_trait::async_trait]
impl FileContentSource for StoreFileContentSource {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        _claim: Option<&RunClaim>,
    ) -> Result<Option<(String, Vec<u8>)>, FileContentSourceError> {
        let Some(record) = self
            .catalog
            .get_file(workspace_id, file_id, false)
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?
        else {
            return Ok(None);
        };
        let bytes = self
            .store
            .get(&record.blob_id)
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?
            .ok_or_else(|| {
                FileContentSourceError::new(format!(
                    "File `{file_id}` references missing content `{}`",
                    record.blob_id
                ))
            })?;
        let actual = awaken_file_store::content_id(&bytes);
        if actual != record.blob_id {
            return Err(FileContentSourceError::new(format!(
                "File `{file_id}` content digest mismatch: expected {}, received {actual}",
                record.blob_id
            )));
        }
        Ok(Some((record.blob_id, bytes)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContentRequest {
    claim: RunClaim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<WorkerIdentity>,
    workspace_id: String,
    file_id: String,
}

/// File data-plane handler dependencies for exact claim-fenced reads.
pub struct WorkerFileContentService {
    source: Arc<dyn FileContentSource>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Option<Arc<dyn WorkerDirectory>>,
}

impl WorkerFileContentService {
    #[must_use]
    pub fn new(
        source: Arc<dyn FileContentSource>,
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

    /// Require the current Coordinator registration in addition to transport
    /// authentication. Registered production composition always installs this.
    #[must_use]
    pub fn with_worker_directory(mut self, directory: Arc<dyn WorkerDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }
}

/// Mount only the immutable File read boundary used by registered Workers.
pub fn worker_file_content_router(service: Arc<WorkerFileContentService>) -> Router {
    Router::new()
        .route(FILE_CONTENT_PATH, post(read_file_content))
        .route_layer(axum::middleware::from_fn_with_state(
            service.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(service)
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

async fn authenticated_claim_owner(
    service: &WorkerFileContentService,
    worker: &VerifiedWorkerContext,
    identity: Option<&WorkerIdentity>,
    claim: &RunClaim,
) -> bool {
    if let Some(directory) = &service.directory {
        let Some(identity) = identity else {
            return false;
        };
        return crate::worker_security::verify_current_worker_identity(
            directory.as_ref(),
            worker,
            identity,
            unix_now_ms(),
            false,
        )
        .await
        .is_ok()
            && claim.owner == identity.lease_owner();
    }
    match identity {
        Some(identity) => {
            crate::worker_security::verify_worker_identity(worker, identity).is_ok()
                && claim.owner == identity.lease_owner()
        }
        None => claim.owner == worker.worker_id(),
    }
}

async fn read_file_content(
    State(service): State<Arc<WorkerFileContentService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<FileContentRequest>,
) -> Response<Body> {
    if request.workspace_id.trim().is_empty()
        || request.file_id.trim().is_empty()
        || !authenticated_claim_owner(
            service.as_ref(),
            &worker,
            request.identity.as_ref(),
            &request.claim,
        )
        .await
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
        .and_then(|envelope| crate::provisioning::decode_session_resource_envelope(envelope).ok());
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

/// Registered-Worker adapter for one exact immutable File content source.
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
impl FileContentSource for HttpFileContentSource {
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
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != StatusCode::OK {
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
        let actual = awaken_file_store::content_id(&bytes);
        if actual != digest {
            return Err(FileContentSourceError::new(format!(
                "File response digest mismatch: expected {digest}, received {actual}"
            )));
        }
        Ok(Some((digest, bytes)))
    }
}
