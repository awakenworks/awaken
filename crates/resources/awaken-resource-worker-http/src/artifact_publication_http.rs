//! Coordinator HTTP adapter for claim-fenced Session artifact publication.

use std::sync::Arc;

use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
    ArtifactRecovery, FileApplicationService, MAX_MANAGED_FILE_SIZE_BYTES, ResourcePurgeError,
    content_id,
};
use awaken_run_ingress_contract::{ArtifactPublicationFence, DispatchQueue, RunClaim};
use awaken_session_contract::{
    SessionEnvironmentOperation, SessionRealizationControl, SessionTerminalCleanupEffect,
};
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

use crate::worker_authority::{
    SessionWorkerEffectTemporalRule, unix_now_ms, verify_session_worker_effect,
};

pub const ARTIFACT_PUBLICATION_PATH: &str = "/v1/worker/resources/files/artifacts";
pub const ARTIFACT_RECOVERY_PATH: &str = "/v1/worker/resources/files/artifacts/recovery";
pub const ARTIFACT_METADATA_HEADER: &str = "x-awaken-artifact-publication";

/// Signed HTTP metadata; bytes remain in the request body to avoid base64
/// expansion of large artifacts.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactPublicationRequest {
    /// The sole authoring vocabulary for every current writer.
    pub operation_fence: ArtifactPublicationFence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<WorkerIdentity>,
    pub workspace_id: String,
    pub session_id: String,
    pub logical_path: String,
    pub mime_type: String,
    pub content_id: String,
    pub effect_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_scope: Option<String>,
}

/// Private compatibility DTO for the prior Run-only JSON spelling. It cannot
/// be constructed as an application request, and custom decoding immediately
/// collapses it into the canonical non-optional fence above.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactPublicationRequestWire {
    #[serde(default)]
    operation_fence: Option<ArtifactPublicationFence>,
    #[serde(default)]
    claim: Option<RunClaim>,
    #[serde(default)]
    identity: Option<WorkerIdentity>,
    workspace_id: String,
    session_id: String,
    logical_path: String,
    mime_type: String,
    content_id: String,
    effect_id: String,
    #[serde(default)]
    idempotency_scope: Option<String>,
}

impl<'de> Deserialize<'de> for ArtifactPublicationRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ArtifactPublicationRequestWire::deserialize(deserializer)?;
        let operation_fence = match (wire.operation_fence, wire.claim) {
            (Some(fence), None) => fence,
            (None, Some(claim)) => ArtifactPublicationFence::Run(claim),
            (Some(_), Some(_)) => {
                return Err(serde::de::Error::custom(
                    "Artifact publication contains ambiguous operation fences",
                ));
            }
            (None, None) => {
                return Err(serde::de::Error::custom(
                    "Artifact publication has no operation fence",
                ));
            }
        };
        Ok(Self {
            operation_fence,
            identity: wire.identity,
            workspace_id: wire.workspace_id,
            session_id: wire.session_id,
            logical_path: wire.logical_path,
            mime_type: wire.mime_type,
            content_id: wire.content_id,
            effect_id: wire.effect_id,
            idempotency_scope: wire.idempotency_scope,
        })
    }
}

/// Terminal-only durable File receipt readback. It carries the existing root
/// effect unchanged; the endpoint adds no recovery registry or cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecoveryRequest {
    pub terminal_effect: SessionTerminalCleanupEffect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<WorkerIdentity>,
    pub workspace_id: String,
    pub session_id: String,
    pub idempotency_scope: String,
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
impl ArtifactPublisher<ArtifactPublicationFence> for HttpArtifactPublisher {
    async fn publish(
        &self,
        publication: ArtifactPublication<ArtifactPublicationFence>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
        publication.verify()?;
        let operation_fence = match publication.fence.clone() {
            Some(fence @ ArtifactPublicationFence::Run(_))
                if publication.idempotency_scope.is_none() =>
            {
                fence
            }
            Some(fence @ ArtifactPublicationFence::CheckpointRelease(_))
                if publication.idempotency_scope.is_none() =>
            {
                fence
            }
            Some(ArtifactPublicationFence::Terminal(effect))
                if publication.idempotency_scope.as_deref() == Some(effect.operation_id()) =>
            {
                ArtifactPublicationFence::Terminal(effect)
            }
            Some(_) => {
                return Err(ArtifactPublicationError::new(
                    "artifact idempotency scope does not match its execution fence",
                ));
            }
            None => {
                return Err(ArtifactPublicationError::new(
                    "remote artifact publication requires an execution fence",
                ));
            }
        };
        let metadata = ArtifactPublicationRequest {
            operation_fence,
            identity: self.upstream.worker_identity().cloned(),
            workspace_id: publication.workspace_id.clone(),
            session_id: publication.session_id.clone(),
            logical_path: publication.logical_path.clone(),
            mime_type: publication.mime_type.clone(),
            content_id: publication.content_id.clone(),
            effect_id: publication.effect_id.clone(),
            idempotency_scope: publication.idempotency_scope.clone(),
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
            .body(publication.bytes.clone());
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
        let receipt = response
            .json::<ArtifactPublicationReceipt>()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        receipt.verify(&publication)?;
        Ok(receipt)
    }

    async fn recover(
        &self,
        recovery: ArtifactRecovery<ArtifactPublicationFence>,
    ) -> Result<Vec<ArtifactPublicationReceipt>, ArtifactPublicationError> {
        recovery.verify()?;
        let ArtifactPublicationFence::Terminal(effect) = &recovery.fence else {
            return Err(ArtifactPublicationError::new(
                "remote artifact recovery requires a terminal effect",
            ));
        };
        if recovery.idempotency_scope != effect.operation_id() {
            return Err(ArtifactPublicationError::new(
                "artifact recovery scope does not match its terminal effect",
            ));
        }
        let request = ArtifactRecoveryRequest {
            terminal_effect: effect.clone(),
            identity: self.upstream.worker_identity().cloned(),
            workspace_id: recovery.workspace_id.clone(),
            session_id: recovery.session_id.clone(),
            idempotency_scope: recovery.idempotency_scope.clone(),
        };
        let request = self
            .upstream
            .http_client()
            .post(format!(
                "{}{ARTIFACT_RECOVERY_PATH}",
                self.upstream.base_url()
            ))
            .json(&request);
        let request = self
            .upstream
            .authorize_request("POST", ARTIFACT_RECOVERY_PATH, request)
            .map_err(ArtifactPublicationError::new)?;
        let response = request
            .send()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            return Err(ArtifactPublicationError::new(format!(
                "artifact recovery authority returned HTTP {status}"
            )));
        }
        let receipts = response
            .json::<Vec<ArtifactPublicationReceipt>>()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        recovery.verify_receipts(&receipts)?;
        Ok(receipts)
    }
}

pub struct WorkerArtifactPublicationService {
    application: Arc<dyn FileApplicationService>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
    session_control: Option<Arc<dyn SessionRealizationControl>>,
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
            session_control: None,
        }
    }

    #[must_use]
    pub fn with_session_control(
        mut self,
        session_control: Arc<dyn SessionRealizationControl>,
    ) -> Self {
        self.session_control = Some(session_control);
        self
    }
}

pub fn worker_artifact_publication_router(
    service: Arc<WorkerArtifactPublicationService>,
) -> Router {
    Router::new()
        .route(ARTIFACT_PUBLICATION_PATH, post(publish_artifact))
        .route(ARTIFACT_RECOVERY_PATH, post(recover_artifacts))
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
        && !metadata.effect_id.trim().is_empty()
        && metadata
            .idempotency_scope
            .as_deref()
            .is_none_or(|scope| !scope.trim().is_empty())
}

async fn authorize_session_worker_lease(
    service: &WorkerArtifactPublicationService,
    worker: &VerifiedWorkerContext,
    identity: Option<&WorkerIdentity>,
    lease: &awaken_session_contract::SessionRealizationLease,
    temporal_rule: SessionWorkerEffectTemporalRule,
) -> Result<(), StatusCode> {
    let Some(identity) = identity else {
        return Err(StatusCode::FORBIDDEN);
    };
    if let Some(status) = verify_session_worker_effect(
        service.directory.as_ref(),
        worker,
        identity,
        lease,
        unix_now_ms(),
        temporal_rule,
    )
    .await
    .rejection_status()
    {
        return Err(status);
    }
    Ok(())
}

fn authorize_workspace_result(
    result: Result<String, awaken_session_contract::SessionRealizationControlFailure>,
    expected_workspace: &str,
) -> Result<(), StatusCode> {
    match result {
        Ok(authorized_workspace) if authorized_workspace == expected_workspace => Ok(()),
        Ok(_) => Err(StatusCode::FORBIDDEN),
        Err(awaken_session_contract::SessionRealizationControlFailure::Unavailable(_))
        | Err(awaken_session_contract::SessionRealizationControlFailure::Conflict) => {
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(_) => Err(StatusCode::CONFLICT),
    }
}

async fn authorize_terminal_artifact(
    service: &WorkerArtifactPublicationService,
    worker: &VerifiedWorkerContext,
    identity: Option<&WorkerIdentity>,
    effect: &SessionTerminalCleanupEffect,
    workspace_id: &str,
    session_id: &str,
    idempotency_scope: &str,
) -> Result<(), StatusCode> {
    if idempotency_scope != effect.operation_id() || effect.command.thread_id != session_id {
        return Err(StatusCode::FORBIDDEN);
    }
    authorize_session_worker_lease(
        service,
        worker,
        identity,
        &effect.lease,
        SessionWorkerEffectTemporalRule::TerminalGeneration,
    )
    .await?;
    let Some(control) = service.session_control.as_deref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    authorize_workspace_result(
        control
            .authorize_terminal_cleanup_effect(effect)
            .await
            .and_then(|authorization| {
                authorization.verify_for(effect)?;
                Ok(authorization.workspace_id().to_string())
            }),
        workspace_id,
    )
}

async fn authorize_checkpoint_release_artifact(
    service: &WorkerArtifactPublicationService,
    worker: &VerifiedWorkerContext,
    identity: Option<&WorkerIdentity>,
    operation: &SessionEnvironmentOperation,
    workspace_id: &str,
    session_id: &str,
) -> Result<(), StatusCode> {
    let Some(lease) = operation.realization.as_ref() else {
        return Err(StatusCode::FORBIDDEN);
    };
    authorize_session_worker_lease(
        service,
        worker,
        identity,
        lease,
        SessionWorkerEffectTemporalRule::AssertedLeaseMustBeLive,
    )
    .await?;
    let Some(control) = service.session_control.as_deref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    authorize_workspace_result(
        control
            .authorize_checkpoint_release_artifact_effect(session_id, operation)
            .await,
        workspace_id,
    )
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
    if !valid_metadata(&metadata) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let operation_fence = metadata.operation_fence.clone();
    if content_id(&bytes) != metadata.content_id {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Cause/effect decision table:
    // C1 exactly one Run/checkpoint-release/terminal fence; C2 authenticated current Worker; C3
    // the selected durable authority admits this exact Session/Workspace; C4
    // content/effect identity is exact. R1 !C1|!C2|!C3 => zero File write;
    // R2 Run+C1..C4 holds the commit-epoch guard through publication; R3
    // CheckpointRelease/Terminal+C1..C4 linearize at their exact Session-root
    // authorization reads; R4 authority unavailable => retryable 503, never an
    // unfenced fallback. Only Terminal may carry File association scope.
    let mut run_guard = None;
    match &operation_fence {
        ArtifactPublicationFence::Run(claim) => {
            if metadata.idempotency_scope.is_some() {
                return StatusCode::BAD_REQUEST.into_response();
            }
            if verify_claim_owner(
                Some(service.directory.as_ref()),
                &worker,
                metadata.identity.as_ref(),
                claim,
                unix_now_ms(),
            )
            .await
            .is_err()
            {
                return StatusCode::FORBIDDEN.into_response();
            }
            let guard = match service.dispatch.lock_commit_epoch(claim).await {
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
            run_guard = Some(guard);
        }
        ArtifactPublicationFence::CheckpointRelease(operation) => {
            if metadata.idempotency_scope.is_some() {
                return StatusCode::BAD_REQUEST.into_response();
            }
            if let Err(status) = authorize_checkpoint_release_artifact(
                service.as_ref(),
                &worker,
                metadata.identity.as_ref(),
                operation,
                &metadata.workspace_id,
                &metadata.session_id,
            )
            .await
            {
                return status.into_response();
            }
        }
        ArtifactPublicationFence::Terminal(effect) => {
            let Some(idempotency_scope) = metadata.idempotency_scope.as_deref() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            if let Err(status) = authorize_terminal_artifact(
                service.as_ref(),
                &worker,
                metadata.identity.as_ref(),
                effect,
                &metadata.workspace_id,
                &metadata.session_id,
                idempotency_scope,
            )
            .await
            {
                return status.into_response();
            }
        }
    }
    let publication = ArtifactPublication {
        effect_id: metadata.effect_id,
        workspace_id: metadata.workspace_id,
        session_id: metadata.session_id,
        logical_path: metadata.logical_path,
        mime_type: metadata.mime_type,
        content_id: metadata.content_id,
        bytes: bytes.to_vec(),
        idempotency_scope: metadata.idempotency_scope,
        fence: Some(operation_fence),
    };
    if publication.verify().is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let _run_guard = run_guard;
    let file_publication = publication.into_file_application();
    match service.application.create_artifact(&file_publication).await {
        Ok(record) => {
            match ArtifactPublicationReceipt::from_publication_record(&file_publication, record) {
                Ok(receipt) => (StatusCode::OK, Json(receipt)).into_response(),
                Err(_) => StatusCode::CONFLICT.into_response(),
            }
        }
        Err(error) => application_error_status(&error).into_response(),
    }
}

async fn recover_artifacts(
    State(service): State<Arc<WorkerArtifactPublicationService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ArtifactRecoveryRequest>,
) -> Response {
    // Recovery cause/effect table: R1 exact current Worker + live terminal
    // effect + exact Workspace/Session/scope => one File list and canonical
    // receipt projection; R2 any stale/foreign/mismatched authority => zero
    // File reads and writes; R3 File authority unavailable => retryable 503.
    if request.workspace_id.trim().is_empty()
        || request.session_id.trim().is_empty()
        || request.idempotency_scope.trim().is_empty()
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(status) = authorize_terminal_artifact(
        service.as_ref(),
        &worker,
        request.identity.as_ref(),
        &request.terminal_effect,
        &request.workspace_id,
        &request.session_id,
        &request.idempotency_scope,
    )
    .await
    {
        return status.into_response();
    }
    let recovery = ArtifactRecovery {
        workspace_id: request.workspace_id,
        session_id: request.session_id,
        idempotency_scope: request.idempotency_scope,
        fence: ArtifactPublicationFence::Terminal(request.terminal_effect),
    };
    let records = match service
        .application
        .list_including_deleted(&recovery.workspace_id, Some(&recovery.session_id))
        .await
    {
        Ok(records) => records,
        Err(error) => return application_error_status(&error).into_response(),
    };
    match recovery.receipts_from_records(records) {
        Ok(receipts) => (StatusCode::OK, Json(receipts)).into_response(),
        Err(_) => StatusCode::CONFLICT.into_response(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_run_ingress_contract::RunClaim;
    use awaken_worker_contract::WorkerIdentity;

    fn metadata(path: &str) -> ArtifactPublicationRequest {
        let claim = RunClaim {
            run_id: awaken_agent_contract::agent::run::Id("run".into()),
            owner: "worker:incarnation:1".into(),
            epoch: 1,
        };
        ArtifactPublicationRequest {
            operation_fence: ArtifactPublicationFence::Run(claim),
            identity: Some(WorkerIdentity::new("worker", "incarnation", 1)),
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            logical_path: path.into(),
            mime_type: "text/plain".into(),
            content_id: "digest".into(),
            effect_id: awaken_resource_contract::harvest_idempotency_key("session", path, "digest"),
            idempotency_scope: None,
        }
    }

    #[test]
    fn metadata_and_application_errors_fail_closed_by_cause_class() {
        // Adapter-validation FMECA/cause-effect decision table. Causes: C1 all
        // bounded relative metadata is present; C2 authority/path field is empty,
        // absolute, dot-segmented, or oversized; C3 application rejects caller
        // input; C4 application detects a concurrency conflict; C5 dependency
        // state is unavailable/unknown. Effects: E1 admit to fence checks; E2 400
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
        // Wire FMECA/cause-effect decision table:
        // | Rule | canonical fence | legacy claim | unknown | Effect |
        // | W1 | Run/Terminal/CheckpointRelease | absent | no | one lossless authority |
        // | W2 | absent | Run | no | normalize to the same Run semantics/fingerprint |
        // | W3 | present | present | no | reject ambiguity before any effect |
        // | W4 | absent | absent | no | reject missing authority before any effect |
        // | W5 | any | any | yes | strict decode rejection |
        // New writers emit only `operation_fence`; `claim` is decode-only for a
        // server-first rolling upgrade and can never become a second authority.
        let request = metadata("reports/result.txt");
        let encoded = serde_json::to_value(&request).expect("W1 encode");
        assert!(encoded.get("operation_fence").is_some(), "W1 canonical");
        assert!(encoded.get("claim").is_none(), "W1 legacy is decode-only");
        let decoded: ArtifactPublicationRequest =
            serde_json::from_value(encoded.clone()).expect("W1 decode");
        let canonical = decoded.operation_fence.clone();
        assert_eq!(canonical, request.operation_fence, "W1");
        assert_eq!(decoded.logical_path, request.logical_path, "W1");
        assert_eq!(decoded.content_id, request.content_id, "W1");

        let mut legacy = encoded.clone();
        let legacy_object = legacy.as_object_mut().expect("request object");
        legacy_object.remove("operation_fence");
        let ArtifactPublicationFence::Run(claim) = canonical.clone() else {
            unreachable!()
        };
        legacy_object.insert("claim".into(), serde_json::to_value(claim).unwrap());
        let legacy: ArtifactPublicationRequest =
            serde_json::from_value(legacy).expect("W2 legacy decode");
        assert_eq!(
            awaken_session_contract::stable_fingerprint(&legacy.operation_fence),
            awaken_session_contract::stable_fingerprint(&canonical),
            "W2"
        );

        let mut ambiguous = encoded.clone();
        ambiguous.as_object_mut().expect("request object").insert(
            "claim".into(),
            serde_json::to_value(match canonical.clone() {
                ArtifactPublicationFence::Run(claim) => claim,
                _ => unreachable!(),
            })
            .unwrap(),
        );
        assert!(
            serde_json::from_value::<ArtifactPublicationRequest>(ambiguous).is_err(),
            "W3"
        );

        let mut missing = encoded.clone();
        missing
            .as_object_mut()
            .expect("request object")
            .remove("operation_fence");
        assert!(
            serde_json::from_value::<ArtifactPublicationRequest>(missing).is_err(),
            "W4"
        );

        let mut unknown = encoded;
        unknown
            .as_object_mut()
            .expect("request object")
            .insert("authority_bypass".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<ArtifactPublicationRequest>(unknown).is_err(),
            "W5"
        );
    }

    #[test]
    fn artifact_recovery_wire_requires_exact_terminal_authority_fields() {
        // Recovery wire cause/effect table: R1 every terminal effect,
        // Workspace, Session, and scope field present => lossless decode; R2 a
        // required authority field is absent or an unknown bypass field exists
        // => reject before the Session/File authorities are called.
        let request = ArtifactRecoveryRequest {
            terminal_effect: SessionTerminalCleanupEffect::new(
                awaken_session_contract::SessionCleanupCommand::new(
                    "session", "session", "cleanup",
                ),
                awaken_session_contract::SessionRealizationLease {
                    owner: "worker".into(),
                    runtime_incarnation: "worker:incarnation".into(),
                    epoch: 1,
                    expires_at_unix_ms: u64::MAX,
                },
            ),
            identity: Some(WorkerIdentity::new("worker", "incarnation", 1)),
            workspace_id: "workspace".into(),
            session_id: "session".into(),
            idempotency_scope: "cleanup".into(),
        };
        let encoded = serde_json::to_value(&request).expect("R1 encode");
        let decoded: ArtifactRecoveryRequest =
            serde_json::from_value(encoded.clone()).expect("R1 decode");
        assert_eq!(decoded.workspace_id, request.workspace_id, "R1");
        assert_eq!(decoded.terminal_effect, request.terminal_effect, "R1");

        let mut missing = encoded.clone();
        missing
            .as_object_mut()
            .expect("request object")
            .remove("idempotency_scope");
        assert!(
            serde_json::from_value::<ArtifactRecoveryRequest>(missing).is_err(),
            "R2 missing authority"
        );
        let mut unknown = encoded;
        unknown
            .as_object_mut()
            .expect("request object")
            .insert("authority_bypass".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<ArtifactRecoveryRequest>(unknown).is_err(),
            "R2 unknown authority"
        );
    }
}
