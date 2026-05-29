use awaken_runtime_contract::contract::identity::RunIdentity;
use awaken_runtime_contract::contract::inference::InferenceOverride;
use awaken_runtime_contract::contract::pinned_registry::PinnedRegistryManifest;
use awaken_runtime_contract::contract::run::{RunKind, RunResolutionScope};
use awaken_runtime_contract::contract::tool::ToolDescriptor;
use awaken_runtime_contract::registry_spec::AgentSpec;

use crate::registry::{ResolvedAgent, ResolvedBackendAgent};
use crate::run::RunActivation;

use super::{BackendProfile, BackendRequirements, ResolveError};

#[derive(Debug, Clone)]
pub struct ResolutionRequest {
    pub target: ResolutionTarget,
    pub resolution_scope: RunResolutionScope,
    pub overrides: Option<InferenceOverride>,
    pub frontend_tools: Vec<ToolDescriptor>,
    pub features: RunFeatureSet,
}

impl ResolutionRequest {
    #[must_use]
    pub fn from_activation(activation: &RunActivation, policy: ResolutionPolicy) -> Self {
        Self::from_activation_with_scope(activation, policy, RunResolutionScope::Live)
    }

    #[must_use]
    pub fn from_activation_with_scope(
        activation: &RunActivation,
        policy: ResolutionPolicy,
        resolution_scope: RunResolutionScope,
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
    Replayable(ResolvedRun<ReplayableScope>),
    LiveOnly(ResolvedRun<LiveOnlyScope>),
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
    pub fn into_replayable(self) -> Result<ResolvedRun<ReplayableScope>, ResolveError> {
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
            Self::Replayable(plan) => &plan.execution,
            Self::LiveOnly(plan) => &plan.execution,
        }
    }

    #[must_use]
    pub fn agent_spec(&self) -> &AgentSpec {
        match self {
            Self::Replayable(plan) => &plan.agent_spec,
            Self::LiveOnly(plan) => &plan.agent_spec,
        }
    }

    #[must_use]
    pub fn role(&self) -> ExecutionRole {
        match self {
            Self::Replayable(plan) => plan.role,
            Self::LiveOnly(plan) => plan.role,
        }
    }

    #[must_use]
    pub fn replayable_manifest(&self) -> Option<&PinnedRegistryManifest> {
        match self {
            Self::Replayable(plan) => Some(&plan.scope.manifest),
            Self::LiveOnly(_) => None,
        }
    }

    #[must_use]
    pub fn backend_profile(&self) -> &BackendProfile {
        match self {
            Self::Replayable(plan) => &plan.backend_profile,
            Self::LiveOnly(plan) => &plan.backend_profile,
        }
    }

    #[must_use]
    pub fn requirements(&self) -> &BackendRequirements {
        match self {
            Self::Replayable(plan) => &plan.requirements,
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

#[derive(Clone)]
pub struct ReplayableScope {
    pub manifest: PinnedRegistryManifest,
}

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelBinding {
    pub upstream_model: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedTool {
    pub descriptor: ToolDescriptor,
}
