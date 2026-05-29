//! Run resolution boundary and typed backend capability profile.

use async_trait::async_trait;
use awaken_runtime_contract::registry_spec::AgentSpec;
use thiserror::Error;

use crate::RuntimeError;
use crate::backend::ExecutionBackendError;

mod capabilities;
mod local_registry;
mod types;

pub use capabilities::{
    BackendProfile, BackendRequirements, CancellationCapability, CapabilityDecision,
    CapabilityMismatch, ContinuationCapability, DecisionCapability, FrontendToolCapability,
    OutputCapability, OverrideCapability, PersistenceCapability, TranscriptCapability,
    WaitCapability,
};
pub use local_registry::LocalRegistryResolver;
pub use types::{
    DelegatePersistence, ExecutionPlan, ExecutionRole, HandoffTranscriptRef, LiveOnlyScope,
    PersistenceRequirement, RegistryResolutionScope, ReplayableResolvedRun, ReplayableScope,
    ResolutionArtifact, ResolutionPolicy, ResolutionRequest, ResolutionTarget,
    ResolvedModelBinding, ResolvedRun, ResolvedRunPlan, ResolvedTool, RootScopeKind, RunFeatureSet,
};

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("runtime resolve failed: {0}")]
    Runtime(String),
    #[error("unsupported resolution target: {0}")]
    UnsupportedTarget(String),
    #[error("unsupported persistence: {0}")]
    UnsupportedPersistence(String),
    #[error("backend capability mismatch: {0:?}")]
    CapabilityMismatch(Vec<CapabilityMismatch>),
    #[error("nested resolution scope mismatch: {0}")]
    NestedScopeMismatch(String),
}

impl From<RuntimeError> for ResolveError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error.to_string())
    }
}

impl From<ExecutionBackendError> for ResolveError {
    fn from(error: ExecutionBackendError) -> Self {
        Self::Runtime(error.to_string())
    }
}

#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, request: ResolutionRequest) -> Result<ResolvedRunPlan, ResolveError>;
}

#[async_trait]
pub trait AgentSpecLookup: Send + Sync {
    async fn resolve_spec(&self, agent_id: &str) -> Result<AgentSpec, ResolveError>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use awaken_runtime_contract::contract::identity::RunIdentity;
    use awaken_runtime_contract::contract::message::Message;

    use crate::registry::{AgentResolver, ResolvedAgent};
    use crate::run::RunActivation;

    fn resume_decision() -> awaken_runtime_contract::contract::suspension::ToolCallResume {
        awaken_runtime_contract::contract::suspension::ToolCallResume {
            decision_id: "d1".into(),
            action: awaken_runtime_contract::contract::suspension::ResumeDecisionAction::Resume,
            result: serde_json::Value::Null,
            reason: None,
            updated_at: 0,
        }
    }

    #[test]
    fn features_are_derived_from_activation_and_policy() {
        let activation = RunActivation::new("thread", vec![Message::user("hi")])
            .with_decisions(vec![("call".into(), resume_decision())])
            .with_hitl_resume_run_id("run-1");
        let features =
            RunFeatureSet::from_activation(&activation, ResolutionPolicy::PersistentServer);
        assert!(features.has_seeded_decisions);
        assert!(features.is_human_resume);
        assert_eq!(
            features.requested_persistence,
            PersistenceRequirement::CheckpointRequired
        );
    }

    #[test]
    fn requirements_are_derived_from_features() {
        let features = RunFeatureSet {
            has_seeded_decisions: true,
            has_live_decision_channel: true,
            has_overrides: true,
            has_frontend_tools: true,
            is_continuation: true,
            requested_persistence: PersistenceRequirement::CheckpointRequired,
            ..Default::default()
        };
        let req = BackendRequirements::from_features(&features);
        assert_eq!(req.decisions, Some(DecisionCapability::LiveAndDurable));
        assert_eq!(req.overrides, Some(OverrideCapability::InferenceParams));
        assert_eq!(
            req.frontend_tools,
            Some(FrontendToolCapability::DescriptorsOnly)
        );
        assert_eq!(req.persistence, Some(PersistenceCapability::Checkpoint));
        assert_eq!(
            req.continuation,
            Some(ContinuationCapability::InProcessState)
        );
    }

    #[test]
    fn profile_check_returns_all_mismatches() {
        let profile = BackendProfile {
            decisions: DecisionCapability::None,
            overrides: OverrideCapability::None,
            frontend_tools: FrontendToolCapability::None,
            persistence: PersistenceCapability::Ephemeral,
            ..BackendProfile::full_local()
        };
        let req = BackendRequirements {
            decisions: Some(DecisionCapability::DurableResume),
            overrides: Some(OverrideCapability::InferenceParams),
            frontend_tools: Some(FrontendToolCapability::DescriptorsOnly),
            persistence: Some(PersistenceCapability::Checkpoint),
            cancellation: None,
            continuation: None,
            waits: None,
            transcript: None,
            output: None,
        };
        let CapabilityDecision::Unsupported(mismatches) = profile.check(&req) else {
            panic!("expected mismatches")
        };
        assert_eq!(mismatches.len(), 4);
    }

    #[test]
    fn full_local_matches_contract_table() {
        let profile = BackendProfile::full_local();
        assert_eq!(profile.decisions, DecisionCapability::LiveAndDurable);
        assert_eq!(profile.overrides, OverrideCapability::ModelAndParams);
        assert_eq!(profile.frontend_tools, FrontendToolCapability::Executable);
        assert_eq!(profile.persistence, PersistenceCapability::Checkpoint);
    }

    #[test]
    fn remote_state_transcript_satisfies_full_transcript_requirement() {
        let profile = BackendProfile {
            transcript: TranscriptCapability::IncrementalUserMessagesWithRemoteState,
            ..BackendProfile::full_local()
        };
        let req = BackendRequirements {
            transcript: Some(TranscriptCapability::FullTranscript),
            cancellation: None,
            continuation: None,
            decisions: None,
            overrides: None,
            frontend_tools: None,
            persistence: None,
            waits: None,
            output: None,
        };
        assert!(matches!(profile.check(&req), CapabilityDecision::Supported));
    }

    struct MockResolver;

    #[async_trait::async_trait]
    impl awaken_runtime_contract::contract::executor::LlmExecutor for MockResolver {
        async fn execute(
            &self,
            _request: awaken_runtime_contract::contract::executor::InferenceRequest,
        ) -> Result<
            awaken_runtime_contract::contract::inference::StreamResult,
            awaken_runtime_contract::contract::executor::InferenceExecutionError,
        > {
            Ok(awaken_runtime_contract::contract::inference::StreamResult {
                content: vec![],
                tool_calls: vec![],
                usage: None,
                stop_reason: None,
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    impl AgentResolver for MockResolver {
        fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            Ok(ResolvedAgent::new(
                agent_id,
                "model",
                "system",
                Arc::new(MockResolver),
            ))
        }
    }

    #[tokio::test]
    async fn legacy_adapter_rejects_pinned_and_non_root_requests() {
        let adapter = LocalRegistryResolver::new(Arc::new(MockResolver));
        let activation =
            RunActivation::new("thread", vec![Message::user("hi")]).with_agent_id("agent-a");
        let mut request =
            ResolutionRequest::from_activation(&activation, ResolutionPolicy::LiveOnlyEmbedded);
        request.resolution_scope = RegistryResolutionScope::Pinned("resolution-1".to_string());
        assert!(matches!(
            adapter.resolve(request).await,
            Err(ResolveError::UnsupportedPersistence(_))
        ));

        let mut request =
            ResolutionRequest::from_activation(&activation, ResolutionPolicy::LiveOnlyEmbedded);
        request.target = ResolutionTarget::Delegate {
            agent_id: "agent-a".into(),
            parent_run: RunIdentity::for_thread("thread"),
            persistence: DelegatePersistence::Ephemeral,
        };
        assert!(matches!(
            adapter.resolve(request).await,
            Err(ResolveError::UnsupportedTarget(_))
        ));
    }

    #[tokio::test]
    async fn runtime_rejects_live_only_plan_for_persistent_policy() {
        let runtime = crate::AgentRuntime::new(Arc::new(MockResolver));
        let activation =
            RunActivation::new("thread", vec![Message::user("hi")]).with_agent_id("agent-a");
        assert!(matches!(
            runtime
                .resolve_activation(&activation, ResolutionPolicy::PersistentServer)
                .await,
            Err(ResolveError::UnsupportedPersistence(_))
        ));
    }

    /// Minimal `Resolver` impl that always returns LiveOnly and accepts any
    /// target. Used to exercise nested-resolution scope validation without
    /// going through the legacy adapter (which rejects Delegate / Handoff).
    struct LiveOnlyEverywhereResolver;

    #[async_trait::async_trait]
    impl Resolver for LiveOnlyEverywhereResolver {
        async fn resolve(&self, req: ResolutionRequest) -> Result<ResolvedRunPlan, ResolveError> {
            let agent_id = match &req.target {
                ResolutionTarget::Root { agent_id, .. } => agent_id.clone(),
                ResolutionTarget::Delegate { agent_id, .. } => agent_id.clone(),
                ResolutionTarget::Handoff { agent_id, .. } => agent_id.clone(),
            };
            let role = match &req.target {
                ResolutionTarget::Root { .. } => ExecutionRole::Root,
                ResolutionTarget::Delegate { .. } => ExecutionRole::Delegate,
                ResolutionTarget::Handoff { .. } => ExecutionRole::Handoff,
            };
            let requirements = BackendRequirements::from_features(&req.features);
            let agent = ResolvedAgent::new(&agent_id, "model", "system", Arc::new(MockResolver));
            Ok(ResolvedRunPlan::LiveOnly(ResolvedRun {
                agent_spec: (*agent.spec).clone(),
                role,
                execution: ExecutionPlan::from_resolved_agent(&agent),
                model: ResolvedModelBinding {
                    upstream_model: agent.upstream_model.clone(),
                },
                tools: Vec::new(),
                overrides: req.overrides,
                backend_profile: BackendProfile::full_local(),
                requirements,
                scope: LiveOnlyScope,
            }))
        }
    }

    struct ReplayableEverywhereResolver;

    #[async_trait::async_trait]
    impl Resolver for ReplayableEverywhereResolver {
        async fn resolve(&self, req: ResolutionRequest) -> Result<ResolvedRunPlan, ResolveError> {
            let agent_id = match &req.target {
                ResolutionTarget::Root { agent_id, .. } => agent_id.clone(),
                ResolutionTarget::Delegate { agent_id, .. } => agent_id.clone(),
                ResolutionTarget::Handoff { agent_id, .. } => agent_id.clone(),
            };
            let role = match &req.target {
                ResolutionTarget::Root { .. } => ExecutionRole::Root,
                ResolutionTarget::Delegate { .. } => ExecutionRole::Delegate,
                ResolutionTarget::Handoff { .. } => ExecutionRole::Handoff,
            };
            let requirements = BackendRequirements::from_features(&req.features);
            let agent = ResolvedAgent::new(&agent_id, "model", "system", Arc::new(MockResolver));
            Ok(ResolvedRunPlan::Replayable(ReplayableResolvedRun {
                execution: ResolvedRun {
                    agent_spec: (*agent.spec).clone(),
                    role,
                    execution: ExecutionPlan::from_resolved_agent(&agent),
                    model: ResolvedModelBinding {
                        upstream_model: agent.upstream_model.clone(),
                    },
                    tools: Vec::new(),
                    overrides: req.overrides,
                    backend_profile: BackendProfile::full_local(),
                    requirements,
                    scope: ReplayableScope,
                },
                artifact: ResolutionArtifact {
                    resolution_id: "override-publication".to_string(),
                },
            }))
        }
    }

    fn runtime_with_resolver<R: Resolver + 'static>(r: R) -> crate::AgentRuntime {
        let runtime = crate::AgentRuntime::new(Arc::new(MockResolver));
        runtime.set_run_resolver(Arc::new(r));
        runtime
    }

    #[tokio::test]
    async fn nested_resolve_rejects_root_target() {
        let runtime = runtime_with_resolver(LiveOnlyEverywhereResolver);
        let sub =
            RunActivation::new("thread", vec![Message::user("delegate")]).with_agent_id("agent-a");
        let target = ResolutionTarget::Root {
            agent_id: "agent-a".into(),
            thread_id: "thread".into(),
        };
        assert!(matches!(
            runtime
                .resolve_nested(RootScopeKind::LiveOnly, &sub, target)
                .await,
            Err(ResolveError::UnsupportedTarget(_))
        ));
    }

    #[tokio::test]
    async fn nested_resolve_replayable_parent_rejects_live_only_sub() {
        let runtime = runtime_with_resolver(LiveOnlyEverywhereResolver);
        let sub =
            RunActivation::new("thread", vec![Message::user("delegate")]).with_agent_id("agent-a");
        let target = ResolutionTarget::Delegate {
            agent_id: "agent-a".into(),
            parent_run: RunIdentity::for_thread("thread"),
            persistence: DelegatePersistence::Ephemeral,
        };
        assert!(matches!(
            runtime
                .resolve_nested(RootScopeKind::Replayable, &sub, target)
                .await,
            Err(ResolveError::NestedScopeMismatch(_))
        ));
    }

    #[tokio::test]
    async fn nested_resolve_live_only_parent_accepts_live_only_sub() {
        let runtime = runtime_with_resolver(LiveOnlyEverywhereResolver);
        let sub =
            RunActivation::new("thread", vec![Message::user("delegate")]).with_agent_id("agent-a");
        let target = ResolutionTarget::Delegate {
            agent_id: "agent-a".into(),
            parent_run: RunIdentity::for_thread("thread"),
            persistence: DelegatePersistence::Ephemeral,
        };
        let plan = runtime
            .resolve_nested(RootScopeKind::LiveOnly, &sub, target)
            .await
            .expect("live-only parent accepts live-only sub");
        assert!(matches!(plan, ResolvedRunPlan::LiveOnly(_)));
    }

    #[tokio::test]
    async fn activation_scoped_resolver_overrides_runtime_default() {
        let runtime = crate::AgentRuntime::new(Arc::new(MockResolver));
        let activation = RunActivation::new("thread", vec![Message::user("hi")])
            .with_agent_id("agent-a")
            .with_run_resolver(Arc::new(ReplayableEverywhereResolver));

        let plan = runtime
            .resolve_activation(&activation, ResolutionPolicy::PersistentServer)
            .await
            .expect("activation resolver supplies replayable plan");

        assert_eq!(plan.resolution_id(), Some("override-publication"));
        // Exercise the shared ResolvedRunPlan accessors on the replayable plan.
        assert_eq!(plan.agent_spec().id, "agent-a");
        assert_eq!(plan.role(), ExecutionRole::Root);
        let _ = plan.execution();
        let _ = plan.backend_profile();
        let replayable = plan.into_replayable().expect("plan is replayable");
        assert_eq!(replayable.artifact.resolution_id, "override-publication");
    }
}
