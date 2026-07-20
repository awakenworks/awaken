//! Reusable stub adapters for the snapshot examples. The examples themselves are
//! the teaching artifacts (`examples/*.rs`); this crate only holds the small,
//! deterministic ports they wire so each example stays focused on *assembly*.
//!
//! The `coding-agent` feature adds a fuller example: a coding agent assembled from
//! the built-in tools, the permission gate, and `ExecutableAgentSnapshot` (see
//! [`coding_agent`]).

#[cfg(feature = "coding-agent")]
pub mod coding_agent;

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
            stop_reason: None,
        })
    }
}

/// The simplest possible model: it replies with one fixed greeting and ends —
/// no tool call. `hello_agent` uses it for the minimal "instructions + model,
/// no tools, no permissions" path.
pub struct GreeterLlm;

#[async_trait::async_trait]
impl LlmExecutor for GreeterLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("Hello! How can I help?".to_string()),
            usage: None,
            stop_reason: None,
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

/// One import for the example assembly. The examples teach *wiring*, not where
/// each type lives, so the deep contract paths are noise — this gathers them
/// (plus the stub ports above) so an example reads as assembly, not imports.
pub mod prelude {
    pub use awaken_agent_contract::agent::content::ContentBlock;
    pub use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    pub use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
    pub use awaken_agent_contract::agent::thread::Id as ThreadId;
    pub use awaken_ext_permission::{
        Mode, PermissionRule, PermissionRuleset, RuleBasedToolPermissionPolicy, ToolCallPattern,
        ToolPermissionBehavior,
    };
    pub use awaken_runtime::memory::MemoryCommitCoordinator;
    pub use awaken_runtime::{PermissionGate, RunInput, Runtime};
    pub use awaken_runtime_contract::activation::RunActivation;
    pub use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
    pub use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
    pub use awaken_runtime_contract::execution::RunExecutor;
    pub use awaken_runtime_contract::resolved::{
        CatalogFingerprint, ModelBinding, ResolvedSpec, ToolDescriptor,
    };
    pub use awaken_runtime_contract::runtime_context::RuntimeRunContext;
    pub use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshotId};
    pub use awaken_runtime_contract::{ExecutableAgentSnapshot, ExecutableAgentSnapshotBuilder};

    pub use crate::{EchoTool, GreeterLlm, ScriptedLlm};
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            tool_id: "echo".to_string(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn echo_returns_the_text_argument() {
        let out = EchoTool
            .invoke(call(serde_json::json!({ "text": "hi" })))
            .await
            .unwrap();
        assert_eq!(out.content, "echoed: hi");
        assert_eq!(out.call_id, "c1", "the call id is echoed back");
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn echo_defaults_to_empty_when_text_is_missing_or_not_a_string() {
        // Missing key → the fail-safe empty default, not an error.
        let missing = EchoTool.invoke(call(serde_json::json!({}))).await.unwrap();
        assert_eq!(missing.content, "echoed: ");
        // Present but not a string → also the empty default (as_str is None).
        let non_string = EchoTool
            .invoke(call(serde_json::json!({ "text": 42 })))
            .await
            .unwrap();
        assert_eq!(non_string.content, "echoed: ");
    }

    #[tokio::test]
    async fn scripted_llm_calls_the_echo_tool_first_then_ends_with_text() {
        use awaken_runtime_contract::llm::ChatRequest;
        use awaken_runtime_contract::resolved::ModelBinding;
        let llm = ScriptedLlm::default();
        let request = || ChatRequest {
            model_binding: ModelBinding::new("x", "y", "z"),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        // First inference: one echo tool call, no terminal text.
        let first = llm.infer(request()).await.unwrap();
        assert_eq!(first.output.tool_calls().len(), 1);
        assert_eq!(first.output.tool_calls()[0].tool_id, "echo");
        // Second inference: the closing text turn (no tool calls).
        let second = llm.infer(request()).await.unwrap();
        assert!(second.output.tool_calls().is_empty());
        assert_eq!(second.output.text_content(), "All done.");
    }
}
