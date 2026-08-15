//! Exact immutable custom-Skill materialization across the Coordinator-to-Worker boundary.
//!
//! Publication freezes a [`ResolvedSkillBinding`]. This boundary lets the Worker
//! read only that exact custom bundle while its dispatch claim is live. Skill
//! authoring, listing, retirement, deletion, and built-in Skills remain outside
//! this execution data plane.

use std::sync::Arc;

use awaken_agent_contract::AgentSkillKind;
use awaken_resource_contract::SkillVersion;
use awaken_run_ingress_contract::{DispatchQueue, RunClaim};
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

const SKILL_BUNDLE_PATH: &str = "/v1/worker/resources/skills/bundle";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillBundleRequest {
    claim: RunClaim,
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

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

async fn read_skill_bundle(
    State(service): State<Arc<WorkerSkillBundleService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SkillBundleRequest>,
) -> Response {
    if request.workspace_id.trim().is_empty()
        || request.binding.kind != AgentSkillKind::Custom
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
    let skill_is_frozen = manifest
        .as_ref()
        .and_then(|manifest| manifest.resources.skills.as_ref())
        .is_some_and(|bindings| bindings.contains(&request.binding));
    if !scope_matches || !skill_is_frozen {
        return StatusCode::FORBIDDEN.into_response();
    }
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
impl SkillBundleSource<RunClaim> for HttpSkillBundleSource {
    async fn load(
        &self,
        workspace_id: &str,
        binding: &ResolvedSkillBinding,
        claim: Option<&RunClaim>,
    ) -> Result<Option<SkillVersion>, SkillBundleSourceError> {
        let claim = claim.ok_or_else(|| {
            SkillBundleSourceError::new("remote Skill materialization requires a dispatch claim")
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
                claim: claim.clone(),
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
