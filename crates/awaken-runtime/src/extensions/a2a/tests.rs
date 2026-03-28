use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool::{Tool, ToolCallContext};
use awaken_contract::registry_spec::{AgentSpec, RemoteEndpoint};

use crate::loop_runner::build_agent_env;
use crate::registry::{AgentResolver, ResolvedAgent};

use super::a2a_backend::A2aConfig;
use super::agent_tool::AgentTool;
use super::backend::{AgentBackend, AgentBackendError, DelegateRunResult, DelegateRunStatus};

// -- Mock Resolver --

struct MockResolver {
    agents: std::collections::HashMap<String, AgentSpec>,
}

impl MockResolver {
    fn with_agent(id: &str) -> Self {
        let mut agents = std::collections::HashMap::new();
        agents.insert(
            id.to_string(),
            AgentSpec {
                id: id.into(),
                model: "test-model".into(),
                system_prompt: "sys".into(),
                ..Default::default()
            },
        );
        Self { agents }
    }
}

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

impl AgentResolver for MockResolver {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, crate::error::RuntimeError> {
        let spec = self
            .agents
            .get(agent_id)
            .ok_or(crate::error::RuntimeError::ResolveFailed {
                message: format!("agent not found: {}", agent_id),
            })?;
        let mut agent = ResolvedAgent::new(
            &spec.id,
            &spec.model,
            &spec.system_prompt,
            Arc::new(MockExecutor),
        );
        agent.env = build_agent_env(&[], &agent).unwrap_or_else(|_| agent.env);
        Ok(agent)
    }
}

// -- Mock Backend --

struct MockBackend {
    result: DelegateRunResult,
}

#[async_trait]
impl AgentBackend for MockBackend {
    async fn execute(
        &self,
        _agent_id: &str,
        _messages: Vec<Message>,
        _event_sink: Arc<dyn EventSink>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        Ok(self.result.clone())
    }
}

struct FailingBackend {
    error: String,
}

#[async_trait]
impl AgentBackend for FailingBackend {
    async fn execute(
        &self,
        _agent_id: &str,
        _messages: Vec<Message>,
        _event_sink: Arc<dyn EventSink>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        Err(AgentBackendError::ExecutionFailed(self.error.clone()))
    }
}

// -- AgentTool with LocalBackend tests --

#[tokio::test]
async fn agent_tool_descriptor_includes_target_id() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "Delegate to worker agent", resolver);
    let desc = tool.descriptor();
    assert_eq!(desc.id, "agent_run_worker");
    assert!(desc.description.contains("worker"));
}

#[tokio::test]
async fn agent_tool_validates_prompt() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);

    let err = tool.validate_args(&json!({}));
    assert!(err.is_err());

    let ok = tool.validate_args(&json!({"prompt": "hello"}));
    assert!(ok.is_ok());
}

#[tokio::test]
async fn agent_tool_execute_runs_sub_agent() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    let ctx = ToolCallContext::test_default();

    let result = tool
        .execute(json!({"prompt": "do work"}), &ctx)
        .await
        .unwrap();

    assert!(result.is_success());
    assert_eq!(result.data["agent_id"], "worker");
    assert!(result.data["status"].as_str().is_some());
    assert!(result.data["steps"].as_u64().is_some());
}

#[tokio::test]
async fn agent_tool_execute_fails_for_missing_agent() {
    let resolver = Arc::new(MockResolver::with_agent("other"));
    let tool = AgentTool::local("missing", "desc", resolver);
    let ctx = ToolCallContext::test_default();

    // Backend error is returned as ToolResult::error (not ToolError)
    let result = tool
        .execute(json!({"prompt": "do work"}), &ctx)
        .await
        .unwrap();
    assert!(!result.is_success());
}

#[tokio::test]
async fn agent_tool_rejects_empty_prompt() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    let ctx = ToolCallContext::test_default();

    let err = tool.execute(json!({"prompt": "   "}), &ctx).await;
    assert!(err.is_err());
}

#[test]
fn agent_tool_agent_id() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    assert_eq!(tool.agent_id(), "worker");
}

// -- AgentTool with mock backend tests --

#[tokio::test]
async fn agent_tool_with_mock_backend_success() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "helper".into(),
            status: DelegateRunStatus::Completed,
            response: Some("done!".into()),
            steps: 3,
        },
    });
    let tool = AgentTool::with_backend("helper", "A helper agent", backend);
    let ctx = ToolCallContext::test_default();

    let result = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(result.is_success());
    assert_eq!(result.data["agent_id"], "helper");
    assert_eq!(result.data["status"], "completed");
    assert_eq!(result.data["response"], "done!");
    assert_eq!(result.data["steps"], 3);
}

#[tokio::test]
async fn agent_tool_with_mock_backend_failure() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "helper".into(),
            status: DelegateRunStatus::Failed("out of memory".into()),
            response: None,
            steps: 0,
        },
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let result = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(result.is_success()); // ToolResult is success but status shows failure
    assert!(result.data["status"].as_str().unwrap().contains("failed"));
}

#[tokio::test]
async fn agent_tool_with_failing_backend() {
    let backend = Arc::new(FailingBackend {
        error: "connection refused".into(),
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let result = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    // Backend errors are returned as ToolResult::error
    assert!(!result.is_success());
    assert!(
        result
            .message
            .as_deref()
            .unwrap()
            .contains("connection refused")
    );
}

// -- AgentTool descriptor format --

#[test]
fn agent_tool_descriptor_format() {
    let resolver = Arc::new(MockResolver::with_agent("researcher"));
    let tool = AgentTool::local("researcher", "Research specialist", resolver);
    let desc = tool.descriptor();

    assert_eq!(desc.id, "agent_run_researcher");
    assert!(desc.description.contains("Research specialist"));
    // Should have proper parameter schema
    assert_eq!(desc.parameters["type"], "object");
    assert!(desc.parameters["properties"]["prompt"].is_object());
}

// -- Validation tests --

#[tokio::test]
async fn agent_tool_validates_empty_object_rejected() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    assert!(tool.validate_args(&json!({})).is_err());
}

#[tokio::test]
async fn agent_tool_validates_non_string_prompt_rejected() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    assert!(tool.validate_args(&json!({"prompt": 42})).is_err());
}

#[tokio::test]
async fn agent_tool_validates_string_prompt_accepted() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    assert!(
        tool.validate_args(&json!({"prompt": "do something"}))
            .is_ok()
    );
}

// -- A2aConfig tests --

#[test]
fn a2a_config_builder() {
    use std::time::Duration;

    let config = A2aConfig::new("https://example.com/a2a")
        .with_bearer_token("tok_123")
        .with_poll_interval(Duration::from_millis(5000))
        .with_timeout(Duration::from_secs(60));

    assert_eq!(config.base_url, "https://example.com/a2a");
    assert_eq!(config.bearer_token.as_deref(), Some("tok_123"));
    assert_eq!(config.poll_interval, Duration::from_millis(5000));
    assert_eq!(config.timeout, Duration::from_secs(60));
}

#[test]
fn a2a_config_default_poll_interval() {
    use std::time::Duration;

    let config = A2aConfig::new("https://api.example.com");
    assert_eq!(config.base_url, "https://api.example.com");
    assert!(config.bearer_token.is_none());
    assert_eq!(config.poll_interval, Duration::from_millis(2000));
    assert_eq!(config.timeout, Duration::from_secs(300));
}

// -- RemoteEndpoint serialization --

#[test]
fn remote_endpoint_serde_roundtrip() {
    let endpoint = RemoteEndpoint {
        base_url: "https://api.example.com".into(),
        bearer_token: Some("tok_123".into()),
        poll_interval_ms: 3000,
        timeout_ms: 60000,
    };
    let json = serde_json::to_string(&endpoint).unwrap();
    let parsed: RemoteEndpoint = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.base_url, "https://api.example.com");
    assert_eq!(parsed.bearer_token.as_deref(), Some("tok_123"));
    assert_eq!(parsed.poll_interval_ms, 3000);
    assert_eq!(parsed.timeout_ms, 60000);
}

#[test]
fn remote_endpoint_defaults() {
    let json = r#"{"base_url": "https://x.com"}"#;
    let endpoint: RemoteEndpoint = serde_json::from_str(json).unwrap();
    assert_eq!(endpoint.base_url, "https://x.com");
    assert!(endpoint.bearer_token.is_none());
    assert_eq!(endpoint.poll_interval_ms, 2000);
    assert_eq!(endpoint.timeout_ms, 300_000);
}

#[test]
fn agent_spec_with_delegate_builder() {
    let spec = AgentSpec::new("main")
        .with_delegate("worker")
        .with_delegate("reviewer");
    assert_eq!(spec.delegates, vec!["worker", "reviewer"]);
}

#[test]
fn agent_spec_with_endpoint_builder() {
    let spec = AgentSpec::new("remote-agent").with_endpoint(RemoteEndpoint {
        base_url: "https://remote.example.com".into(),
        bearer_token: Some("tok_remote_123".into()),
        ..Default::default()
    });
    let ep = spec.endpoint.unwrap();
    assert_eq!(ep.base_url, "https://remote.example.com");
    assert_eq!(ep.bearer_token.as_deref(), Some("tok_remote_123"));
    assert_eq!(ep.poll_interval_ms, 2000);
    assert_eq!(ep.timeout_ms, 300_000);
}

// -- Multiple agent resolver test --

#[test]
fn mock_resolver_with_multiple_agents() {
    let mut agents = std::collections::HashMap::new();
    agents.insert(
        "writer".to_string(),
        AgentSpec {
            id: "writer".into(),
            model: "test-model".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        },
    );
    agents.insert(
        "reviewer".to_string(),
        AgentSpec {
            id: "reviewer".into(),
            model: "test-model".into(),
            system_prompt: "sys".into(),
            ..Default::default()
        },
    );
    let resolver = MockResolver { agents };

    assert!(resolver.resolve("writer").is_ok());
    assert!(resolver.resolve("reviewer").is_ok());
    assert!(resolver.resolve("nonexistent").is_err());
}

// -- AgentTool result structure --

#[tokio::test]
async fn agent_tool_result_structure() {
    let resolver = Arc::new(MockResolver::with_agent("analyst"));
    let tool = AgentTool::local("analyst", "Data analyst", resolver);
    let ctx = ToolCallContext::test_default();

    let result = tool
        .execute(json!({"prompt": "analyze data"}), &ctx)
        .await
        .unwrap();

    assert!(result.is_success());
    assert_eq!(result.data["agent_id"], "analyst");
    assert!(result.data["response"].as_str().is_some());
}

// -- DelegateRunStatus display --

#[test]
fn delegate_run_status_display() {
    assert_eq!(DelegateRunStatus::Completed.to_string(), "completed");
    assert_eq!(DelegateRunStatus::Cancelled.to_string(), "cancelled");
    assert_eq!(DelegateRunStatus::Timeout.to_string(), "timeout");
    assert!(
        DelegateRunStatus::Failed("oops".into())
            .to_string()
            .contains("oops")
    );
}

// -- AgentBackendError display --

#[test]
fn agent_backend_error_display() {
    let err = AgentBackendError::AgentNotFound("x".into());
    assert!(err.to_string().contains("agent not found"));

    let err = AgentBackendError::ExecutionFailed("boom".into());
    assert!(err.to_string().contains("execution failed"));

    let err = AgentBackendError::RemoteError("timeout".into());
    assert!(err.to_string().contains("remote error"));
}
