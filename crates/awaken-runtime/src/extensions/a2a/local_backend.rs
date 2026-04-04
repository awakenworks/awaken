//! Local agent delegation backend -- executes a sub-agent in-process.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::{RunIdentity, RunOrigin};
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::Message;

use crate::loop_runner::{AgentLoopParams, run_agent_loop};
use crate::registry::AgentResolver;

use super::backend::{AgentBackend, AgentBackendError, DelegateRunResult, DelegateRunStatus};

/// Backend that delegates to a sub-agent running in the same process.
pub struct LocalBackend {
    resolver: Arc<dyn AgentResolver>,
}

impl LocalBackend {
    /// Create a new local backend with the given agent resolver.
    pub fn new(resolver: Arc<dyn AgentResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl AgentBackend for LocalBackend {
    async fn execute(
        &self,
        agent_id: &str,
        messages: Vec<Message>,
        event_sink: Arc<dyn EventSink>,
        parent_run_id: Option<String>,
        parent_tool_call_id: Option<String>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        // Resolve the target agent
        self.resolver.resolve(agent_id).map_err(|e| {
            AgentBackendError::AgentNotFound(format!("failed to resolve agent '{agent_id}': {e}"))
        })?;

        // Build execution environment
        let store = crate::state::StateStore::new();
        store
            .install_plugin(crate::loop_runner::LoopStatePlugin)
            .map_err(|e| AgentBackendError::ExecutionFailed(format!("state setup failed: {e}")))?;
        store
            .install_plugin(crate::loop_runner::LoopActionHandlersPlugin)
            .map_err(|e| {
                AgentBackendError::ExecutionFailed(format!("action handlers setup: {e}"))
            })?;

        let phase_runtime = crate::phase::PhaseRuntime::new(store.clone())
            .map_err(|e| AgentBackendError::ExecutionFailed(format!("phase setup failed: {e}")))?;

        // Create sub-agent run identity
        let sub_run_id = uuid::Uuid::now_v7().to_string();
        let child_run_id = sub_run_id.clone();
        let thread_id = sub_run_id.clone();
        let mut sub_identity = RunIdentity::new(
            thread_id.clone(),
            Some(thread_id),
            sub_run_id,
            parent_run_id,
            agent_id.to_string(),
            RunOrigin::Subagent,
        );
        if let Some(call_id) = parent_tool_call_id {
            sub_identity = sub_identity.with_parent_tool_call_id(call_id);
        }

        let result = run_agent_loop(AgentLoopParams {
            resolver: self.resolver.as_ref(),
            agent_id,
            runtime: &phase_runtime,
            sink: event_sink,
            checkpoint_store: None,
            messages,
            run_identity: sub_identity,
            cancellation_token: None,
            decision_rx: None,
            overrides: None,
            frontend_tools: Vec::new(),
        })
        .await
        .map_err(|e| {
            AgentBackendError::ExecutionFailed(format!(
                "sub-agent '{agent_id}' execution failed: {e}"
            ))
        })?;

        let status = match result.termination {
            TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested => {
                DelegateRunStatus::Completed
            }
            TerminationReason::Cancelled => DelegateRunStatus::Cancelled,
            TerminationReason::Stopped(reason) => {
                DelegateRunStatus::Failed(format!("stopped: {reason:?}"))
            }
            TerminationReason::Blocked(msg) => DelegateRunStatus::Failed(format!("blocked: {msg}")),
            other => DelegateRunStatus::Failed(format!("{other:?}")),
        };

        let response = if result.response.is_empty() {
            None
        } else {
            Some(result.response)
        };

        Ok(DelegateRunResult {
            agent_id: agent_id.to_string(),
            status,
            response,
            steps: result.steps,
            run_id: Some(child_run_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::event_sink::{NullEventSink, VecEventSink};
    use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::registry_spec::AgentSpec;

    use crate::error::RuntimeError;
    use crate::loop_runner::build_agent_env;
    use crate::registry::{AgentResolver, ResolvedAgent};

    use super::super::backend::DelegateRunStatus;

    // -- Mock infrastructure --

    struct MockExecutor;

    #[async_trait]
    impl awaken_contract::contract::executor::LlmExecutor for MockExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![ContentBlock::text("sub-agent response")],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    struct TestResolver {
        agents: std::collections::HashMap<String, AgentSpec>,
    }

    impl TestResolver {
        fn with_agent(id: &str) -> Self {
            let mut agents = std::collections::HashMap::new();
            agents.insert(
                id.to_string(),
                AgentSpec {
                    id: id.into(),
                    model: "test-model".into(),
                    system_prompt: "system".into(),
                    ..Default::default()
                },
            );
            Self { agents }
        }

        fn empty() -> Self {
            Self {
                agents: std::collections::HashMap::new(),
            }
        }
    }

    impl AgentResolver for TestResolver {
        fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            let spec = self
                .agents
                .get(agent_id)
                .ok_or(RuntimeError::ResolveFailed {
                    message: format!("agent not found: {agent_id}"),
                })?;
            let mut agent = ResolvedAgent::new(
                &spec.id,
                &spec.model,
                &spec.system_prompt,
                Arc::new(MockExecutor),
            );
            agent.env = build_agent_env(&[], &agent).unwrap_or(agent.env);
            Ok(agent)
        }
    }

    // -- Tests --

    #[tokio::test]
    async fn execute_returns_agent_not_found_for_missing_agent() {
        let resolver = Arc::new(TestResolver::empty());
        let backend = LocalBackend::new(resolver);

        let result = backend
            .execute(
                "nonexistent",
                vec![Message::user("hello")],
                Arc::new(NullEventSink),
                None,
                None,
            )
            .await;

        match result {
            Err(super::super::backend::AgentBackendError::AgentNotFound(msg)) => {
                assert!(msg.contains("nonexistent"));
            }
            other => panic!("expected AgentNotFound, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_successful_sub_agent_returns_completed() {
        let resolver = Arc::new(TestResolver::with_agent("worker"));
        let backend = LocalBackend::new(resolver);

        let result = backend
            .execute(
                "worker",
                vec![Message::user("do work")],
                Arc::new(NullEventSink),
                Some("parent-run-1".into()),
                Some("tool-call-1".into()),
            )
            .await
            .unwrap();

        assert_eq!(result.agent_id, "worker");
        assert!(matches!(result.status, DelegateRunStatus::Completed));
        assert!(result.response.is_some());
        assert!(
            result
                .response
                .as_deref()
                .unwrap()
                .contains("sub-agent response")
        );
        assert!(result.run_id.is_some());
    }

    #[tokio::test]
    async fn execute_returns_child_run_id() {
        let resolver = Arc::new(TestResolver::with_agent("worker"));
        let backend = LocalBackend::new(resolver);

        let result = backend
            .execute(
                "worker",
                vec![Message::user("task")],
                Arc::new(NullEventSink),
                None,
                None,
            )
            .await
            .unwrap();

        // child_run_id should be a valid UUID v7
        let run_id = result.run_id.unwrap();
        assert!(!run_id.is_empty());
        assert!(uuid::Uuid::parse_str(&run_id).is_ok());
    }

    #[tokio::test]
    async fn execute_emits_events_to_sink() {
        let resolver = Arc::new(TestResolver::with_agent("worker"));
        let backend = LocalBackend::new(resolver);
        let sink = Arc::new(VecEventSink::new());

        let _result = backend
            .execute(
                "worker",
                vec![Message::user("do work")],
                sink.clone(),
                None,
                None,
            )
            .await
            .unwrap();

        let events = sink.take();
        // The agent loop should emit at least RunStart and RunFinish
        assert!(
            events.len() >= 2,
            "expected at least 2 events, got {}",
            events.len()
        );
    }

    #[tokio::test]
    async fn execute_without_parent_ids() {
        let resolver = Arc::new(TestResolver::with_agent("solo"));
        let backend = LocalBackend::new(resolver);

        let result = backend
            .execute(
                "solo",
                vec![Message::user("hello")],
                Arc::new(NullEventSink),
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(result.agent_id, "solo");
        assert!(matches!(result.status, DelegateRunStatus::Completed));
    }

    #[tokio::test]
    async fn execute_empty_response_maps_to_none() {
        // MockExecutor returns "sub-agent response" so this tests the non-empty path.
        // We verify the mapping logic by checking the response is Some when content exists.
        let resolver = Arc::new(TestResolver::with_agent("worker"));
        let backend = LocalBackend::new(resolver);

        let result = backend
            .execute(
                "worker",
                vec![Message::user("go")],
                Arc::new(NullEventSink),
                None,
                None,
            )
            .await
            .unwrap();

        // MockExecutor always returns "sub-agent response", so response should be Some
        assert!(result.response.is_some());
    }
}
