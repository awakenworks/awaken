//! Coordinator HTTP adapter for claim-fenced immutable File reads.

use std::sync::Arc;

use awaken_agent_contract::agent::content::{ContentBlock, DocumentSource, ImageSource};
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_resource_contract::{
    FileContentSource, FileContentSourceError, FileReadPurpose, ResolvedFileContent, content_id,
};
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
const FILE_CONTENT_METADATA_HEADER: &str = "x-awaken-file-content-metadata";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContentRequest {
    claim: RunClaim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<WorkerIdentity>,
    workspace_id: String,
    file_id: String,
    purpose: FileReadPurpose,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContentMetadata {
    file_id: String,
    content_id: String,
    filename: String,
    media_type: String,
}

pub struct WorkerFileContentService {
    source: Arc<dyn FileContentSource<RunClaim>>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Option<Arc<dyn WorkerDirectory>>,
    session_repository: Option<Arc<dyn awaken_session_contract::ManagedSessionRepository>>,
    recovery: Option<Arc<dyn RunRecoverySource>>,
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
            session_repository: None,
            recovery: None,
        }
    }

    #[must_use]
    pub fn with_worker_directory(mut self, directory: Arc<dyn WorkerDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Install the durable Session authority used to validate a claimed frozen
    /// Resource generation. The live claim and exact realization lease still
    /// fence every read.
    #[must_use]
    pub fn with_session_repository(
        mut self,
        sessions: Arc<dyn awaken_session_contract::ManagedSessionRepository>,
    ) -> Self {
        self.session_repository = Some(sessions);
        self
    }

    #[must_use]
    pub fn with_recovery(mut self, recovery: Arc<dyn RunRecoverySource>) -> Self {
        self.recovery = Some(recovery);
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
        purpose: &FileReadPurpose,
        claim: Option<&RunClaim>,
    ) -> Result<Option<ResolvedFileContent>, FileContentSourceError> {
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
                purpose: purpose.clone(),
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
        use base64::Engine as _;
        let metadata = response
            .headers()
            .get(FILE_CONTENT_METADATA_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| FileContentSourceError::new("File response has no content metadata"))?;
        let metadata = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(metadata)
            .map_err(|error| {
                FileContentSourceError::new(format!("invalid File metadata: {error}"))
            })?;
        let metadata: FileContentMetadata = serde_json::from_slice(&metadata).map_err(|error| {
            FileContentSourceError::new(format!("invalid File metadata: {error}"))
        })?;
        if metadata.file_id != file_id
            || metadata.content_id.trim().is_empty()
            || metadata.media_type.trim().is_empty()
        {
            return Err(FileContentSourceError::new(
                "File response metadata does not match the requested File",
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?
            .to_vec();
        let actual = content_id(&bytes);
        if actual != metadata.content_id {
            return Err(FileContentSourceError::new(format!(
                "File response digest mismatch: expected {}, received {actual}",
                metadata.content_id
            )));
        }
        Ok(Some(ResolvedFileContent {
            file_id: metadata.file_id,
            content_id: metadata.content_id,
            filename: metadata.filename,
            media_type: metadata.media_type,
            bytes,
        }))
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
        manifest.resources.inputs().iter().any(|input| {
            matches!(
                &input.source,
                awaken_session_contract::ResolvedInputSource::File { file_id }
                    if file_id.as_str() == request.file_id
            )
        })
    });
    let file_is_authorized = match &request.purpose {
        FileReadPurpose::SessionResource => {
            if file_is_frozen {
                true
            } else {
                application_session_file_is_frozen(&service, dispatch, &request, unix_now_ms())
                    .await
            }
        }
        FileReadPurpose::ModelContent { thread_id } => {
            model_content_file_is_authorized(&service, dispatch, thread_id, &request.file_id).await
        }
    };
    if !scope_matches || !file_is_authorized {
        return StatusCode::FORBIDDEN.into_response();
    }
    match service
        .source
        .read(
            &request.workspace_id,
            &request.file_id,
            &request.purpose,
            None,
        )
        .await
    {
        Ok(Some(resolved)) => {
            use base64::Engine as _;
            let metadata = FileContentMetadata {
                file_id: resolved.file_id,
                content_id: resolved.content_id,
                filename: resolved.filename,
                media_type: resolved.media_type,
            };
            let metadata = serde_json::to_vec(&metadata)
                .map(|value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value));
            match metadata {
                Ok(metadata) => Response::builder()
                    .status(StatusCode::OK)
                    .header(FILE_CONTENT_METADATA_HEADER, metadata)
                    .body(Body::from(resolved.bytes))
                    .expect("validated File content response is valid"),
                Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn model_content_file_is_authorized(
    service: &WorkerFileContentService,
    dispatch: &awaken_run_ingress_contract::RunDispatch,
    thread_id: &str,
    file_id: &str,
) -> bool {
    if dispatch.activation.thread_id.0 != thread_id {
        return false;
    }
    if messages_reference_file(&dispatch.activation.input, file_id) {
        return true;
    }
    let Some(recovery) = &service.recovery else {
        return false;
    };
    recovery
        .recovery_snapshot_in_session(
            dispatch.session_thread_id(),
            dispatch.thread_id(),
            dispatch.run_id(),
        )
        .await
        .is_ok_and(|snapshot| messages_reference_file(&snapshot.messages, file_id))
}

fn messages_reference_file(messages: &[Message], file_id: &str) -> bool {
    messages
        .iter()
        .any(|message| blocks_reference_file(&message.content, file_id))
}

fn blocks_reference_file(blocks: &[ContentBlock], file_id: &str) -> bool {
    blocks.iter().any(|block| match block {
        ContentBlock::Image {
            source: ImageSource::File { file_id: candidate },
        }
        | ContentBlock::Document {
            source: DocumentSource::File { file_id: candidate },
            ..
        } => candidate == file_id,
        ContentBlock::ToolResult { content, .. } => blocks_reference_file(content, file_id),
        ContentBlock::Text { .. }
        | ContentBlock::Image { .. }
        | ContentBlock::Document { .. }
        | ContentBlock::SearchResult { .. }
        | ContentBlock::ToolReference { .. }
        | ContentBlock::Redacted
        | ContentBlock::ToolUse { .. }
        | ContentBlock::Thinking { .. } => false,
    })
}

async fn application_session_file_is_frozen(
    service: &WorkerFileContentService,
    dispatch: &awaken_run_ingress_contract::RunDispatch,
    request: &FileContentRequest,
    now_ms: u64,
) -> bool {
    let (Some(sessions), Some(session_id), Some(identity)) = (
        service.session_repository.as_ref(),
        dispatch.session_thread_id.as_ref(),
        request.identity.as_ref(),
    ) else {
        return false;
    };
    let Ok(owner_scope) = sessions.owner(&session_id.0).await else {
        return false;
    };
    if owner_scope != request.workspace_id {
        return false;
    }
    let Ok(session) = sessions.get(&session_id.0).await else {
        return false;
    };
    let lease_matches = session.realization.as_ref().is_some_and(|lease| {
        lease.owner == identity.worker_id
            && lease.runtime_incarnation == identity.lease_owner()
            && awaken_session_contract::realization_lease_is_live_at(
                lease.expires_at_unix_ms,
                now_ms,
            )
    });
    session.frozen_baseline().is_some()
        && !session.is_terminal()
        && lease_matches
        && session.resources.desired().inputs().iter().any(|input| {
            matches!(
                &input.source,
                awaken_session_contract::ResolvedInputSource::File { file_id }
                    if file_id.as_str() == request.file_id
            )
        })
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
            purpose: FileReadPurpose::SessionResource,
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
