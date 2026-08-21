//! Coordinator HTTP adapter for claim-fenced Repository binding validation.

use std::sync::Arc;

use awaken_resource_contract::{
    ConfigVersion, LiveResourceBindingVerifier, RepositoryBindingVerifier,
    RepositoryBindingVerifierError, RepositoryTransport,
};
use awaken_run_ingress_contract::{DispatchQueue, RunClaim};
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

const REPOSITORY_BINDING_PATH: &str = "/v1/worker/resources/repositories/verify";

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

pub struct WorkerRepositoryBindingService {
    validator: Arc<dyn LiveResourceBindingVerifier>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Option<Arc<dyn WorkerDirectory>>,
    transport_authorizer: Option<Arc<dyn RepositoryTransportAuthorizer>>,
}

/// Exact frozen facts supplied to the deployment adapter after the common
/// Worker identity, claim, Session manifest, and Resource checks have passed.
#[derive(Debug, Clone)]
pub struct RepositoryTransportAuthorization {
    pub worker: WorkerIdentity,
    pub claim: RunClaim,
    pub claim_expires_ms: u64,
    pub session_id: String,
    pub workspace_id: String,
    pub input: awaken_session_contract::ResolvedInput,
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
impl RepositoryBindingVerifier<RunClaim> for HttpRepositoryBindingVerifier {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        claim: Option<&RunClaim>,
    ) -> Result<RepositoryTransport, RepositoryBindingVerifierError> {
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
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(RepositoryTransport::Direct);
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(RepositoryBindingVerifierError::new(format!(
                "Repository binding authority returned HTTP {}",
                response.status()
            )));
        }
        response.json::<RepositoryTransport>().await.map_err(|_| {
            RepositoryBindingVerifierError::new(
                "Repository binding authority returned an invalid transport",
            )
        })
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
            transport_authorizer: None,
        }
    }

    #[must_use]
    pub fn with_worker_directory(mut self, directory: Arc<dyn WorkerDirectory>) -> Self {
        self.directory = Some(directory);
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
                awaken_session_contract::ResolvedInputSource::Repository {
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
            claim: request.claim.clone(),
            claim_expires_ms,
            session_id,
            workspace_id: request.workspace_id,
            input: frozen_input,
        })
        .await
    {
        Ok(transport) => transport,
        Err(_) => return StatusCode::FORBIDDEN.into_response(),
    };
    let revalidated = service
        .dispatch
        .lock_commit_epoch(&request.claim)
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
    match transport {
        RepositoryTransport::Direct => StatusCode::NO_CONTENT.into_response(),
        transport => (StatusCode::OK, Json(transport)).into_response(),
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
    fn repository_binding_wire_rejects_unknown_authority_fields() {
        // Wire cause/effect decision table: R1 exact claim/workspace/repository/
        // revision => lossless request; R2 an unknown compatibility or attacker
        // field => reject before validation. Rules W1 R1+!R2=>decode; W2
        // R1+R2=>fail closed.
        let request = RepositoryBindingRequest {
            claim: RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("run-repo".into()),
                owner: "worker-repo".into(),
                epoch: 5,
            },
            identity: None,
            workspace_id: "workspace".into(),
            repository_id: "repository".into(),
            config_version: ConfigVersion(9),
        };
        let encoded = serde_json::to_value(&request).expect("W1 encode");
        let decoded: RepositoryBindingRequest =
            serde_json::from_value(encoded.clone()).expect("W1 decode");
        assert_eq!(decoded.claim, request.claim, "W1 claim");
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
}
