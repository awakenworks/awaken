// @formal entrypoint session_resources.external_io_requires_durable_prepared_state
//! Coordinator HTTP adapter for claim-fenced Repository binding validation.

use std::sync::Arc;

use awaken_resource_contract::{
    ConfigVersion, LiveResourceBindingVerifier, RepositoryBindingVerifier,
    RepositoryBindingVerifierError, RepositoryTransport,
};
use awaken_dispatch_contract::{DispatchQueue, ResourceOperationFence, RunClaim};
use awaken_worker_contract::{WorkerDirectory, WorkerIdentity};
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, WorkerUpstream, authenticate_worker_request,
    verify_claim_owner,
};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::worker_authority::{
    SessionWorkerEffectTemporalRule, unix_now_ms, verify_session_worker_effect,
};

const REPOSITORY_BINDING_PATH: &str = "/v1/worker/resources/repositories/verify";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryBindingRequest {
    authority: ResourceOperationFence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<WorkerIdentity>,
    workspace_id: String,
    repository_id: String,
    config_version: ConfigVersion,
}

pub struct WorkerRepositoryBindingService {
    validator: Arc<dyn LiveResourceBindingVerifier>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Option<Arc<dyn WorkerDirectory>>,
    materialization_control:
        Option<Arc<dyn awaken_session_realization_contract::SessionResourceMaterializationControl>>,
    publication_control: Option<
        Arc<dyn awaken_session_realization_contract::SessionTerminalRepositoryPublicationControl>,
    >,
    transport_authorizer: Option<Arc<dyn RepositoryTransportAuthorizer>>,
}

/// Exact frozen facts supplied to the deployment adapter after the common
/// Worker identity, claim, Session manifest, and Resource checks have passed.
#[derive(Debug, Clone)]
pub struct RepositoryTransportAuthorization {
    pub worker: WorkerIdentity,
    pub session_id: String,
    pub workspace_id: String,
    pub input: awaken_session_resource_contract::ResolvedInput,
    pub authority: RepositoryTransportAuthority,
}

/// Closed authorization fence for the one deployment-selected Repository
/// transport. Run execution retains its exact dispatch claim. Terminal
/// publication instead carries the aggregate-derived command and its current
/// realization lease; neither variant can be silently interpreted as the other.
#[derive(Debug, Clone)]
pub enum RepositoryTransportAuthority {
    Run {
        claim: RunClaim,
        claim_expires_ms: u64,
    },
    SessionMaterialization {
        intent: Box<awaken_session_realization_contract::SessionResourceMaterializationIntent>,
    },
    TerminalPublication {
        command: Box<awaken_session_state_contract::SessionRepositoryPublicationCommand>,
        lease: awaken_session_resource_contract::SessionRealizationLease,
    },
}

/// Deployment adapter for the last credential hop. Self-hosted compositions
/// omit it and retain `Direct`; Cloud installs Gateway mediation.
#[async_trait::async_trait]
pub trait RepositoryTransportAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        request: RepositoryTransportAuthorization,
    ) -> Result<RepositoryTransport, RepositoryBindingVerifierError>;
}

/// Temporal scope applied to an issuer-owned Repository capability. Ordinary
/// Run work cannot outlive its dispatch claim. Terminal publication is already
/// one exact aggregate-authorized operation, so its trusted issuer may mint a
/// one-shot capability long enough to finish after the short Worker heartbeat
/// lease, but must report that capability's live expiry. The Session generation
/// is revalidated before and after issuance and the Host revalidates it again
/// after Git I/O.
#[derive(Clone, Copy)]
enum RepositoryTransportExpiryRule {
    BoundToAuthority(u64),
    TrustedTerminalOperation,
}

/// Validate additive issuer expiry evidence without changing legacy Run
/// host-operation semantics. `None` remains compatible only there; the new
/// terminal Gateway entry requires an external issuer to return `Some(live)`.
/// Long-lived workload consumers separately require `Some` at their boundary.
fn transport_expiry_is_admitted(
    transport: &RepositoryTransport,
    now_unix_ms: u64,
    rule: RepositoryTransportExpiryRule,
) -> bool {
    match transport {
        RepositoryTransport::Direct => true,
        RepositoryTransport::GatewayMediated {
            expires_at_unix_ms: None,
            ..
        } => matches!(rule, RepositoryTransportExpiryRule::BoundToAuthority(_)),
        RepositoryTransport::GatewayMediated {
            expires_at_unix_ms: Some(expires_at),
            ..
        } => {
            expires_at.is_live_at(now_unix_ms)
                && match rule {
                    RepositoryTransportExpiryRule::BoundToAuthority(upper_bound) => {
                        expires_at.unix_ms() <= upper_bound
                    }
                    RepositoryTransportExpiryRule::TrustedTerminalOperation => true,
                }
        }
    }
}

fn repository_verifier_status(error: &RepositoryBindingVerifierError) -> StatusCode {
    match error {
        RepositoryBindingVerifierError::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
        RepositoryBindingVerifierError::Denied { .. } => StatusCode::FORBIDDEN,
        RepositoryBindingVerifierError::Conflict { .. } => StatusCode::CONFLICT,
        RepositoryBindingVerifierError::AuthorityUnavailable { .. }
        | RepositoryBindingVerifierError::Indeterminate { .. } => StatusCode::SERVICE_UNAVAILABLE,
        RepositoryBindingVerifierError::CorruptAuthority { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Registered-Worker client for exact Repository binding verification.
#[derive(Clone)]
pub struct HttpRepositoryBindingVerifier {
    upstream: WorkerUpstream,
}

impl HttpRepositoryBindingVerifier {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self { upstream }
    }

    async fn verify_request(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        authority: ResourceOperationFence,
    ) -> Result<RepositoryTransport, RepositoryBindingVerifierError> {
        if workspace_id.trim().is_empty() || repository_id.trim().is_empty() {
            return Err(RepositoryBindingVerifierError::InvalidRequest {
                detail: "Workspace and Repository identities must not be empty".into(),
            });
        }
        let request = self
            .upstream
            .http_client()
            .post(format!(
                "{}{REPOSITORY_BINDING_PATH}",
                self.upstream.base_url()
            ))
            .json(&RepositoryBindingRequest {
                authority,
                identity: self.upstream.worker_identity().cloned(),
                workspace_id: workspace_id.to_owned(),
                repository_id: repository_id.to_owned(),
                config_version,
            });
        let request = self
            .upstream
            .authorize_request("POST", REPOSITORY_BINDING_PATH, request)
            .map_err(|detail| RepositoryBindingVerifierError::AuthorityUnavailable { detail })?;
        let response = request.send().await.map_err(|error| {
            RepositoryBindingVerifierError::AuthorityUnavailable {
                detail: error.to_string(),
            }
        })?;
        if response.status() == http::StatusCode::NO_CONTENT {
            return Ok(RepositoryTransport::Direct);
        }
        if response.status() != http::StatusCode::OK {
            let status = response.status();
            let detail = format!("Repository binding authority returned HTTP {status}");
            return Err(match status {
                http::StatusCode::BAD_REQUEST => {
                    RepositoryBindingVerifierError::InvalidRequest { detail }
                }
                http::StatusCode::UNAUTHORIZED | http::StatusCode::FORBIDDEN => {
                    RepositoryBindingVerifierError::Denied { detail }
                }
                http::StatusCode::CONFLICT => RepositoryBindingVerifierError::Conflict { detail },
                status if status.is_server_error() => {
                    RepositoryBindingVerifierError::AuthorityUnavailable { detail }
                }
                _ => RepositoryBindingVerifierError::Indeterminate { detail },
            });
        }
        response.json::<RepositoryTransport>().await.map_err(|_| {
            RepositoryBindingVerifierError::CorruptAuthority {
                detail: "Repository binding authority returned an invalid transport".into(),
            }
        })
    }
}

#[async_trait::async_trait]
impl RepositoryBindingVerifier<ResourceOperationFence> for HttpRepositoryBindingVerifier {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        authority: Option<&ResourceOperationFence>,
    ) -> Result<RepositoryTransport, RepositoryBindingVerifierError> {
        let authority =
            authority.ok_or_else(|| RepositoryBindingVerifierError::InvalidRequest {
                detail: "remote Repository verification requires an operation fence".into(),
            })?;
        self.verify_request(
            workspace_id,
            repository_id,
            config_version,
            authority.clone(),
        )
        .await
    }
}

/// Source-compatible adapter for callers that still hold a raw Run claim.
#[async_trait::async_trait]
impl RepositoryBindingVerifier<RunClaim> for HttpRepositoryBindingVerifier {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        claim: Option<&RunClaim>,
    ) -> Result<RepositoryTransport, RepositoryBindingVerifierError> {
        let claim = claim.ok_or_else(|| RepositoryBindingVerifierError::InvalidRequest {
            detail: "remote Repository verification requires a dispatch claim".into(),
        })?;
        self.verify_request(
            workspace_id,
            repository_id,
            config_version,
            ResourceOperationFence::Run(claim.clone()),
        )
        .await
    }
}

/// Source-compatible terminal adapter over the same canonical wire fence.
#[async_trait::async_trait]
impl
    RepositoryBindingVerifier<(
        awaken_session_state_contract::SessionRepositoryPublicationCommand,
        awaken_session_resource_contract::SessionRealizationLease,
    )> for HttpRepositoryBindingVerifier
{
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        fence: Option<&(
            awaken_session_state_contract::SessionRepositoryPublicationCommand,
            awaken_session_resource_contract::SessionRealizationLease,
        )>,
    ) -> Result<RepositoryTransport, RepositoryBindingVerifierError> {
        let (command, lease) = fence.ok_or_else(|| {
            RepositoryBindingVerifierError::InvalidRequest {
                detail: "remote terminal Repository verification requires its publication command and realization lease".into(),
            }
        })?;
        self.verify_request(
            workspace_id,
            repository_id,
            config_version,
            ResourceOperationFence::RepositoryPublication {
                command: Box::new(command.clone()),
                lease: lease.clone(),
            },
        )
        .await
    }
}

impl WorkerRepositoryBindingService {
    #[must_use]
    pub fn new(
        validator: Arc<dyn LiveResourceBindingVerifier>,
        dispatch: Arc<dyn DispatchQueue>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            validator,
            dispatch,
            authenticator,
            directory: None,
            materialization_control: None,
            publication_control: None,
            transport_authorizer: None,
        }
    }

    #[must_use]
    pub fn with_worker_directory(mut self, directory: Arc<dyn WorkerDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }

    /// Install another port view of the same Coordinator Session application
    /// already used by terminal cleanup polling. This is read-only command
    /// projection plus exact lease validation, never another publication queue.
    #[must_use]
    pub fn with_materialization_control(
        mut self,
        control: Arc<
            dyn awaken_session_realization_contract::SessionResourceMaterializationControl,
        >,
    ) -> Self {
        self.materialization_control = Some(control);
        self
    }

    #[must_use]
    pub fn with_publication_control(
        mut self,
        control: Arc<
            dyn awaken_session_realization_contract::SessionTerminalRepositoryPublicationControl,
        >,
    ) -> Self {
        self.publication_control = Some(control);
        self
    }

    /// Install the Resource-materialization and terminal-publication roles
    /// supplied by the same canonical Session application.
    #[must_use]
    pub fn with_session_control<T>(mut self, control: Arc<T>) -> Self
    where
        T: awaken_session_realization_contract::SessionResourceMaterializationControl
            + awaken_session_realization_contract::SessionTerminalRepositoryPublicationControl
            + 'static,
    {
        self.materialization_control = Some(control.clone());
        self.publication_control = Some(control);
        self
    }

    #[must_use]
    pub fn with_transport_authorizer(
        mut self,
        authorizer: Arc<dyn RepositoryTransportAuthorizer>,
    ) -> Self {
        self.transport_authorizer = Some(authorizer);
        self
    }
}

pub fn worker_repository_binding_router(service: Arc<WorkerRepositoryBindingService>) -> Router {
    Router::new()
        .route(REPOSITORY_BINDING_PATH, post(verify_repository_binding))
        .route_layer(axum::middleware::from_fn_with_state(
            service.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(service)
}

async fn verify_repository_binding(
    State(service): State<Arc<WorkerRepositoryBindingService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RepositoryBindingRequest>,
) -> Response {
    if request.workspace_id.trim().is_empty() || request.repository_id.trim().is_empty() {
        return StatusCode::FORBIDDEN.into_response();
    }
    match request.authority.clone() {
        ResourceOperationFence::Run(claim) => {
            verify_run_repository_binding(service, &worker, request, claim).await
        }
        ResourceOperationFence::SessionMaterialization(intent) => {
            verify_session_repository_binding(service, &worker, request, intent).await
        }
        ResourceOperationFence::RepositoryPublication { command, lease } => {
            verify_terminal_repository_binding(service, &worker, request, *command, lease).await
        }
        ResourceOperationFence::CheckpointRelease(_) | ResourceOperationFence::Terminal(_) => {
            StatusCode::FORBIDDEN.into_response()
        }
    }
}

async fn verify_run_repository_binding(
    service: Arc<WorkerRepositoryBindingService>,
    worker: &VerifiedWorkerContext,
    request: RepositoryBindingRequest,
    claim: RunClaim,
) -> Response {
    if verify_claim_owner(
        service.directory.as_deref(),
        worker,
        request.identity.as_ref(),
        &claim,
        unix_now_ms(),
    )
    .await
    .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let guard = match service.dispatch.lock_commit_epoch(&claim).await {
        Ok(Some(guard)) if guard.is_live_at(unix_now_ms()) && !guard.cancellation_requested() => {
            guard
        }
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
    let frozen_input = manifest.as_ref().and_then(|manifest| {
        manifest.resources.inputs().iter().find(|input| {
            matches!(
                &input.source,
                awaken_session_resource_contract::ResolvedInputSource::Repository {
                    repository_id,
                    config,
                    ..
                } if repository_id.as_str() == request.repository_id
                    && config.version == request.config_version
            )
        })
    });
    if !scope_matches || frozen_input.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if service
        .validator
        .verify_repository_binding(
            &request.workspace_id,
            &request.repository_id,
            request.config_version,
        )
        .await
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(authorizer) = &service.transport_authorizer else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let Some(worker) = request.identity else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let dispatch_fingerprint = dispatch.canonical_fingerprint();
    let claim_expires_ms = guard.expires_ms();
    let session_id = dispatch.session_thread_id().0.clone();
    let frozen_input = frozen_input.expect("checked").clone();
    drop(guard);
    let transport = match authorizer
        .authorize(RepositoryTransportAuthorization {
            worker,
            session_id,
            workspace_id: request.workspace_id,
            input: frozen_input,
            authority: RepositoryTransportAuthority::Run {
                claim: claim.clone(),
                claim_expires_ms,
            },
        })
        .await
    {
        Ok(transport) => transport,
        Err(error) => return repository_verifier_status(&error).into_response(),
    };
    let revalidated = service
        .dispatch
        .lock_commit_epoch(&claim)
        .await
        .ok()
        .flatten()
        .is_some_and(|guard| {
            guard.is_live_at(unix_now_ms())
                && !guard.cancellation_requested()
                && guard.request().canonical_fingerprint() == dispatch_fingerprint
        });
    if !revalidated {
        return StatusCode::CONFLICT.into_response();
    }
    if !transport_expiry_is_admitted(
        &transport,
        unix_now_ms(),
        RepositoryTransportExpiryRule::BoundToAuthority(claim_expires_ms),
    ) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match transport {
        RepositoryTransport::Direct => StatusCode::NO_CONTENT.into_response(),
        transport => (StatusCode::OK, Json(transport)).into_response(),
    }
}

async fn verify_session_repository_binding(
    service: Arc<WorkerRepositoryBindingService>,
    worker: &VerifiedWorkerContext,
    request: RepositoryBindingRequest,
    intent: awaken_session_realization_contract::SessionResourceMaterializationIntent,
) -> Response {
    let Some(identity) = request.identity.as_ref() else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(directory) = service.directory.as_deref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(control) = service.materialization_control.as_deref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let now_ms = unix_now_ms();
    if let Some(status) = verify_session_worker_effect(
        directory,
        worker,
        identity,
        intent.lease(),
        now_ms,
        SessionWorkerEffectTemporalRule::AssertedLeaseMustBeLive,
    )
    .await
    .rejection_status()
    {
        return status.into_response();
    }
    let awaken_session_realization_contract::SessionResourceMaterializationTarget::Input(input) =
        intent.target()
    else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let awaken_session_resource_contract::ResolvedInputSource::Repository {
        repository_id,
        config,
        ..
    } = &input.source
    else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let workspace_id = match control
        .authorize_resource_materialization_intent(&intent)
        .await
    {
        Ok(workspace_id) => workspace_id,
        Err(
            awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(_),
        ) => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(_) => return StatusCode::CONFLICT.into_response(),
    };
    if workspace_id != request.workspace_id
        || repository_id.as_str() != request.repository_id
        || config.repository_id.as_str() != request.repository_id
        || config.version != request.config_version
        || service
            .validator
            .verify_repository_binding(
                &request.workspace_id,
                &request.repository_id,
                request.config_version,
            )
            .await
            .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(authorizer) = &service.transport_authorizer else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let transport = match authorizer
        .authorize(RepositoryTransportAuthorization {
            worker: identity.clone(),
            session_id: intent.session_id().to_string(),
            workspace_id: workspace_id.clone(),
            input: input.as_ref().clone(),
            authority: RepositoryTransportAuthority::SessionMaterialization {
                intent: Box::new(intent.clone()),
            },
        })
        .await
    {
        Ok(transport) => transport,
        Err(error) => return repository_verifier_status(&error).into_response(),
    };
    let latest_now_ms = unix_now_ms();
    if control
        .authorize_resource_materialization_intent(&intent)
        .await
        .ok()
        .as_deref()
        != Some(workspace_id.as_str())
    {
        return StatusCode::CONFLICT.into_response();
    }
    if let Some(status) = verify_session_worker_effect(
        directory,
        worker,
        identity,
        intent.lease(),
        latest_now_ms,
        SessionWorkerEffectTemporalRule::AssertedLeaseMustBeLive,
    )
    .await
    .rejection_status()
    {
        return status.into_response();
    }
    if !transport_expiry_is_admitted(
        &transport,
        latest_now_ms,
        RepositoryTransportExpiryRule::BoundToAuthority(intent.lease().expires_at_unix_ms),
    ) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match transport {
        RepositoryTransport::Direct => StatusCode::NO_CONTENT.into_response(),
        transport => (StatusCode::OK, Json(transport)).into_response(),
    }
}

async fn verify_terminal_repository_binding(
    service: Arc<WorkerRepositoryBindingService>,
    worker: &VerifiedWorkerContext,
    request: RepositoryBindingRequest,
    command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
    lease: awaken_session_resource_contract::SessionRealizationLease,
) -> Response {
    let Some(identity) = request.identity.as_ref() else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let Some(directory) = service.directory.as_deref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(control) = service.publication_control.as_deref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let now_ms = unix_now_ms();
    if let Some(status) = verify_session_worker_effect(
        directory,
        worker,
        identity,
        &lease,
        now_ms,
        SessionWorkerEffectTemporalRule::SessionRoot,
    )
    .await
    .rejection_status()
    {
        return status.into_response();
    }
    let projection = match control
        .terminal_repository_publication_command(&command.session_id, &lease)
        .await
    {
        Ok(Some(projection)) if projection.command == command => projection,
        Ok(_) => return StatusCode::CONFLICT.into_response(),
        Err(
            awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(_),
        ) => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(_) => return StatusCode::CONFLICT.into_response(),
    };
    if !awaken_session_realization_contract::realization_lease_authorizes(
        &projection.current_lease,
        &lease,
        now_ms,
    ) {
        return StatusCode::CONFLICT.into_response();
    }
    if let Some(status) = verify_session_worker_effect(
        directory,
        worker,
        identity,
        &projection.current_lease,
        now_ms,
        SessionWorkerEffectTemporalRule::AssertedLeaseMustBeLive,
    )
    .await
    .rejection_status()
    {
        return status.into_response();
    }
    if projection.workspace_id != request.workspace_id {
        return StatusCode::FORBIDDEN.into_response();
    }
    let canonical = &projection.command;
    let awaken_session_resource_contract::ResolvedInputSource::Repository {
        repository_id,
        config,
        ..
    } = &canonical.intent.input.source
    else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if canonical.intent.validate().is_err()
        || canonical.intent.input.access != awaken_resource_contract::ResourceAccess::ReadWrite
        || repository_id.as_str() != request.repository_id
        || config.repository_id.as_str() != request.repository_id
        || config.version != request.config_version
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(authorizer) = &service.transport_authorizer else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let transport = match authorizer
        .authorize(RepositoryTransportAuthorization {
            worker: identity.clone(),
            session_id: canonical.session_id.to_string(),
            workspace_id: projection.workspace_id.clone(),
            input: canonical.intent.input.clone(),
            authority: RepositoryTransportAuthority::TerminalPublication {
                command: Box::new(canonical.clone()),
                lease: projection.current_lease.clone(),
            },
        })
        .await
    {
        Ok(transport) => transport,
        Err(error) => return repository_verifier_status(&error).into_response(),
    };
    let latest = match control
        .terminal_repository_publication_command(&canonical.session_id, &lease)
        .await
    {
        Ok(Some(latest)) => latest,
        _ => return StatusCode::CONFLICT.into_response(),
    };
    let latest_now_ms = unix_now_ms();
    if latest.workspace_id != projection.workspace_id
        || latest.command != projection.command
        || !awaken_session_realization_contract::realization_lease_generation_authorizes(
            &latest.current_lease,
            &projection.current_lease,
        )
        || !awaken_session_realization_contract::realization_lease_authorizes(
            &latest.current_lease,
            &lease,
            latest_now_ms,
        )
    {
        return StatusCode::CONFLICT.into_response();
    }
    if let Some(status) = verify_session_worker_effect(
        directory,
        worker,
        identity,
        &latest.current_lease,
        latest_now_ms,
        SessionWorkerEffectTemporalRule::AssertedLeaseMustBeLive,
    )
    .await
    .rejection_status()
    {
        return status.into_response();
    }
    if !transport_expiry_is_admitted(
        &transport,
        latest_now_ms,
        RepositoryTransportExpiryRule::TrustedTerminalOperation,
    ) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match transport {
        RepositoryTransport::Direct => StatusCode::NO_CONTENT.into_response(),
        transport => (StatusCode::OK, Json(transport)).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_verifier_failures_keep_their_recovery_class_at_http_boundary() {
        // Cause/effect table: caller shape failure -> 400; denied authority ->
        // 403; stale/concurrent authority -> 409; unavailable or unknown read
        // decision -> 503; corrupt authority response -> 500. Every row must
        // remain distinct so clients never retry bad input or treat outage as
        // denial.
        let cases = [
            (
                RepositoryBindingVerifierError::InvalidRequest { detail: "x".into() },
                StatusCode::BAD_REQUEST,
            ),
            (
                RepositoryBindingVerifierError::Denied { detail: "x".into() },
                StatusCode::FORBIDDEN,
            ),
            (
                RepositoryBindingVerifierError::Conflict { detail: "x".into() },
                StatusCode::CONFLICT,
            ),
            (
                RepositoryBindingVerifierError::AuthorityUnavailable { detail: "x".into() },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                RepositoryBindingVerifierError::Indeterminate { detail: "x".into() },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                RepositoryBindingVerifierError::CorruptAuthority { detail: "x".into() },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(repository_verifier_status(&error), expected, "{error}");
        }
    }

    #[test]
    fn repository_binding_wire_rejects_unknown_authority_fields() {
        // Wire cause/effect decision table: R1 exact claim/workspace/repository/
        // revision => lossless request; R2 an unknown compatibility or attacker
        // field => reject before validation. Rules W1 R1+!R2=>decode; W2
        // R1+R2=>fail closed.
        let request = RepositoryBindingRequest {
            authority: ResourceOperationFence::Run(RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("run-repo".into()),
                owner: "worker-repo".into(),
                epoch: 5,
            }),
            identity: None,
            workspace_id: "workspace".into(),
            repository_id: "repository".into(),
            config_version: ConfigVersion(9),
        };
        let encoded = serde_json::to_value(&request).expect("W1 encode");
        let decoded: RepositoryBindingRequest =
            serde_json::from_value(encoded.clone()).expect("W1 decode");
        assert_eq!(decoded.authority, request.authority, "W1 authority");
        assert_eq!(decoded.config_version, request.config_version, "W1 version");

        let mut unknown = encoded;
        unknown
            .as_object_mut()
            .expect("request object")
            .insert("authority_bypass".into(), serde_json::json!(true));
        assert!(
            serde_json::from_value::<RepositoryBindingRequest>(unknown).is_err(),
            "W2"
        );
    }

    #[test]
    fn gateway_actual_expiry_obeys_run_and_terminal_operation_rules() {
        /* Dynamic expiry decision table:
         * C1=Direct; C2=Gateway expiry absent/present; C3=present expiry live;
         * C4=Run-bounded versus trusted exact terminal operation; C5=present
         * expiry <= current Run claim. E1=preserve Direct and additive legacy
         * Run host operations; E2=accept bounded Run evidence; E3=accept a live
         * issuer-owned terminal capability beyond the heartbeat lease; E4=fail
         * closed on a terminal capability without expiry, dead evidence, or a
         * widened Run capability. Rules: X1 C1=>E1; X2 Run+!C1+!C2=>E1;
         * X3 Terminal+!C1+!C2=>E4; X4 Run+C2+C3+C5=>E2;
         * X5 Terminal+C2+C3=>E3; X6 C2+!C3 or Run+C2+!C5=>E4.
         */
        let gateway = |expiry: Option<u64>| RepositoryTransport::GatewayMediated {
            remote_url: "https://gateway.invalid/git/repository".into(),
            capability: awaken_resource_contract::RepositoryGatewayCapability::new("cap").unwrap(),
            expires_at_unix_ms: expiry.map(|value| {
                awaken_resource_contract::RepositoryGatewayCapabilityExpiry::new(value).unwrap()
            }),
        };
        let run = RepositoryTransportExpiryRule::BoundToAuthority(200);
        let terminal = RepositoryTransportExpiryRule::TrustedTerminalOperation;
        assert!(transport_expiry_is_admitted(
            &RepositoryTransport::Direct,
            100,
            run,
        ));
        assert!(transport_expiry_is_admitted(
            &RepositoryTransport::Direct,
            100,
            terminal,
        ));
        assert!(transport_expiry_is_admitted(&gateway(None), 100, run));
        assert!(!transport_expiry_is_admitted(&gateway(None), 100, terminal,));
        assert!(transport_expiry_is_admitted(&gateway(Some(150)), 100, run));
        assert!(!transport_expiry_is_admitted(&gateway(Some(100)), 100, run));
        assert!(!transport_expiry_is_admitted(&gateway(Some(201)), 100, run));
        assert!(transport_expiry_is_admitted(
            &gateway(Some(225)),
            100,
            terminal,
        ));
        assert!(!transport_expiry_is_admitted(
            &gateway(Some(100)),
            100,
            terminal,
        ));
    }
}
