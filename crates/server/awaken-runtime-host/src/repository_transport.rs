//! Exact Repository binding verification across the Coordinator-to-Worker boundary.
//!
//! Repository bytes remain owned by the external Git provider and are realized
//! through the existing `RepositoryRealizer`. This boundary only proves that the
//! frozen Workspace/config binding remains live while the Worker's dispatch
//! claim is current; it exposes no catalog read or mutation operation.

use std::sync::Arc;

use awaken_resource_contract::{ConfigVersion, ResourceBindingValidator};
use awaken_run_ingress::{DispatchQueue, RunClaim, WorkerDirectory, WorkerIdentity};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::worker_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, WorkerUpstream, authenticate_worker_request,
    verify_claim_owner,
};

const REPOSITORY_BINDING_PATH: &str = "/v1/worker/resources/repositories/verify";

/// Failure at the exact Repository binding-verification boundary.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("Repository binding verifier: {0}")]
pub struct RepositoryBindingVerifierError(String);

impl RepositoryBindingVerifierError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Verify one exact frozen Repository configuration before Git realization/use.
#[async_trait::async_trait]
pub trait RepositoryBindingVerifier: Send + Sync {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        claim: Option<&RunClaim>,
    ) -> Result<(), RepositoryBindingVerifierError>;
}

/// Local adapter over the authoritative Resource Catalog invariant port.
pub struct CatalogRepositoryBindingVerifier {
    validator: Arc<dyn ResourceBindingValidator>,
}

impl CatalogRepositoryBindingVerifier {
    #[must_use]
    pub fn new(validator: Arc<dyn ResourceBindingValidator>) -> Self {
        Self { validator }
    }
}

#[async_trait::async_trait]
impl RepositoryBindingVerifier for CatalogRepositoryBindingVerifier {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        _claim: Option<&RunClaim>,
    ) -> Result<(), RepositoryBindingVerifierError> {
        self.validator
            .validate_repository_binding(workspace_id, repository_id, config_version)
            .map_err(|error| RepositoryBindingVerifierError::new(error.to_string()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryBindingRequest {
    claim: RunClaim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<WorkerIdentity>,
    workspace_id: String,
    repository_id: String,
    config_version: ConfigVersion,
}

/// Handler dependencies for claim-fenced Repository binding verification.
pub struct WorkerRepositoryBindingService {
    validator: Arc<dyn ResourceBindingValidator>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Option<Arc<dyn WorkerDirectory>>,
}

impl WorkerRepositoryBindingService {
    #[must_use]
    pub fn new(
        validator: Arc<dyn ResourceBindingValidator>,
        dispatch: Arc<dyn DispatchQueue>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            validator,
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

/// Mount only the Repository binding guard used by registered Workers.
pub fn worker_repository_binding_router(service: Arc<WorkerRepositoryBindingService>) -> Router {
    Router::new()
        .route(REPOSITORY_BINDING_PATH, post(verify_repository_binding))
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

async fn verify_repository_binding(
    State(service): State<Arc<WorkerRepositoryBindingService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RepositoryBindingRequest>,
) -> Response {
    if request.workspace_id.trim().is_empty()
        || request.repository_id.trim().is_empty()
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
        .and_then(|envelope| crate::provisioning::decode_session_resource_envelope(envelope).ok());
    let binding_is_frozen = manifest.as_ref().is_some_and(|manifest| {
        manifest.resources.inputs.iter().any(|input| {
            matches!(
                &input.source,
                awaken_session_contract::ResolvedInputSource::Repository {
                    repository_id,
                    config,
                    ..
                } if repository_id.as_str() == request.repository_id
                    && config.version == request.config_version
            )
        })
    });
    if !scope_matches || !binding_is_frozen {
        return StatusCode::FORBIDDEN.into_response();
    }
    match service.validator.validate_repository_binding(
        &request.workspace_id,
        &request.repository_id,
        request.config_version,
    ) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::FORBIDDEN.into_response(),
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
}

#[async_trait::async_trait]
impl RepositoryBindingVerifier for HttpRepositoryBindingVerifier {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        claim: Option<&RunClaim>,
    ) -> Result<(), RepositoryBindingVerifierError> {
        let claim = claim.ok_or_else(|| {
            RepositoryBindingVerifierError::new(
                "remote Repository verification requires a dispatch claim",
            )
        })?;
        if workspace_id.trim().is_empty() || repository_id.trim().is_empty() {
            return Err(RepositoryBindingVerifierError::new(
                "Workspace and Repository identities must not be empty",
            ));
        }
        let request = self
            .upstream
            .http_client()
            .post(format!(
                "{}{REPOSITORY_BINDING_PATH}",
                self.upstream.base_url()
            ))
            .json(&RepositoryBindingRequest {
                claim: claim.clone(),
                identity: self.upstream.worker_identity().cloned(),
                workspace_id: workspace_id.to_owned(),
                repository_id: repository_id.to_owned(),
                config_version,
            });
        let request = self
            .upstream
            .authorize_request("POST", REPOSITORY_BINDING_PATH, request)
            .map_err(RepositoryBindingVerifierError::new)?;
        let response = request
            .send()
            .await
            .map_err(|error| RepositoryBindingVerifierError::new(error.to_string()))?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(RepositoryBindingVerifierError::new(format!(
                "Repository binding authority returned HTTP {}",
                response.status()
            )));
        }
        Ok(())
    }
}
