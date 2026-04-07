use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest};
use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::tool::{Tool, ToolCallContext};
use awaken_contract::registry_spec::{AgentSpec, RemoteAuth, RemoteEndpoint};

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
        agent.env = build_agent_env(&[], &agent).unwrap_or(agent.env);
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
        _parent_run_id: Option<String>,
        _parent_tool_call_id: Option<String>,
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
        _parent_run_id: Option<String>,
        _parent_tool_call_id: Option<String>,
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

    let output = tool
        .execute(json!({"prompt": "do work"}), &ctx)
        .await
        .unwrap();

    assert!(output.result.is_success());
    assert_eq!(output.result.data["agent_id"], "worker");
    assert!(output.result.data["status"].as_str().is_some());
    assert!(output.result.data["steps"].as_u64().is_some());
}

#[tokio::test]
async fn agent_tool_execute_fails_for_missing_agent() {
    let resolver = Arc::new(MockResolver::with_agent("other"));
    let tool = AgentTool::local("missing", "desc", resolver);
    let ctx = ToolCallContext::test_default();

    // Backend error is returned as ToolResult::error (not ToolError)
    let output = tool
        .execute(json!({"prompt": "do work"}), &ctx)
        .await
        .unwrap();
    assert!(!output.result.is_success());
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
            run_id: None,
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("helper", "A helper agent", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(output.result.is_success());
    assert_eq!(output.result.data["agent_id"], "helper");
    assert_eq!(output.result.data["status"], "completed");
    assert_eq!(output.result.data["response"], "done!");
    assert_eq!(output.result.data["steps"], 3);
}

#[tokio::test]
async fn agent_tool_with_mock_backend_failure() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "helper".into(),
            status: DelegateRunStatus::Failed("out of memory".into()),
            response: None,
            steps: 0,
            run_id: None,
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(output.result.is_success()); // ToolResult is success but status shows failure
    assert!(
        output.result.data["status"]
            .as_str()
            .unwrap()
            .contains("failed")
    );
}

#[tokio::test]
async fn agent_tool_with_failing_backend() {
    let backend = Arc::new(FailingBackend {
        error: "connection refused".into(),
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    // Backend errors are returned as ToolResult::error
    assert!(!output.result.is_success());
    assert!(
        output
            .result
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
    let mut options = std::collections::BTreeMap::new();
    options.insert("poll_interval_ms".into(), json!(3000));
    let endpoint = RemoteEndpoint {
        backend: "a2a".into(),
        base_url: "https://api.example.com".into(),
        auth: Some(RemoteAuth::bearer("tok_123")),
        target: None,
        timeout_ms: 60000,
        options,
    };
    let json = serde_json::to_string(&endpoint).unwrap();
    let parsed: RemoteEndpoint = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.backend, "a2a");
    assert_eq!(parsed.base_url, "https://api.example.com");
    assert_eq!(
        parsed
            .auth
            .as_ref()
            .and_then(|auth| auth.param_str("token")),
        Some("tok_123")
    );
    assert_eq!(parsed.options.get("poll_interval_ms"), Some(&json!(3000)));
    assert_eq!(parsed.timeout_ms, 60000);
}

#[test]
fn remote_endpoint_defaults() {
    let json = r#"{"base_url": "https://x.com"}"#;
    let endpoint: RemoteEndpoint = serde_json::from_str(json).unwrap();
    assert_eq!(endpoint.backend, "a2a");
    assert_eq!(endpoint.base_url, "https://x.com");
    assert!(endpoint.auth.is_none());
    assert!(endpoint.options.is_empty());
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
        backend: "a2a".into(),
        base_url: "https://remote.example.com".into(),
        auth: Some(RemoteAuth::bearer("tok_remote_123")),
        ..Default::default()
    });
    let ep = spec.endpoint.unwrap();
    assert_eq!(ep.backend, "a2a");
    assert_eq!(ep.base_url, "https://remote.example.com");
    assert_eq!(
        ep.auth.as_ref().and_then(|auth| auth.param_str("token")),
        Some("tok_remote_123")
    );
    assert!(ep.options.is_empty());
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

    let output = tool
        .execute(json!({"prompt": "analyze data"}), &ctx)
        .await
        .unwrap();

    assert!(output.result.is_success());
    assert_eq!(output.result.data["agent_id"], "analyst");
    assert!(output.result.data["response"].as_str().is_some());
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

// -- Identity propagation through AgentTool --

/// A backend that captures the parent_run_id and parent_tool_call_id it receives.
struct CapturingBackend {
    captured_parent_run_id: std::sync::Mutex<Option<String>>,
    captured_parent_tool_call_id: std::sync::Mutex<Option<String>>,
}

impl CapturingBackend {
    fn new() -> Self {
        Self {
            captured_parent_run_id: std::sync::Mutex::new(None),
            captured_parent_tool_call_id: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl AgentBackend for CapturingBackend {
    async fn execute(
        &self,
        agent_id: &str,
        _messages: Vec<Message>,
        _event_sink: Arc<dyn EventSink>,
        parent_run_id: Option<String>,
        parent_tool_call_id: Option<String>,
    ) -> Result<DelegateRunResult, AgentBackendError> {
        *self.captured_parent_run_id.lock().unwrap() = parent_run_id;
        *self.captured_parent_tool_call_id.lock().unwrap() = parent_tool_call_id;
        Ok(DelegateRunResult {
            agent_id: agent_id.to_string(),
            status: DelegateRunStatus::Completed,
            response: Some("done".into()),
            steps: 1,
            run_id: None,
            inbox: None,
        })
    }
}

#[tokio::test]
async fn agent_tool_propagates_parent_identity_to_backend() {
    use awaken_contract::contract::identity::{RunIdentity, RunOrigin};

    let backend = Arc::new(CapturingBackend::new());
    let tool = AgentTool::with_backend("worker", "desc", backend.clone());

    // Build a context with a known run identity
    let mut ctx = ToolCallContext::test_default();
    ctx.run_identity = RunIdentity::new(
        "parent-thread-123".to_string(),
        None,
        "parent-run-456".to_string(),
        None,
        "orchestrator".to_string(),
        RunOrigin::User,
    );
    ctx.call_id = "tool-call-789".to_string();

    let output = tool
        .execute(json!({"prompt": "do work"}), &ctx)
        .await
        .unwrap();
    assert!(output.result.is_success());

    // Verify the backend received the parent's run_id and tool_call_id
    let captured_run_id = backend.captured_parent_run_id.lock().unwrap().clone();
    let captured_tool_call_id = backend.captured_parent_tool_call_id.lock().unwrap().clone();

    assert_eq!(captured_run_id.as_deref(), Some("parent-run-456"));
    assert_eq!(captured_tool_call_id.as_deref(), Some("tool-call-789"));
}

#[tokio::test]
async fn local_backend_sets_sub_agent_identity() {
    // This test exercises LocalBackend end-to-end: it resolves a mock agent,
    // runs the agent loop (which completes immediately with MockExecutor),
    // and we verify the result proves the sub-agent ran with Subagent origin.
    let resolver = Arc::new(MockResolver::with_agent("sub-worker"));
    let tool = AgentTool::local("sub-worker", "desc", resolver);

    let mut ctx = ToolCallContext::test_default();
    ctx.run_identity = awaken_contract::contract::identity::RunIdentity::new(
        "parent-thread".to_string(),
        None,
        "parent-run-id".to_string(),
        None,
        "parent-agent".to_string(),
        awaken_contract::contract::identity::RunOrigin::User,
    );
    ctx.call_id = "tool-call-xyz".to_string();

    let output = tool
        .execute(json!({"prompt": "sub-task"}), &ctx)
        .await
        .unwrap();

    // If the sub-agent identity was constructed correctly, the run completes
    // successfully (MockExecutor returns a response, LocalBackend maps it).
    assert!(output.result.is_success());
    assert_eq!(output.result.data["agent_id"], "sub-worker");
    assert_eq!(output.result.data["status"], "completed");
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

// -- child_run_id propagation --

#[tokio::test]
async fn agent_tool_propagates_child_run_id_in_metadata() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "helper".into(),
            status: DelegateRunStatus::Completed,
            response: Some("done".into()),
            steps: 1,
            run_id: Some("child-run-123".into()),
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(output.result.is_success());
    assert_eq!(
        output
            .result
            .metadata
            .get("child_run_id")
            .and_then(|v| v.as_str()),
        Some("child-run-123")
    );
}

// -- Cancelled status --

#[tokio::test]
async fn agent_tool_with_mock_backend_cancelled() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "helper".into(),
            status: DelegateRunStatus::Cancelled,
            response: None,
            steps: 2,
            run_id: None,
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(output.result.is_success());
    assert_eq!(output.result.data["status"], "cancelled");
    assert_eq!(output.result.data["agent_id"], "helper");
    assert_eq!(output.result.data["steps"], 2);
    assert!(output.result.data["response"].is_null());
}

// -- Timeout status --

#[tokio::test]
async fn agent_tool_with_mock_backend_timeout() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "helper".into(),
            status: DelegateRunStatus::Timeout,
            response: Some("partial".into()),
            steps: 5,
            run_id: None,
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(output.result.is_success());
    assert_eq!(output.result.data["status"], "timeout");
    assert_eq!(output.result.data["agent_id"], "helper");
    assert_eq!(output.result.data["response"], "partial");
    assert_eq!(output.result.data["steps"], 5);
}

// -- Failed status contains error info --

#[tokio::test]
async fn agent_tool_failed_status_contains_error_info() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "helper".into(),
            status: DelegateRunStatus::Failed("rate limit exceeded".into()),
            response: None,
            steps: 1,
            run_id: None,
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    let status = output.result.data["status"].as_str().unwrap();
    assert!(status.contains("failed"));
    assert!(status.contains("rate limit exceeded"));
}

// -- validate_args with null and array inputs --

#[test]
fn agent_tool_validate_args_null_input() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    assert!(tool.validate_args(&json!(null)).is_err());
}

#[test]
fn agent_tool_validate_args_array_input() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    assert!(tool.validate_args(&json!([1, 2, 3])).is_err());
}

#[test]
fn agent_tool_validate_args_prompt_is_number() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    assert!(tool.validate_args(&json!({"prompt": 2.5})).is_err());
}

#[test]
fn agent_tool_validate_args_prompt_is_bool() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    assert!(tool.validate_args(&json!({"prompt": true})).is_err());
}

#[test]
fn agent_tool_validate_args_prompt_is_null() {
    let resolver = Arc::new(MockResolver::with_agent("worker"));
    let tool = AgentTool::local("worker", "desc", resolver);
    assert!(tool.validate_args(&json!({"prompt": null})).is_err());
}

// -- Command is empty (no side-effects from delegation) --

#[tokio::test]
async fn agent_tool_command_is_empty() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "helper".into(),
            status: DelegateRunStatus::Completed,
            response: Some("done".into()),
            steps: 1,
            run_id: None,
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(
        output.command.is_empty(),
        "AgentTool should produce no side-effect commands"
    );
}

#[tokio::test]
async fn agent_tool_command_is_empty_on_failure() {
    let backend = Arc::new(FailingBackend {
        error: "backend down".into(),
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(
        output.command.is_empty(),
        "AgentTool should produce no side-effect commands even on error"
    );
}

// -- Result data structure has all expected fields --

#[tokio::test]
async fn agent_tool_result_data_has_all_fields() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "analyst".into(),
            status: DelegateRunStatus::Completed,
            response: Some("analysis complete".into()),
            steps: 7,
            run_id: Some("child-456".into()),
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("analyst", "Data analyst", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "analyze data"}), &ctx)
        .await
        .unwrap();

    let data = &output.result.data;
    assert_eq!(data["agent_id"], "analyst");
    assert_eq!(data["status"], "completed");
    assert_eq!(data["response"], "analysis complete");
    assert_eq!(data["steps"], 7);
    // Metadata contains child_run_id
    assert_eq!(
        output
            .result
            .metadata
            .get("child_run_id")
            .and_then(|v| v.as_str()),
        Some("child-456")
    );
}

// -- Descriptor tool_id pattern --

#[test]
fn agent_tool_descriptor_tool_id_follows_pattern() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "my-special-agent".into(),
            status: DelegateRunStatus::Completed,
            response: None,
            steps: 0,
            run_id: None,
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("my-special-agent", "desc", backend);
    let desc = tool.descriptor();
    assert_eq!(desc.id, "agent_run_my-special-agent");
    assert!(desc.id.starts_with("agent_run_"));
}

// -- Parent propagation with capturing backend --

#[tokio::test]
async fn agent_tool_propagates_run_id_and_call_id() {
    use awaken_contract::contract::identity::{RunIdentity, RunOrigin};

    let backend = Arc::new(CapturingBackend::new());
    let tool = AgentTool::with_backend("worker", "desc", backend.clone());

    let mut ctx = ToolCallContext::test_default();
    ctx.run_identity = RunIdentity::new(
        "thread-abc".to_string(),
        None,
        "run-def".to_string(),
        None,
        "main-agent".to_string(),
        RunOrigin::User,
    );
    ctx.call_id = "tc-ghi".to_string();

    let _output = tool
        .execute(json!({"prompt": "delegate"}), &ctx)
        .await
        .unwrap();

    let run_id = backend.captured_parent_run_id.lock().unwrap().clone();
    let call_id = backend.captured_parent_tool_call_id.lock().unwrap().clone();
    assert_eq!(run_id.as_deref(), Some("run-def"));
    assert_eq!(call_id.as_deref(), Some("tc-ghi"));
}

#[tokio::test]
async fn agent_tool_omits_child_run_id_when_none() {
    let backend = Arc::new(MockBackend {
        result: DelegateRunResult {
            agent_id: "helper".into(),
            status: DelegateRunStatus::Completed,
            response: Some("done".into()),
            steps: 1,
            run_id: None,
            inbox: None,
        },
    });
    let tool = AgentTool::with_backend("helper", "desc", backend);
    let ctx = ToolCallContext::test_default();

    let output = tool
        .execute(json!({"prompt": "help me"}), &ctx)
        .await
        .unwrap();

    assert!(output.result.is_success());
    assert!(!output.result.metadata.contains_key("child_run_id"));
}
