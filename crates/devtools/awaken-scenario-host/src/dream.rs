//! Deterministic Dream scenario composition shared by Rust and TypeScript E2E.
//!
//! The scenario uses the production MemoryStore mounter and a scripted model that
//! performs a real output-file write through the ordinary tool loop.

use std::sync::Arc;

use axum::Router;

use crate::{SharedHost, build_router_and_host};

/// Build the one Dream scenario router used by process-level SDK E2E.
pub fn build_dream_router() -> Router {
    build_dream_router_and_host().0
}

/// Return the same router plus its Host for focused cross-module assertions.
pub fn build_dream_router_and_host() -> (Router, Arc<SharedHost>) {
    build_router_and_host(Arc::new(DreamScenarioModel), "claude-sonnet-5")
}

struct DreamScenarioModel;

#[async_trait::async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for DreamScenarioModel {
    async fn infer(
        &self,
        request: awaken_runtime_contract::llm::ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse> {
        use awaken_agent_contract::agent::message::Role;
        use awaken_runtime_contract::llm::{AssistantOutput, ChatResponse, ToolCall};

        let is_dream = request.messages.iter().any(|message| {
            message.role == Role::User
                && awaken_runtime_host::block_text(&message.content).contains("[dream-job:")
        });
        if !is_dream {
            return awaken_runtime_contract::llm::LlmExecutor::infer(&crate::EchoModel, request)
                .await;
        }
        if request
            .messages
            .last()
            .is_some_and(|message| message.role == Role::Tool)
        {
            return Ok(ChatResponse {
                output: AssistantOutput::text("Dream consolidation written."),
                usage: None,
                stop_reason: None,
            });
        }
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "dream-write-through-1".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({
                    "path": "/mnt/dream/output-memory/MEMORY.md",
                    "content": "# Dream\n- Consolidated by the Dream Agent.\n"
                }),
            }]),
            usage: None,
            stop_reason: None,
        })
    }
}
