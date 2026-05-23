#![allow(missing_docs)]

use async_trait::async_trait;
use awaken::contract::content::ContentBlock;
use awaken::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken::contract::inference::{StopReason, StreamResult};
use awaken::contract::lifecycle::TerminationReason;
use awaken::contract::message::{Message, ToolCall};
use awaken::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
};
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, RunActivation};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct ScriptedProvider {
    responses: Mutex<Vec<StreamResult>>,
    upstream_models: Mutex<Vec<String>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: Mutex::new(responses),
            upstream_models: Mutex::new(Vec::new()),
        }
    }

    fn upstream_models(&self) -> Vec<String> {
        self.upstream_models.lock().expect("lock poisoned").clone()
    }
}

#[async_trait]
impl LlmExecutor for ScriptedProvider {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.upstream_models
            .lock()
            .expect("lock poisoned")
            .push(request.upstream_model);
        let mut responses = self.responses.lock().expect("lock poisoned");
        Ok(responses.remove(0))
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

struct EchoTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for EchoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new("echo", "Echo", "Echo input back to the caller").with_parameters(
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        )
    }

    async fn execute(&self, args: Value, _ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = args["text"].as_str().unwrap_or_default();
        Ok(ToolResult::success("echo", json!({ "echoed": text })).into())
    }
}

#[tokio::test]
async fn readme_quickstart_runs_end_to_end_without_streaming_events() {
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(ScriptedProvider::new(vec![
        StreamResult {
            content: vec![ContentBlock::text("I'll call echo.")],
            tool_calls: vec![ToolCall::new("echo-1", "echo", json!({"text": "hello"}))],
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
            has_incomplete_tool_calls: false,
        },
        StreamResult {
            content: vec![ContentBlock::text("Echoed hello.")],
            tool_calls: vec![],
            usage: None,
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        },
    ]));

    let agent_spec = AgentSpec::new("assistant")
        .with_model_id("gpt-4o-mini")
        .with_system_prompt("You are a helpful assistant. Use the echo tool when asked.")
        .with_max_rounds(5);

    let runtime = AgentRuntimeBuilder::new()
        .with_agent_spec(agent_spec)
        .with_tool(
            "echo",
            Arc::new(EchoTool {
                calls: tool_calls.clone(),
            }),
        )
        .with_provider("openai", provider.clone())
        .with_model_binding(
            "gpt-4o-mini",
            ModelBinding {
                provider_id: "openai".into(),
                upstream_model: "gpt-4o-mini".into(),
            },
        )
        .build()
        .expect("quickstart runtime should build");

    let request = RunActivation::new(
        "thread-1",
        vec![Message::user("Say hello using the echo tool")],
    )
    .with_agent_id("assistant");

    let result = runtime
        .run_to_completion(request)
        .await
        .expect("quickstart run should succeed");

    assert_eq!(result.response, "Echoed hello.");
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.steps, 2);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.upstream_models(),
        vec!["gpt-4o-mini".to_string(), "gpt-4o-mini".to_string()]
    );
}
