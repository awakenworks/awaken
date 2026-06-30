//! Reusable stub adapters for the runnable examples. The examples themselves are
//! the teaching artifacts (`examples/*.rs`); this crate only holds the small,
//! deterministic ports they wire so each example stays focused on *assembly*.

use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

/// A deterministic stand-in for a model: it calls the `echo` tool once, then ends
/// with text. Swap `ScriptedLlm` for `awaken_provider_genai::GenAiExecutor::new()`
/// to run against a real model (set the provider API key in the environment) —
/// the runtime sees only the `LlmExecutor` port either way.
#[derive(Default)]
pub struct ScriptedLlm {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "call-1".to_string(),
                tool_id: "echo".to_string(),
                arguments: serde_json::json!({"text": "hello"}),
            }])
        } else {
            AssistantOutput::text("All done.".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

/// A trivial tool that echoes its `text` argument — a stand-in for a real tool
/// (the runtime sees only the `RawTool` port).
pub struct EchoTool;

#[async_trait::async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let text = call
            .arguments
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(ToolOutput::ok(call.call_id, format!("echoed: {text}")))
    }
}
