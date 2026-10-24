// @formal entrypoint remote_worker.stale_epoch_never_mutates_truth
//! Coordinator HTTP adapter for claim-fenced Session artifact publication.

use std::sync::Arc;

use awaken_dispatch_contract::{DispatchCommitAuthority, RunClaim};
use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
    ArtifactRecovery, FileApplicationService, MAX_MANAGED_FILE_SIZE_BYTES, ResourcePurgeError,
};
use awaken_session_realization_contract::ResourceOperationFence;
use awaken_session_state_contract::{SessionEnvironmentOperation, SessionTerminalCleanupEffect};
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
    SessionWorkerEffectTemporalRule, clock_ms, verify_session_worker_effect,
};

pub const ARTIFACT_PUBLICATION_PATH: &str = "/v1/worker/resources/files/artifacts";
pub const ARTIFACT_CAPTURE_PATH: &str = "/v1/worker/resources/files/artifacts/capture";
pub const ARTIFACT_RECOVERY_PATH: &str = "/v1/worker/resources/files/artifacts/recovery";
pub const ARTIFACT_METADATA_HEADER: &str = "x-awaken-artifact-publication";

/// Signed HTTP metadata; bytes remain in the request body to avoid base64
/// expansion of large artifacts.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactPublicationRequest {
    /// The sole authoring vocabulary for every current writer.
    pub operation_fence: ResourceOperationFence,
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
    operation_fence: Option<ResourceOperationFence>,
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
            (None, Some(claim)) => ResourceOperationFence::Run(claim),
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

/// One authenticated transport envelope for reading/replacing a complete
/// output observation. This carries only File references, never output bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCaptureRequest {
    pub operation_fence: ResourceOperationFence,
    pub identity: Option<WorkerIdentity>,
    pub operation: ArtifactCaptureOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactCaptureOperation {
    Read {
        workspace_id: String,
        session_id: String,
    },
    Replace {
        request: awaken_resource_contract::ReplaceArtifactCapture,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "result",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum ArtifactCaptureResponse {
    Current(Option<awaken_resource_contract::ArtifactCapture>),
    Replaced(awaken_resource_contract::ReplaceArtifactCaptureOutcome),
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

    async fn exchange_capture(
        &self,
        operation: ArtifactCaptureOperation,
        fence: Option<&ResourceOperationFence>,
    ) -> Result<ArtifactCaptureResponse, ArtifactPublicationError> {
        let fence = fence.ok_or_else(|| ArtifactPublicationError::InvalidRequest {
            detail: "remote artifact capture requires an execution fence".into(),
        })?;
        let payload = ArtifactCaptureRequest {
            operation_fence: fence.clone(),
            identity: self.upstream.worker_identity().cloned(),
            operation,
        };
        let request = self
            .upstream
            .http_client()
            .post(format!(
                "{}{ARTIFACT_CAPTURE_PATH}",
                self.upstream.base_url()
            ))
            .json(&payload);
        let request = self
            .upstream
            .authorize_request("POST", ARTIFACT_CAPTURE_PATH, request)
            .map_err(|detail| ArtifactPublicationError::Denied { detail })?;
        let response = request
            .send()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        if response.status() != http::StatusCode::OK {
            return Err(remote_failure(
                response.status(),
                response.bytes().await.ok().as_deref(),
            ));
        }
        response
            .json()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))
    }
}

#[async_trait::async_trait]
impl ArtifactPublisher<ResourceOperationFence> for HttpArtifactPublisher {
    async fn capture(
        &self,
        workspace_id: &str,
        session_id: &str,
        fence: Option<&ResourceOperationFence>,
    ) -> Result<Option<awaken_resource_contract::ArtifactCapture>, ArtifactPublicationError> {
        let response = self
            .exchange_capture(
                ArtifactCaptureOperation::Read {
                    workspace_id: workspace_id.into(),
                    session_id: session_id.into(),
                },
                fence,
            )
            .await?;
        let ArtifactCaptureResponse::Current(capture) = response else {
            return Err(ArtifactPublicationError::ReceiptMismatch {
                detail: "artifact capture authority returned the wrong operation".into(),
            });
        };
        if let Some(capture) = &capture {
            capture
                .verify_scope(workspace_id, session_id)
                .map_err(ArtifactPublicationError::from)?;
        }
        Ok(capture)
    }

    async fn replace_capture(
        &self,
        mut request: awaken_resource_contract::ReplaceArtifactCapture,
        fence: Option<&ResourceOperationFence>,
    ) -> Result<awaken_resource_contract::ReplaceArtifactCaptureOutcome, ArtifactPublicationError>
    {
        request.bind_run_origin(fence.and_then(ResourceOperationFence::run_claim_identity))?;
        request
            .decide(None)
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        let response = self
            .exchange_capture(
                ArtifactCaptureOperation::Replace {
                    request: request.clone(),
                },
                fence,
            )
            .await?;
        let ArtifactCaptureResponse::Replaced(outcome) = response else {
            return Err(ArtifactPublicationError::ReceiptMismatch {
                detail: "artifact capture authority returned the wrong operation".into(),
            });
        };
        request
            .verify_outcome(&outcome)
            .map_err(ArtifactPublicationError::from)?;
        Ok(outcome)
    }

    async fn publish(
        &self,
        publication: ArtifactPublication<ResourceOperationFence>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
        publication.verify()?;
        let operation_fence = match publication.fence.clone() {
            Some(fence @ ResourceOperationFence::Run(_))
                if publication.idempotency_scope.is_none() =>
            {
                fence
            }
            Some(fence @ ResourceOperationFence::CheckpointRelease(_))
                if publication.idempotency_scope.is_none() =>
            {
                fence
            }
            Some(ResourceOperationFence::Terminal(effect))
                if publication.idempotency_scope.as_deref() == Some(effect.operation_id()) =>
            {
                ResourceOperationFence::Terminal(effect)
            }
            Some(_) => {
                return Err(ArtifactPublicationError::InvalidRequest {
                    detail: "artifact idempotency scope does not match its execution fence".into(),
                });
            }
            None => {
                return Err(ArtifactPublicationError::InvalidRequest {
                    detail: "remote artifact publication requires an execution fence".into(),
                });
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
            .header(http::header::CONTENT_TYPE, "application/octet-stream")
            .body(publication.bytes.clone());
        let request = self
            .upstream
            .authorize_request("POST", ARTIFACT_PUBLICATION_PATH, request)
            .map_err(|detail| ArtifactPublicationError::Denied { detail })?;
        let response = request
            .send()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        let status = response.status();
        if status != http::StatusCode::OK {
            return Err(remote_failure(
                response.status(),
                response.bytes().await.ok().as_deref(),
            ));
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
        recovery: ArtifactRecovery<ResourceOperationFence>,
    ) -> Result<Vec<ArtifactPublicationReceipt>, ArtifactPublicationError> {
        recovery.verify()?;
        let ResourceOperationFence::Terminal(effect) = &recovery.fence else {
            return Err(ArtifactPublicationError::InvalidRequest {
                detail: "remote artifact recovery requires a terminal effect".into(),
            });
        };
        if recovery.idempotency_scope != effect.operation_id() {
            return Err(ArtifactPublicationError::InvalidRequest {
                detail: "artifact recovery scope does not match its terminal effect".into(),
            });
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
            .map_err(|detail| ArtifactPublicationError::Denied { detail })?;
        let response = request
            .send()
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        let status = response.status();
        if status != http::StatusCode::OK {
            return Err(remote_failure(
                response.status(),
                response.bytes().await.ok().as_deref(),
            ));
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
    dispatch: Arc<dyn DispatchCommitAuthority>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
    terminal_preparation_control:
        Option<Arc<dyn awaken_session_realization_contract::SessionTerminalPreparationControl>>,
    checkpoint_artifact_control:
        Option<Arc<dyn awaken_session_realization_contract::SessionCheckpointArtifactControl>>,
    /// The Coordinator's wall clock: the instant claims and leases are judged at.
    clock: Arc<dyn awaken_time::WallClock>,
}

impl WorkerArtifactPublicationService {
    #[must_use]
    pub fn new(
        application: Arc<dyn FileApplicationService>,
        dispatch: Arc<dyn DispatchCommitAuthority>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        directory: Arc<dyn WorkerDirectory>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            application,
            dispatch,
            authenticator,
            directory,
            terminal_preparation_control: None,
            checkpoint_artifact_control: None,
            clock,
        }
    }

    #[must_use]
    pub fn with_terminal_preparation_control(
        mut self,
        control: Arc<dyn awaken_session_realization_contract::SessionTerminalPreparationControl>,
    ) -> Self {
        self.terminal_preparation_control = Some(control);
        self
    }

    #[must_use]
    pub fn with_checkpoint_artifact_control(
        mut self,
        control: Arc<dyn awaken_session_realization_contract::SessionCheckpointArtifactControl>,
    ) -> Self {
        self.checkpoint_artifact_control = Some(control);
        self
    }

    /// Install the two independent Session roles used by Artifact publication
    /// from one composition object. The stored trait objects remain separate;
    /// this helper adds no aggregate behavior or fallback path.
    #[must_use]
    pub fn with_session_control<T>(mut self, control: Arc<T>) -> Self
    where
        T: awaken_session_realization_contract::SessionTerminalPreparationControl
            + awaken_session_realization_contract::SessionCheckpointArtifactControl
            + 'static,
    {
        self.terminal_preparation_control = Some(control.clone());
        self.checkpoint_artifact_control = Some(control);
        self
    }
}

pub fn worker_artifact_publication_router(
    service: Arc<WorkerArtifactPublicationService>,
) -> Router {
    Router::new()
        .route(ARTIFACT_PUBLICATION_PATH, post(publish_artifact))
        .route(ARTIFACT_RECOVERY_PATH, post(recover_artifacts))
        .route(ARTIFACT_CAPTURE_PATH, post(capture_artifacts))
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

async fn authorize_session_worker_lease(
    service: &WorkerArtifactPublicationService,
    worker: &VerifiedWorkerContext,
    identity: Option<&WorkerIdentity>,
    lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
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
        clock_ms(service.clock.as_ref()),
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
    result: Result<String, awaken_session_realization_contract::SessionRealizationControlFailure>,
    expected_workspace: &str,
) -> Result<(), StatusCode> {
    match result {
        Ok(authorized_workspace) if authorized_workspace == expected_workspace => Ok(()),
        Ok(_) => Err(StatusCode::FORBIDDEN),
        Err(
            awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(_),
        )
        | Err(awaken_session_realization_contract::SessionRealizationControlFailure::Conflict) => {
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
        SessionWorkerEffectTemporalRule::SessionRoot,
    )
    .await?;
    let Some(control) = service.terminal_preparation_control.as_deref() else {
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
    session_id: &awaken_session_types::SessionId,
) -> Result<(), StatusCode> {
    let Some(lease) = operation.realization() else {
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
    let Some(control) = service.checkpoint_artifact_control.as_deref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    authorize_workspace_result(
        control
            .authorize_checkpoint_release_artifact_effect(session_id, operation)
            .await,
        workspace_id,
    )
}

async fn authorize_artifact_effect(
    service: &WorkerArtifactPublicationService,
    worker: &VerifiedWorkerContext,
    identity: Option<&WorkerIdentity>,
    fence: &ResourceOperationFence,
    workspace_id: &str,
    session_id: &str,
    idempotency_scope: Option<&str>,
) -> Result<Option<awaken_dispatch_contract::CommitEpochGuard>, StatusCode> {
    let mut run_guard = None;
    match fence {
        ResourceOperationFence::SessionMaterialization(_)
        | ResourceOperationFence::RepositoryPublication { .. } => {
            return Err(StatusCode::FORBIDDEN);
        }
        ResourceOperationFence::Run(claim) => {
            if idempotency_scope.is_some() {
                return Err(StatusCode::BAD_REQUEST);
            }
            if verify_claim_owner(
                Some(service.directory.as_ref()),
                worker,
                identity,
                claim,
                clock_ms(service.clock.as_ref()),
            )
            .await
            .is_err()
            {
                return Err(StatusCode::FORBIDDEN);
            }
            let guard = match service.dispatch.lock_commit_epoch(claim).await {
                Ok(Some(guard)) if guard.is_live_at(clock_ms(service.clock.as_ref())) => guard,
                Ok(_) => return Err(StatusCode::CONFLICT),
                Err(_) => return Err(StatusCode::SERVICE_UNAVAILABLE),
            };
            if !guard.matches_execution_scope(workspace_id, session_id) {
                return Err(StatusCode::FORBIDDEN);
            }
            run_guard = Some(guard);
        }
        ResourceOperationFence::CheckpointRelease(operation) => {
            if idempotency_scope.is_some() {
                return Err(StatusCode::BAD_REQUEST);
            }
            authorize_checkpoint_release_artifact(
                service,
                worker,
                identity,
                operation,
                workspace_id,
                &awaken_session_types::SessionId::from(session_id),
            )
            .await?;
        }
        ResourceOperationFence::Terminal(effect) => {
            let Some(idempotency_scope) = idempotency_scope else {
                return Err(StatusCode::BAD_REQUEST);
            };
            authorize_terminal_artifact(
                service,
                worker,
                identity,
                effect,
                workspace_id,
                session_id,
                idempotency_scope,
            )
            .await?;
        }
    }
    Ok(run_guard)
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
    let identity = metadata.identity;
    let publication = ArtifactPublication {
        effect_id: metadata.effect_id,
        workspace_id: metadata.workspace_id,
        session_id: metadata.session_id,
        logical_path: metadata.logical_path,
        mime_type: metadata.mime_type,
        content_id: metadata.content_id,
        bytes: bytes.to_vec(),
        idempotency_scope: metadata.idempotency_scope,
        fence: Some(metadata.operation_fence),
    };
    if publication.verify().is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Cause/effect decision table:
    // C1 exactly one publication-capable Run/checkpoint-release/terminal fence; C2 authenticated current Worker; C3
    // the selected durable authority admits this exact Session/Workspace; C4
    // content/effect identity is exact. R1 !C1|!C2|!C3 => zero File write;
    // R2 Run+C1..C4 holds the commit-epoch guard through publication; R3
    // CheckpointRelease/Terminal+C1..C4 linearize at their exact Session-root
    // authorization reads; R4 authority unavailable => retryable 503, never an
    // unfenced fallback. Only Terminal may carry File association scope.
    let _run_guard = match authorize_artifact_effect(
        service.as_ref(),
        &worker,
        identity.as_ref(),
        publication.fence.as_ref().expect("decoded Worker fence"),
        &publication.workspace_id,
        &publication.session_id,
        publication.idempotency_scope.as_deref(),
    )
    .await
    {
        Ok(guard) => guard,
        Err(status) => return admission_error_response(status),
    };
    let file_publication = publication.into_file_application();
    match service.application.create_artifact(&file_publication).await {
        Ok(record) => {
            match ArtifactPublicationReceipt::from_publication_record(&file_publication, record) {
                Ok(receipt) => (StatusCode::OK, Json(receipt)).into_response(),
                Err(_) => (
                    ArtifactFailure::ReceiptMismatch {}.status(),
                    Json(ArtifactFailure::ReceiptMismatch {}),
                )
                    .into_response(),
            }
        }
        Err(error) => application_error_response(&error),
    }
}

async fn capture_artifacts(
    State(service): State<Arc<WorkerArtifactPublicationService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ArtifactCaptureRequest>,
) -> Response {
    let (workspace, session) = match &request.operation {
        ArtifactCaptureOperation::Read {
            workspace_id,
            session_id,
        } => (workspace_id, session_id),
        ArtifactCaptureOperation::Replace { request } => {
            if request.decide(None).is_err() {
                return StatusCode::BAD_REQUEST.into_response();
            }
            (&request.workspace_id, &request.session_id)
        }
    };
    let scope = match &request.operation_fence {
        ResourceOperationFence::Terminal(effect) => Some(effect.operation_id()),
        _ => None,
    };
    // The exact same guard as per-File publication stays alive through CAS.
    // A capture read is fenced too: an obsolete Worker cannot scan a successor's
    // revision and then attempt to attach its own stale output bytes to it.
    let _guard = match authorize_artifact_effect(
        service.as_ref(),
        &worker,
        request.identity.as_ref(),
        &request.operation_fence,
        workspace,
        session,
        scope,
    )
    .await
    {
        Ok(guard) => guard,
        Err(status) => return admission_error_response(status),
    };
    let request_fence = request.operation_fence.clone();
    match request.operation {
        ArtifactCaptureOperation::Read {
            workspace_id,
            session_id,
        } => {
            match service
                .application
                .artifact_capture(&workspace_id, &session_id)
                .await
            {
                Ok(capture) => {
                    if capture.as_ref().is_some_and(|capture| {
                        capture.verify_scope(&workspace_id, &session_id).is_err()
                    }) {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    Json(ArtifactCaptureResponse::Current(capture)).into_response()
                }
                Err(error) => application_error_response(&error),
            }
        }
        ArtifactCaptureOperation::Replace { mut request } => {
            if let Err(error) = request.bind_run_origin(request_fence.run_claim_identity()) {
                return admission_error_response(match error {
                    ArtifactPublicationError::Denied { .. } => StatusCode::FORBIDDEN,
                    ArtifactPublicationError::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
                    _ => StatusCode::SERVICE_UNAVAILABLE,
                });
            }
            match service
                .application
                .replace_artifact_capture(request.clone())
                .await
            {
                Ok(outcome) if request.verify_outcome(&outcome).is_ok() => {
                    Json(ArtifactCaptureResponse::Replaced(outcome)).into_response()
                }
                Ok(_) => (
                    ArtifactFailure::ReceiptMismatch {}.status(),
                    Json(ArtifactFailure::ReceiptMismatch {}),
                )
                    .into_response(),
                Err(error) => application_error_response(&error),
            }
        }
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
    let recovery = ArtifactRecovery {
        workspace_id: request.workspace_id,
        session_id: request.session_id,
        idempotency_scope: request.idempotency_scope,
        fence: request.terminal_effect,
    };
    if recovery.verify().is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Err(status) = authorize_terminal_artifact(
        service.as_ref(),
        &worker,
        request.identity.as_ref(),
        &recovery.fence,
        &recovery.workspace_id,
        &recovery.session_id,
        &recovery.idempotency_scope,
    )
    .await
    {
        return admission_error_response(status);
    }
    let records = match service
        .application
        .list_including_deleted(&recovery.workspace_id, Some(&recovery.session_id))
        .await
    {
        Ok(records) => records,
        Err(error) => return application_error_response(&error),
    };
    match recovery.receipts_from_records(records) {
        Ok(receipts) => (StatusCode::OK, Json(receipts)).into_response(),
        Err(_) => (
            ArtifactFailure::ReceiptMismatch {}.status(),
            Json(ArtifactFailure::ReceiptMismatch {}),
        )
            .into_response(),
    }
}

// This is the private wire projection of Resources failures, not a second
// error authority. Never transmit backend diagnostics or infer absence from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ArtifactFailure {
    InvalidRequest {},
    Denied {},
    Conflict {},
    CorruptData {},
    ReceiptMismatch {},
    AuthorityUnavailable {},
    Indeterminate {},
}

impl ArtifactFailure {
    fn status(self) -> StatusCode {
        match self {
            Self::InvalidRequest {} => StatusCode::BAD_REQUEST,
            Self::Denied {} => StatusCode::FORBIDDEN,
            Self::Conflict {} => StatusCode::CONFLICT,
            Self::CorruptData {} => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ReceiptMismatch {} => StatusCode::BAD_GATEWAY,
            Self::AuthorityUnavailable {} | Self::Indeterminate {} => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        }
    }
    fn error(self) -> ArtifactPublicationError {
        let detail = format!("artifact authority reported {self:?}");
        match self {
            Self::InvalidRequest {} => ArtifactPublicationError::InvalidRequest { detail },
            Self::Denied {} => ArtifactPublicationError::Denied { detail },
            Self::Conflict {} => ArtifactPublicationError::Conflict { detail },
            Self::CorruptData {} => ArtifactPublicationError::CorruptData { detail },
            Self::ReceiptMismatch {} => ArtifactPublicationError::ReceiptMismatch { detail },
            Self::AuthorityUnavailable {} => {
                ArtifactPublicationError::AuthorityUnavailable { detail }
            }
            Self::Indeterminate {} => ArtifactPublicationError::Indeterminate { detail },
        }
    }
}

fn application_failure(error: &ResourcePurgeError) -> ArtifactFailure {
    use awaken_resource_contract::{FileCatalogError, FileStoreError};
    match error {
        ResourcePurgeError::InvalidIntent { .. }
        | ResourcePurgeError::InvalidClaim { .. }
        | ResourcePurgeError::InvalidReference { .. }
        | ResourcePurgeError::ReceiptMismatch { .. }
        | ResourcePurgeError::LimitExceeded { .. }
        | ResourcePurgeError::Catalog(FileCatalogError::InvalidRecord { .. })
        | ResourcePurgeError::Catalog(FileCatalogError::InvalidCapture { .. }) => {
            ArtifactFailure::InvalidRequest {}
        }
        ResourcePurgeError::IdempotencyConflict(_)
        | ResourcePurgeError::RevisionConflict(_)
        | ResourcePurgeError::Catalog(FileCatalogError::IdentityConflict { .. })
        | ResourcePurgeError::Catalog(FileCatalogError::AssociationConflict { .. })
        | ResourcePurgeError::StaleClaim
        | ResourcePurgeError::StaleReclamationFence
        | ResourcePurgeError::ReclamationFenced { .. } => ArtifactFailure::Conflict {},
        ResourcePurgeError::CorruptData { .. }
        | ResourcePurgeError::Catalog(FileCatalogError::CorruptData { .. })
        | ResourcePurgeError::Blob(FileStoreError::CorruptContent { .. }) => {
            ArtifactFailure::CorruptData {}
        }
        ResourcePurgeError::Catalog(FileCatalogError::AuthorityUnavailable { .. })
        | ResourcePurgeError::Blob(FileStoreError::AuthorityUnavailable { .. }) => {
            ArtifactFailure::AuthorityUnavailable {}
        }
        _ => ArtifactFailure::Indeterminate {},
    }
}

#[cfg(test)]
fn application_error_status(error: &ResourcePurgeError) -> StatusCode {
    application_failure(error).status()
}

fn application_error_response(error: &ResourcePurgeError) -> Response {
    let failure = application_failure(error);
    (failure.status(), Json(failure)).into_response()
}

fn admission_error_response(status: StatusCode) -> Response {
    // This boundary has not invoked the File application. Unlike an arbitrary
    // remote 503, its missing authority is known before the physical effect.
    let failure = match status {
        StatusCode::BAD_REQUEST => ArtifactFailure::InvalidRequest {},
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ArtifactFailure::Denied {},
        StatusCode::CONFLICT => ArtifactFailure::Conflict {},
        StatusCode::SERVICE_UNAVAILABLE => ArtifactFailure::AuthorityUnavailable {},
        _ => ArtifactFailure::Indeterminate {},
    };
    (failure.status(), Json(failure)).into_response()
}

fn remote_failure(status: StatusCode, body: Option<&[u8]>) -> ArtifactPublicationError {
    // A lost body or nonempty malformed/contradictory envelope cannot prove
    // non-execution. Only an actually empty legacy response may use the
    // middleware admission status; decode failure is not an empty body.
    let Some(body) = body else {
        return ArtifactFailure::Indeterminate {}.error();
    };
    if !body.is_empty() {
        return match serde_json::from_slice::<ArtifactFailure>(body) {
            Ok(failure) if failure.status() == status => failure.error(),
            _ => ArtifactFailure::Indeterminate {}.error(),
        };
    }
    // Admission refusals also originate in authentication/fence middleware,
    // which returns bare statuses before calling the Resources application.
    match status {
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE => {
            ArtifactFailure::InvalidRequest {}.error()
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ArtifactFailure::Denied {}.error(),
        StatusCode::CONFLICT => ArtifactFailure::Conflict {}.error(),
        _ => ArtifactFailure::Indeterminate {}.error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn error_projection_preserves_decisions_without_backend_diagnostics() {
        use awaken_resource_contract::FileCatalogError;
        // Cause/effect table: R1 invalid => refusal; R2 stale => conflict;
        // R3 corrupt stored data => corrupt, never absent; R4 unknown commit
        // => indeterminate; R5 missing authority => unavailable. Each row uses
        // the real server response encoder and HTTP client decoder. Changing
        // diagnostic text cannot change the decision or expose it on the wire.
        for (error, expected) in [
            (
                ResourcePurgeError::InvalidIntent {
                    detail: "private validation".into(),
                },
                ArtifactFailure::InvalidRequest {},
            ),
            (ResourcePurgeError::StaleClaim, ArtifactFailure::Conflict {}),
            (
                ResourcePurgeError::Catalog(FileCatalogError::CorruptData {
                    detail: "private SQL".into(),
                }),
                ArtifactFailure::CorruptData {},
            ),
            (
                ResourcePurgeError::Catalog(FileCatalogError::Indeterminate {
                    detail: "private host".into(),
                }),
                ArtifactFailure::Indeterminate {},
            ),
            (
                ResourcePurgeError::Catalog(FileCatalogError::AuthorityUnavailable {
                    detail: "private topology".into(),
                }),
                ArtifactFailure::AuthorityUnavailable {},
            ),
        ] {
            let router = Router::new().route(
                "/",
                post(move || {
                    let error = error.clone();
                    async move { application_error_response(&error) }
                }),
            );
            let address = awaken_dispatch_testkit::worker_http::serve(router).await;
            let response = reqwest::Client::new()
                .post(format!("http://{address}/"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), expected.status());
            assert_eq!(
                remote_failure(response.status(), response.bytes().await.ok().as_deref()),
                expected.error()
            );
            let encoded = serde_json::to_string(&expected).unwrap();
            assert!(!encoded.contains("private"));
        }
    }

    #[tokio::test]
    async fn contradictory_or_unreadable_failure_does_not_prove_non_execution() {
        // R6 typed refusal with a success/mismatched envelope => unknown;
        // R7 unreadable server error => unknown; R8 bare middleware denial =>
        // denied; R9 bare stale fence => conflict; R10 post-effect bad receipt
        // => ReceiptMismatch, never a claim that the write was rejected.
        // R11 a nonempty malformed/extended 400 envelope is not the legacy
        // empty admission response and therefore remains indeterminate.
        // R12 response-body transport failure never becomes a bare denial.
        assert_eq!(
            remote_failure(StatusCode::FORBIDDEN, None),
            ArtifactFailure::Indeterminate {}.error()
        );
        for (status, body, expected) in [
            (
                StatusCode::BAD_REQUEST,
                "not-json",
                ArtifactFailure::Indeterminate {},
            ),
            (
                StatusCode::BAD_REQUEST,
                r#"{"kind":"invalid_request","committed":true}"#,
                ArtifactFailure::Indeterminate {},
            ),
            (
                StatusCode::BAD_GATEWAY,
                r#"{"kind":"receipt_mismatch"}"#,
                ArtifactFailure::ReceiptMismatch {},
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"kind":"invalid_request"}"#,
                ArtifactFailure::Indeterminate {},
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "not-json",
                ArtifactFailure::Indeterminate {},
            ),
            (StatusCode::FORBIDDEN, "", ArtifactFailure::Denied {}),
            (StatusCode::CONFLICT, "", ArtifactFailure::Conflict {}),
        ] {
            let router = Router::new().route("/", post(move || async move { (status, body) }));
            let address = awaken_dispatch_testkit::worker_http::serve(router).await;
            let response = reqwest::Client::new()
                .post(format!("http://{address}/"))
                .send()
                .await
                .unwrap();
            assert_eq!(
                remote_failure(response.status(), response.bytes().await.ok().as_deref()),
                expected.error()
            );
        }
    }

    use awaken_dispatch_contract::RunClaim;
    use awaken_worker_contract::WorkerIdentity;

    fn metadata(path: &str) -> ArtifactPublicationRequest {
        let claim = RunClaim {
            run_id: awaken_agent_contract::agent::run::Id("run".into()),
            owner: "worker:incarnation:1".into(),
            epoch: 1,
        };
        ArtifactPublicationRequest {
            operation_fence: ResourceOperationFence::Run(claim),
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
    fn application_errors_fail_closed_by_cause_class() {
        use awaken_resource_contract::FileCatalogError;
        // Adapter decision table: caller intent/claim/reference/receipt/limit
        // -> 400; identity/association/concurrency -> 409; unknown or
        // incomplete effect facts -> 503. Publication shape is checked only by
        // ArtifactPublication::verify; local/HTTP parity has a transport test.
        let bad_request = [
            ResourcePurgeError::InvalidIntent { detail: "x".into() },
            ResourcePurgeError::InvalidClaim { detail: "x".into() },
            ResourcePurgeError::InvalidReference { detail: "x".into() },
            ResourcePurgeError::ReceiptMismatch { detail: "x".into() },
            ResourcePurgeError::LimitExceeded { detail: "x".into() },
        ];
        for error in bad_request {
            assert_eq!(application_error_status(&error), StatusCode::BAD_REQUEST);
        }
        for error in [
            ResourcePurgeError::RevisionConflict("file".into()),
            ResourcePurgeError::Catalog(FileCatalogError::IdentityConflict { detail: "x".into() }),
            ResourcePurgeError::Catalog(FileCatalogError::AssociationConflict {
                detail: "x".into(),
            }),
        ] {
            assert_eq!(application_error_status(&error), StatusCode::CONFLICT);
        }
        for error in [
            ResourcePurgeError::Indeterminate {
                detail: "offline".into(),
            },
            ResourcePurgeError::ArithmeticExhausted { detail: "x".into() },
            ResourcePurgeError::AdmissionIncomplete { detail: "x".into() },
        ] {
            assert_eq!(
                application_error_status(&error),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
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
        let ResourceOperationFence::Run(claim) = canonical.clone() else {
            unreachable!()
        };
        legacy_object.insert("claim".into(), serde_json::to_value(claim).unwrap());
        let legacy: ArtifactPublicationRequest =
            serde_json::from_value(legacy).expect("W2 legacy decode");
        assert_eq!(
            awaken_agent_contract::stable_fingerprint(&legacy.operation_fence),
            awaken_agent_contract::stable_fingerprint(&canonical),
            "W2"
        );

        let mut ambiguous = encoded.clone();
        ambiguous.as_object_mut().expect("request object").insert(
            "claim".into(),
            serde_json::to_value(match canonical.clone() {
                ResourceOperationFence::Run(claim) => claim,
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
                awaken_session_state_contract::SessionCleanupCommand::new(
                    &awaken_session_types::SessionId::from("session"),
                    "session",
                    "cleanup",
                ),
                awaken_session_configuration_contract::resource::SessionRealizationLease {
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
