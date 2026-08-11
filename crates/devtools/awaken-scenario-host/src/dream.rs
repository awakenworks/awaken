//! Deterministic Dream scenario platform shared by Rust and TypeScript E2E.
//!
//! The scenario uses the production MemoryStore mounter and a scripted model that
//! performs a real output-file write through the ordinary tool loop.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::routing::get;

use crate::{SharedHost, build_router_and_host};

/// Build the one Dream scenario router used by process-level SDK E2E.
pub fn build_dream_router() -> Router {
    build_dream_router_and_host().0
}

/// Return the same router plus its Host for focused cross-module assertions.
pub fn build_dream_router_and_host() -> (Router, Arc<SharedHost>) {
    let (router, host) = build_router_and_host(Arc::new(DreamScenarioModel), "claude-sonnet-5");
    let capabilities = awaken_config_service::capabilities_router(
        awaken_runtime_host::authorable_tools(),
        awaken_runtime_host::platform_plugin_capabilities(),
        vec![awaken_config_service::PolicyCapability::new(
            "permission",
            awaken_ext_permission::permission_config_schema(),
        )],
        vec![awaken_config_service::RuntimeCapability::native()],
    );
    let console_context = Router::new()
        .route(
            "/.well-known/awaken-suite-navigation",
            get(|| async { Json(serde_json::json!({ "hub_url": null })) }),
        )
        .route(
            "/v1/session",
            get(|| async { Json(serde_json::json!({ "authenticated": true })) }),
        )
        .route(
            "/v1/config/workspace-context",
            get(|| async { Json(serde_json::json!({ "workspace_id": "default" })) }),
        );
    (router.merge(capabilities).merge(console_context), host)
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
