use awaken_runtime_contract::contract::identity::RunIdentity;
use awaken_runtime_contract::contract::inference::InferenceOverride;
use awaken_runtime_contract::contract::run::RunKind;
use awaken_runtime_contract::contract::tool::ToolDescriptor;
use awaken_runtime_contract::registry_spec::AgentSpec;

use crate::registry::{ResolvedAgent, ResolvedBackendAgent};
use crate::run::RunActivation;

use super::{BackendProfile, BackendRequirements, ResolveError};

#[derive(Debug, Clone)]
pub struct ResolutionRequest {
    pub target: ResolutionTarget,
    pub resolution_scope: RegistryResolutionScope,
    pub overrides: Option<InferenceOverride>,
    pub frontend_tools: Vec<ToolDescriptor>,
    pub features: RunFeatureSet,
}

impl ResolutionRequest {
    #[must_use]
    pub fn from_activation(activation: &RunActivation, policy: ResolutionPolicy) -> Self {
        Self::from_activation_with_scope(activation, policy, RegistryResolutionScope::Live)
    }

    #[must_use]
    pub fn from_activation_with_scope(
        activation: &RunActivation,
        policy: ResolutionPolicy,
        resolution_scope: RegistryResolutionScope,
    ) -> Self {
        let agent_id = activation
            .intent
            .agent_id
            .clone()
            .unwrap_or_else(|| "default".to_string());
        Self {
            target: ResolutionTarget::Root {
                agent_id,
                thread_id: activation.intent.thread_id.clone(),
            },
            resolution_scope,
            overrides: activation.options.overrides.clone(),
            frontend_tools: activation.options.frontend_tools.clone(),
            features: RunFeatureSet::from_activation(activation, policy),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum RegistryResolutionScope {
    #[default]
    Live,
    /// Bound to a server-owned resolved registry snapshot, referenced by an
    /// opaque id. The runtime never inspects the underlying manifest.
    Pinned(String),
}

#[derive(Debug, Clone, Default)]
pub struct RunFeatureSet {
    pub has_seeded_decisions: bool,
    pub has_live_decision_channel: bool,
    pub has_overrides: bool,
    pub has_frontend_tools: bool,
    pub is_human_resume: bool,
    pub is_continuation: bool,
    pub requested_persistence: PersistenceRequirement,
}

impl RunFeatureSet {
    #[must_use]
    pub fn from_activation(activation: &RunActivation, policy: ResolutionPolicy) -> Self {
        Self {
            has_seeded_decisions: !activation.control.seeded_decisions.is_empty(),
            has_live_decision_channel: activation.control.decision_rx.is_some(),
            has_overrides: activation.options.overrides.is_some(),
            has_frontend_tools: !activation.options.frontend_tools.is_empty(),
            is_human_resume: matches!(&activation.intent.kind, RunKind::HitlResume { .. }),
            is_continuation: matches!(&activation.intent.kind, RunKind::ContinuationFromRun { .. }),
            requested_persistence: PersistenceRequirement::from(policy),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionPolicy {
    PersistentServer,
    LiveOnlyEmbedded,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PersistenceRequirement {
    #[default]
    NotRequired,
    CheckpointRequired,
}

impl From<ResolutionPolicy> for PersistenceRequirement {
    fn from(policy: ResolutionPolicy) -> Self {
        match policy {
            ResolutionPolicy::PersistentServer => Self::CheckpointRequired,
            ResolutionPolicy::LiveOnlyEmbedded => Self::NotRequired,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ResolutionTarget {
    Root {
        agent_id: String,
        thread_id: String,
    },
    Delegate {
        agent_id: String,
        parent_run: RunIdentity,
        persistence: DelegatePersistence,
    },
    Handoff {
        agent_id: String,
        from_agent: String,
        transcript_ref: HandoffTranscriptRef,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegatePersistence {
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffTranscriptRef {
    pub run_id: String,
}

#[derive(Clone)]
pub enum ResolvedRunPlan {
    Replayable(ReplayableResolvedRun),
    LiveOnly(ResolvedRun<LiveOnlyScope>),
}

#[derive(Clone)]
pub struct ResolutionArtifact {
    /// Opaque id of the server-owned resolved registry snapshot.
    pub resolution_id: String,
}

#[derive(Clone)]
pub struct ReplayableResolvedRun {
    pub execution: ResolvedRun<ReplayableScope>,
    pub artifact: ResolutionArtifact,
}

/// Scope kind of a resolved plan, used for nested-resolution constraints
/// (ADR-0040 D7). A `Replayable` parent run cannot spawn a `LiveOnly`
/// sub-run; a `LiveOnly` parent accepts either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootScopeKind {
    Replayable,
    LiveOnly,
}

impl ResolvedRunPlan {
    pub(crate) fn from_execution_for_request(
        execution: ExecutionPlan,
        role: ExecutionRole,
        request: ResolutionRequest,
    ) -> Result<Self, ResolveError> {
        let requirements = BackendRequirements::from_features(&request.features);
        let backend_profile = execution.backend_profile()?;
        Ok(Self::from_execution(
            execution,
            role,
            request.overrides,
            requirements,
            backend_profile,
            request.resolution_scope,
        ))
    }

    #[must_use]
    pub(crate) fn from_execution(
        execution: ExecutionPlan,
        role: ExecutionRole,
        overrides: Option<InferenceOverride>,
        requirements: BackendRequirements,
        backend_profile: BackendProfile,
        resolution_scope: RegistryResolutionScope,
    ) -> Self {
        let agent_spec = execution.spec().clone();
        let (upstream_model, tools) = execution_model_and_tools(&execution);
        match resolution_scope {
            RegistryResolutionScope::Pinned(resolution_id) => {
                Self::Replayable(ReplayableResolvedRun {
                    execution: ResolvedRun {
                        agent_spec,
                        role,
                        execution,
                        model: ResolvedModelBinding { upstream_model },
                        tools,
                        overrides,
                        backend_profile,
                        requirements,
                        scope: ReplayableScope,
                    },
                    artifact: ResolutionArtifact { resolution_id },
                })
            }
            RegistryResolutionScope::Live => Self::LiveOnly(ResolvedRun {
                agent_spec,
                role,
                execution,
                model: ResolvedModelBinding { upstream_model },
                tools,
                overrides,
                backend_profile,
                requirements,
                scope: LiveOnlyScope,
            }),
        }
    }

    pub fn into_replayable(self) -> Result<ReplayableResolvedRun, ResolveError> {
        match self {
            Self::Replayable(plan) => Ok(plan),
            Self::LiveOnly(_) => Err(ResolveError::UnsupportedPersistence(
                "persistent execution requires a replayable resolved run".into(),
            )),
        }
    }

    #[must_use]
    pub fn execution(&self) -> &ExecutionPlan {
        match self {
            Self::Replayable(plan) => &plan.execution.execution,
            Self::LiveOnly(plan) => &plan.execution,
        }
    }

    #[must_use]
    pub fn agent_spec(&self) -> &AgentSpec {
        match self {
            Self::Replayable(plan) => &plan.execution.agent_spec,
            Self::LiveOnly(plan) => &plan.agent_spec,
        }
    }

    #[must_use]
    pub fn role(&self) -> ExecutionRole {
        match self {
            Self::Replayable(plan) => plan.execution.role,
            Self::LiveOnly(plan) => plan.role,
        }
    }

    #[must_use]
    pub fn resolution_id(&self) -> Option<&str> {
        match self {
            Self::Replayable(plan) => Some(plan.artifact.resolution_id.as_str()),
            Self::LiveOnly(_) => None,
        }
    }

    #[must_use]
    pub fn backend_profile(&self) -> &BackendProfile {
        match self {
            Self::Replayable(plan) => &plan.execution.backend_profile,
            Self::LiveOnly(plan) => &plan.backend_profile,
        }
    }

    #[must_use]
    pub fn requirements(&self) -> &BackendRequirements {
        match self {
            Self::Replayable(plan) => &plan.execution.requirements,
            Self::LiveOnly(plan) => &plan.requirements,
        }
    }

    #[must_use]
    pub fn root_scope_kind(&self) -> RootScopeKind {
        match self {
            Self::Replayable(_) => RootScopeKind::Replayable,
            Self::LiveOnly(_) => RootScopeKind::LiveOnly,
        }
    }
}

fn execution_model_and_tools(execution: &ExecutionPlan) -> (String, Vec<ResolvedTool>) {
    match execution {
        ExecutionPlan::Local(agent) => (
            agent.upstream_model.clone(),
            agent
                .tool_descriptors()
                .into_iter()
                .map(|descriptor| ResolvedTool { descriptor })
                .collect(),
        ),
        ExecutionPlan::Remote(agent) => (agent.spec.model_id.clone(), Vec::new()),
    }
}

#[derive(Clone)]
pub struct ReplayableScope;

#[derive(Clone)]
pub struct LiveOnlyScope;

#[derive(Clone)]
pub struct ResolvedRun<S> {
    pub agent_spec: AgentSpec,
    pub role: ExecutionRole,
    pub execution: ExecutionPlan,
    pub model: ResolvedModelBinding,
    pub tools: Vec<ResolvedTool>,
    pub overrides: Option<InferenceOverride>,
    pub backend_profile: BackendProfile,
    pub requirements: BackendRequirements,
    pub scope: S,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionRole {
    Root,
    Delegate,
    Handoff,
}

#[derive(Clone)]
pub enum ExecutionPlan {
    Local(Box<ResolvedAgent>),
    Remote(ResolvedBackendAgent),
}

impl ExecutionPlan {
    #[must_use]
    pub fn from_resolved_agent(agent: &ResolvedAgent) -> Self {
        Self::Local(Box::new(agent.clone()))
    }

    #[must_use]
    pub fn spec(&self) -> &AgentSpec {
        match self {
            Self::Local(agent) => agent.spec.as_ref(),
            Self::Remote(agent) => agent.spec.as_ref(),
        }
    }

    pub(crate) fn backend_profile(&self) -> Result<BackendProfile, ResolveError> {
        match self {
            Self::Local(_) => Ok(BackendProfile::full_local()),
            Self::Remote(agent) => Ok(agent.backend()?.capabilities()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelBinding {
    pub upstream_model: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedTool {
    pub descriptor: ToolDescriptor,
}
