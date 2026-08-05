//! Coordinator HTTP adapter for claim-fenced immutable File reads.

use std::sync::Arc;

use awaken_run_ingress::{DispatchQueue, WorkerDirectory};
use awaken_run_ingress_contract::{
    FILE_CONTENT_DIGEST_HEADER, FILE_CONTENT_PATH, FileContentRequest, FileContentSource,
};
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, authenticate_worker_request,
    verify_claim_owner,
};
use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};

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

    #[must_use]
    pub fn with_worker_directory(mut self, directory: Arc<dyn WorkerDirectory>) -> Self {
        self.directory = Some(directory);
        self
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
