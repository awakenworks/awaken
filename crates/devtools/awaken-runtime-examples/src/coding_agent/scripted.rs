//! A deterministic stand-in model for offline tests: it drives a fixed
//! read → edit → reply sequence on one file, so the smoke test proves the agent
//! actually reads and edits code (and that the edit awaits for approval) without a
//! network or an API key.

use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};

/// Scripts the three model turns of one edit: read the file, replace `old` with
/// `new`, then report. Each `infer` advances one step.
pub struct ScriptedCoder {
    path: String,
    old: String,
    new: String,
    step: AtomicUsize,
}

impl ScriptedCoder {
    pub fn new(path: impl Into<String>, old: impl Into<String>, new: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            old: old.into(),
            new: new.into(),
            step: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmExecutor for ScriptedCoder {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.step.fetch_add(1, Ordering::SeqCst);
        let output = match n {
            0 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "read-1".to_string(),
                tool_id: "read".to_string(),
                arguments: serde_json::json!({ "path": self.path }),
            }]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "edit-1".to_string(),
                tool_id: "edit".to_string(),
                arguments: serde_json::json!({
                    "path": self.path,
                    "old": self.old,
                    "new": self.new,
                }),
            }]),
            _ => AssistantOutput::text(format!("Replaced `{}` with `{}`.", self.old, self.new)),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}
