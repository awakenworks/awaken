use std::sync::Arc;

use async_trait::async_trait;

use crate::registry::AgentResolver;

use super::{
    ExecutionRole, RegistryResolutionScope, ResolutionRequest, ResolutionTarget, ResolveError,
    ResolvedRunPlan, Resolver,
};

/// `Resolver` backed by an `AgentResolver` registry. Handles root local /
/// remote execution against the live registry; not valid for persistent
/// (pinned-manifest) submission paths, which must use a registry-aware
/// resolver that can materialise manifests.
pub struct LocalRegistryResolver {
    inner: Arc<dyn AgentResolver>,
}

impl LocalRegistryResolver {
    #[must_use]
    pub fn new(inner: Arc<dyn AgentResolver>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Resolver for LocalRegistryResolver {
    async fn resolve(&self, req: ResolutionRequest) -> Result<ResolvedRunPlan, ResolveError> {
        let ResolutionTarget::Root { agent_id, .. } = &req.target else {
            return Err(ResolveError::UnsupportedTarget(
                "local-registry resolver supports root resolution only".into(),
            ));
        };
        if matches!(req.resolution_scope, RegistryResolutionScope::Pinned(_)) {
            return Err(ResolveError::UnsupportedPersistence(
                "local-registry resolver cannot materialize pinned registry scopes".into(),
            ));
        }
        let execution = self.inner.resolve_execution(agent_id)?;
        ResolvedRunPlan::from_execution_for_request(execution, ExecutionRole::Root, req)
    }
}
