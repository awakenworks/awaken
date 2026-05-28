use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::contract::inference::InferenceOverride;
use awaken_runtime_contract::contract::run::RunResolutionScope;

use crate::registry::{AgentResolver, ResolvedAgent};

use super::{
    BackendProfile, BackendRequirements, ExecutionPlan, ExecutionRole, LiveOnlyScope,
    ResolutionRequest, ResolutionTarget, ResolveError, ResolvedModelBinding, ResolvedRun,
    ResolvedRunPlan, ResolvedTool, Resolver,
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
        if matches!(req.resolution_scope, RunResolutionScope::Pinned(_)) {
            return Err(ResolveError::UnsupportedPersistence(
                "local-registry resolver cannot materialize pinned registry scopes".into(),
            ));
        }
        let execution = self.inner.resolve_execution(agent_id)?;
        let requirements = BackendRequirements::from_features(&req.features);
        match execution {
            ExecutionPlan::Local(agent) => Ok(ResolvedRunPlan::LiveOnly(resolved_local_live(
                *agent,
                ExecutionRole::Root,
                req.overrides,
                requirements,
            ))),
            ExecutionPlan::Remote(agent) => {
                let backend = agent.backend()?;
                let profile = backend.capabilities();
                let upstream_model = agent.spec.model_id.clone();
                Ok(ResolvedRunPlan::LiveOnly(ResolvedRun {
                    agent_spec: (*agent.spec).clone(),
                    role: ExecutionRole::Root,
                    execution: ExecutionPlan::Remote(agent),
                    model: ResolvedModelBinding { upstream_model },
                    tools: Vec::new(),
                    overrides: req.overrides,
                    backend_profile: profile,
                    requirements,
                    scope: LiveOnlyScope,
                }))
            }
        }
    }
}

fn resolved_local_live(
    agent: ResolvedAgent,
    role: ExecutionRole,
    overrides: Option<InferenceOverride>,
    requirements: BackendRequirements,
) -> ResolvedRun<LiveOnlyScope> {
    let tools = agent
        .tool_descriptors()
        .into_iter()
        .map(|descriptor| ResolvedTool { descriptor })
        .collect();
    ResolvedRun {
        agent_spec: (*agent.spec).clone(),
        role,
        execution: ExecutionPlan::from_resolved_agent(&agent),
        model: ResolvedModelBinding {
            upstream_model: agent.upstream_model.clone(),
        },
        tools,
        overrides,
        backend_profile: BackendProfile::full_local(),
        requirements,
        scope: LiveOnlyScope,
    }
}
