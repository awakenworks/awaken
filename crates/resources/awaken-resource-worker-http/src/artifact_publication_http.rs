//! Coordinator HTTP adapter for claim-fenced Session artifact publication.

use std::sync::Arc;

use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublisher, FileApplicationService,
    MAX_MANAGED_FILE_SIZE_BYTES, ResourcePurgeError, content_id, harvest_idempotency_key,
};
use awaken_run_ingress_contract::{DispatchQueue, RunClaim};
use awaken_worker_contract::{WorkerDirectory, WorkerIdentity};
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, WorkerUpstream, authenticate_worker_request,
    verify_claim_owner,
};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub const ARTIFACT_PUBLICATION_PATH: &str = "/v1/worker/resources/files/artifacts";
pub const ARTIFACT_METADATA_HEADER: &str = "x-awaken-artifact-publication";

/// Signed HTTP metadata; bytes remain in the request body to avoid base64
/// expansion of large artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPublicationRequest {
    pub claim: RunClaim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<WorkerIdentity>,
    pub workspace_id: String,
    pub session_id: String,
    pub logical_path: String,
    pub mime_type: String,
    pub content_id: String,
}

/// Worker-side peer of the claim-fenced artifact HTTP endpoint.
#[derive(Clone)]
pub struct HttpArtifactPublisher {
    upstream: WorkerUpstream,
}

impl HttpArtifactPublisher {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self { upstream }
    }
}

#[async_trait::async_trait]
impl ArtifactPublisher<RunClaim> for HttpArtifactPublisher {
    async fn publish(
        &self,
        publication: ArtifactPublication<RunClaim>,
    ) -> Result<awaken_resource_contract::FileRecord, ArtifactPublicationError> {
        let claim = publication.fence.ok_or_else(|| {
            ArtifactPublicationError::new("remote artifact publication requires a dispatch claim")
        })?;
        let metadata = ArtifactPublicationRequest {
            claim,
            identity: self.upstream.worker_identity().cloned(),
            workspace_id: publication.workspace_id,
            session_id: publication.session_id,
            logical_path: publication.logical_path,
            mime_type: publication.mime_type,
            content_id: content_id(&publication.bytes),
        };
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&metadata)
                .map_err(|error| ArtifactPublicationError::new(error.to_string()))?,
        );
        let request = self
            .upstream
            .http_client()
            .post(format!(
                "{}{ARTIFACT_PUBLICATION_PATH}",
                self.upstream.base_url()
            ))
            .header(ARTIFACT_METADATA_HEADER, encoded)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(publication.bytes);
        let request = self
            .upstream
            .authorize_request("POST", ARTIFACT_PUBLICATION_PATH, request)
            .map_err(ArtifactPublicationError::new)?;
        let response = request
            .send()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            return Err(ArtifactPublicationError::new(format!(
                "artifact authority returned HTTP {status}"
            )));
        }
        response
            .json::<awaken_resource_contract::FileRecord>()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))
    }
}

pub struct WorkerArtifactPublicationService {
    application: Arc<dyn FileApplicationService>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
}

impl WorkerArtifactPublicationService {
    #[must_use]
    pub fn new(
        application: Arc<dyn FileApplicationService>,
        dispatch: Arc<dyn DispatchQueue>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        directory: Arc<dyn WorkerDirectory>,
    ) -> Self {
        Self {
            application,
            dispatch,
            authenticator,
            directory,
        }
    }
}

pub fn worker_artifact_publication_router(
    service: Arc<WorkerArtifactPublicationService>,
) -> Router {
    Router::new()
        .route(ARTIFACT_PUBLICATION_PATH, post(publish_artifact))
        .layer(DefaultBodyLimit::max(
            usize::try_from(MAX_MANAGED_FILE_SIZE_BYTES).unwrap_or(usize::MAX),
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            service.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(service)
}

fn decode_metadata(headers: &HeaderMap) -> Result<ArtifactPublicationRequest, ()> {
    let encoded = headers
        .get(ARTIFACT_METADATA_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or(())?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn valid_metadata(metadata: &ArtifactPublicationRequest) -> bool {
    !metadata.workspace_id.trim().is_empty()
        && !metadata.session_id.trim().is_empty()
        && !metadata.logical_path.trim().is_empty()
        && metadata.logical_path.len() <= 1024
        && !metadata.logical_path.starts_with('/')
        && metadata
            .logical_path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
        && !metadata.mime_type.trim().is_empty()
        && metadata.mime_type.len() <= 255
        && !metadata.content_id.trim().is_empty()
}

async fn publish_artifact(
    State(service): State<Arc<WorkerArtifactPublicationService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let Ok(metadata) = decode_metadata(&headers) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !valid_metadata(&metadata)
        || verify_claim_owner(
            Some(service.directory.as_ref()),
            &worker,
            metadata.identity.as_ref(),
            &metadata.claim,
            unix_now_ms(),
        )
        .await
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if content_id(&bytes) != metadata.content_id {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let guard = match service.dispatch.lock_commit_epoch(&metadata.claim).await {
        Ok(Some(guard)) if guard.is_live_at(unix_now_ms()) => guard,
        Ok(_) => return StatusCode::CONFLICT.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let dispatch = guard.request();
    let scope_matches = dispatch
        .execution_scope
        .as_ref()
        .is_some_and(|scope| scope.0.0 == metadata.workspace_id);
    let session_matches = dispatch.session_thread_id().0 == metadata.session_id;
    if !scope_matches || !session_matches {
        return StatusCode::FORBIDDEN.into_response();
    }
    let key = harvest_idempotency_key(
        &metadata.session_id,
        &metadata.logical_path,
        &metadata.content_id,
    );
    match service
        .application
        .create_artifact(
            &metadata.workspace_id,
            &metadata.session_id,
            metadata.logical_path,
            metadata.mime_type,
            &bytes,
            key,
        )
        .await
    {
        Ok(record) => (StatusCode::OK, Json(record)).into_response(),
        Err(error) => application_error_status(&error).into_response(),
    }
}

fn application_error_status(error: &ResourcePurgeError) -> StatusCode {
    match error {
        ResourcePurgeError::Invalid(_) => StatusCode::BAD_REQUEST,
        ResourcePurgeError::IdempotencyConflict(_) | ResourcePurgeError::RevisionConflict(_) => {
            StatusCode::CONFLICT
        }
        _ => StatusCode::SERVICE_UNAVAILABLE,
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
    use awaken_run_ingress_contract::RunClaim;
    use awaken_worker_contract::WorkerIdentity;

    fn metadata(path: &str) -> ArtifactPublicationRequest {
        ArtifactPublicationRequest {
            claim: RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("run".into()),
                owner: "worker:incarnation:1".into(),
                epoch: 1,
            },
            identity: Some(WorkerIdentity::new("worker", "incarnation", 1)),
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            logical_path: path.into(),
            mime_type: "text/plain".into(),
            content_id: "digest".into(),
        }
    }

    #[test]
    fn metadata_and_application_errors_fail_closed_by_cause_class() {
        // Adapter-validation FMECA/cause-effect decision table. Causes: C1 all
        // bounded relative metadata is present; C2 authority/path field is empty,
        // absolute, dot-segmented, or oversized; C3 application rejects caller
        // input; C4 application detects a concurrency conflict; C5 dependency
        // state is unavailable/unknown. Effects: E1 admit to claim checks; E2 403
        // before application; E3 400 non-retryable; E4 409 retry/reconcile; E5
        // 503 without false success. Rules V1 C1=>E1; V2 C2=>E2; V3 C3=>E3;
        // V4 C4=>E4; V5 C5=>E5.
        assert!(valid_metadata(&metadata("reports/result.txt")), "V1");
        for invalid in [
            "",
            "/absolute",
            ".",
            "..",
            "a/./b",
            "a/../b",
            "a//b",
            &"x".repeat(1025),
        ] {
            assert!(!valid_metadata(&metadata(invalid)), "V2: {invalid:?}");
        }
        let mut missing_scope = metadata("result.txt");
        missing_scope.workspace_id.clear();
        assert!(!valid_metadata(&missing_scope), "V2 workspace");
        let mut missing_session = metadata("result.txt");
        missing_session.session_id.clear();
        assert!(!valid_metadata(&missing_session), "V2 session");
        let mut missing_digest = metadata("result.txt");
        missing_digest.content_id.clear();
        assert!(!valid_metadata(&missing_digest), "V2 digest");

        assert_eq!(
            application_error_status(&ResourcePurgeError::Invalid("quota".into())),
            StatusCode::BAD_REQUEST,
            "V3"
        );
        assert_eq!(
            application_error_status(&ResourcePurgeError::RevisionConflict("file".into())),
            StatusCode::CONFLICT,
            "V4"
        );
        assert_eq!(
            application_error_status(&ResourcePurgeError::Storage("offline".into())),
            StatusCode::SERVICE_UNAVAILABLE,
            "V5"
        );
    }

    #[test]
    fn artifact_metadata_wire_is_strict_and_lossless() {
        // Wire FMECA cause/effect table: C1 every authority field is present;
        // C2 a version-skewed/attacker-controlled field is present. Effects: E1
        // lossless claim/scope/digest transport; E2 fail closed before authority
        // checks. Rules W1 C1+!C2=>E1; W2 C1+C2=>E2.
        let request = metadata("reports/result.txt");
        let encoded = serde_json::to_value(&request).expect("W1 encode");
        let decoded: ArtifactPublicationRequest =
            serde_json::from_value(encoded.clone()).expect("W1 decode");
        assert_eq!(decoded.claim, request.claim, "W1");
        assert_eq!(decoded.logical_path, request.logical_path, "W1");
        assert_eq!(decoded.content_id, request.content_id, "W1");

        let mut unknown = encoded;
        unknown
            .as_object_mut()
            .expect("request object")
            .insert("authority_bypass".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<ArtifactPublicationRequest>(unknown).is_err(),
            "W2"
        );
    }
}
