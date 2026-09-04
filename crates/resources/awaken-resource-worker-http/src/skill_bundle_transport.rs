//! Exact immutable custom-Skill materialization across the Coordinator-to-Worker boundary.
//!
//! Publication freezes a [`ResolvedSkillBinding`]. This boundary lets the Worker
//! read only that exact custom bundle while its dispatch claim is live. Skill
//! authoring, listing, retirement, deletion, and built-in Skills remain outside
//! this execution data plane.

use std::sync::Arc;

use awaken_agent_contract::AgentSkillKind;
use awaken_resource_contract::SkillVersion;
use awaken_dispatch_contract::{DispatchQueue, ResourceOperationFence, RunClaim};
use awaken_session_contract::{
    ResolvedSkillBinding, SkillBundleSource, SkillBundleSourceError, validate_skill_bundle,
};
use awaken_worker_contract::{WorkerDirectory, WorkerIdentity};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, WorkerUpstream, authenticate_worker_request,
    verify_claim_owner,
};

use crate::worker_authority::{
    SessionWorkerEffectTemporalRule, unix_now_ms, verify_session_worker_effect,
};

const SKILL_BUNDLE_PATH: &str = "/v1/worker/resources/skills/bundle";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillBundleRequest {
    authority: ResourceOperationFence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<WorkerIdentity>,
    workspace_id: String,
    binding: ResolvedSkillBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillBundleResponse {
    workspace_id: String,
    version: SkillVersion,
}

/// Handler dependencies for claim-fenced exact custom-Skill reads.
pub struct WorkerSkillBundleService {
    source: Arc<dyn SkillBundleSource<RunClaim>>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Option<Arc<dyn WorkerDirectory>>,
    session_control: Option<Arc<dyn awaken_session_contract::SessionRealizationControl>>,
}

impl WorkerSkillBundleService {
    #[must_use]
    pub fn new(
        source: Arc<dyn SkillBundleSource<RunClaim>>,
        dispatch: Arc<dyn DispatchQueue>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            source,
            dispatch,
            authenticator,
            directory: None,
            session_control: None,
        }
    }

    /// Require the current Coordinator registration in addition to transport
    /// authentication. Registered production composition always installs this.
    #[must_use]
    pub fn with_worker_directory(mut self, directory: Arc<dyn WorkerDirectory>) -> Self {
        self.directory = Some(directory);
        self
    }

    #[must_use]
    pub fn with_session_control(
        mut self,
        control: Arc<dyn awaken_session_contract::SessionRealizationControl>,
    ) -> Self {
        self.session_control = Some(control);
        self
    }
}

/// Mount only the exact custom-Skill read boundary used by registered Workers.
pub fn worker_skill_bundle_router(service: Arc<WorkerSkillBundleService>) -> Router {
    Router::new()
        .route(SKILL_BUNDLE_PATH, post(read_skill_bundle))
        .route_layer(axum::middleware::from_fn_with_state(
            service.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(service)
}

async fn read_skill_bundle(
    State(service): State<Arc<WorkerSkillBundleService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SkillBundleRequest>,
) -> Response {
    if request.workspace_id.trim().is_empty() || request.binding.kind != AgentSkillKind::Custom {
        return StatusCode::FORBIDDEN.into_response();
    }
    let now_ms = unix_now_ms();
    let mut run_guard = None;
    match &request.authority {
        ResourceOperationFence::Run(claim) => {
            if verify_claim_owner(
                service.directory.as_deref(),
                &worker,
                request.identity.as_ref(),
                claim,
                now_ms,
            )
            .await
            .is_err()
            {
                return StatusCode::FORBIDDEN.into_response();
            }
            let guard = match service.dispatch.lock_commit_epoch(claim).await {
                Ok(Some(guard)) if guard.is_live_at(now_ms) => guard,
                Ok(_) => return StatusCode::CONFLICT.into_response(),
                Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
            let dispatch = guard.request();
            let scope_matches = dispatch
                .execution_scope
                .as_ref()
                .is_some_and(|scope| scope.0.0 == request.workspace_id);
            let skill_is_frozen = dispatch
                .session_resources
                .as_ref()
                .filter(|envelope| envelope.workspace_id == request.workspace_id)
                .and_then(|envelope| envelope.decode_manifest().ok())
                .is_some_and(|manifest| manifest.resources.skills().contains(&request.binding));
            if !scope_matches || !skill_is_frozen {
                return StatusCode::FORBIDDEN.into_response();
            }
            run_guard = Some(guard);
        }
        ResourceOperationFence::SessionMaterialization(intent) => {
            let (Some(identity), Some(directory), Some(control)) = (
                request.identity.as_ref(),
                service.directory.as_deref(),
                service.session_control.as_deref(),
            ) else {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            };
            if let Some(status) = verify_session_worker_effect(
                directory,
                &worker,
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
            let workspace = match control
                .authorize_resource_materialization_intent(intent)
                .await
            {
                Ok(workspace) => workspace,
                Err(awaken_session_contract::SessionRealizationControlFailure::Unavailable(_))
                | Err(awaken_session_contract::SessionRealizationControlFailure::Conflict) => {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership) => {
                    return StatusCode::CONFLICT.into_response();
                }
                Err(_) => return StatusCode::FORBIDDEN.into_response(),
            };
            if workspace != request.workspace_id
                || !matches!(
                    intent.target(),
                    awaken_session_contract::SessionResourceMaterializationTarget::Skill(binding)
                        if binding == &request.binding
                )
            {
                return StatusCode::FORBIDDEN.into_response();
            }
        }
        ResourceOperationFence::RepositoryPublication { .. }
        | ResourceOperationFence::CheckpointRelease(_)
        | ResourceOperationFence::Terminal(_) => {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let _run_guard = run_guard;
    match service
        .source
        .load(&request.workspace_id, &request.binding, None)
        .await
    {
        Ok(Some(version)) => Json(SkillBundleResponse {
            workspace_id: request.workspace_id,
            version,
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Registered-Worker adapter for one exact immutable custom-Skill source.
#[derive(Clone)]
pub struct HttpSkillBundleSource {
    upstream: WorkerUpstream,
}

impl HttpSkillBundleSource {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self { upstream }
    }
}

#[async_trait::async_trait]
impl SkillBundleSource<ResourceOperationFence> for HttpSkillBundleSource {
    async fn load(
        &self,
        workspace_id: &str,
        binding: &ResolvedSkillBinding,
        authority: Option<&ResourceOperationFence>,
    ) -> Result<Option<SkillVersion>, SkillBundleSourceError> {
        let authority = authority.ok_or_else(|| {
            SkillBundleSourceError::new(
                "remote Skill materialization requires an exact Resource authority",
            )
        })?;
        if workspace_id.trim().is_empty() || binding.kind != AgentSkillKind::Custom {
            return Err(SkillBundleSourceError::new(
                "a Workspace and custom Skill binding are required",
            ));
        }
        let request = self
            .upstream
            .http_client()
            .post(format!("{}{SKILL_BUNDLE_PATH}", self.upstream.base_url()))
            .json(&SkillBundleRequest {
                authority: authority.clone(),
                identity: self.upstream.worker_identity().cloned(),
                workspace_id: workspace_id.to_owned(),
                binding: binding.clone(),
            });
        let request = self
            .upstream
            .authorize_request("POST", SKILL_BUNDLE_PATH, request)
            .map_err(SkillBundleSourceError::new)?;
        let response = request
            .send()
            .await
            .map_err(|error| SkillBundleSourceError::new(error.to_string()))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() != StatusCode::OK {
            return Err(SkillBundleSourceError::new(format!(
                "Skill bundle authority returned HTTP {}",
                response.status()
            )));
        }
        let response = response
            .json::<SkillBundleResponse>()
            .await
            .map_err(|error| SkillBundleSourceError::new(error.to_string()))?;
        validate_skill_bundle(
            workspace_id,
            &response.workspace_id,
            binding,
            response.version,
        )
        .map(Some)
    }
}
