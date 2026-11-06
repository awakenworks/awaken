// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: This example composes the actual SDK Runtime and public wire
// @formal reason: adapters without constructing Session or Sandbox authorities.
//! Run with `cargo run -p awaken-sdk-server -- 127.0.0.1:3030`.
//!
//! The deterministic echo model keeps the example offline. Replace that one
//! executor with a provider to serve a model; the Runtime and routers stay the
//! same. MCP exports are deliberately empty until the host selects its tools.
use std::sync::Arc;

use awaken_conversation::ConversationRuns;
use awaken_runtime::model::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime::{ExecutableAgentSnapshot, ModelBinding, Runtime, RuntimeRunContext};
use awaken_store_inmem::MemoryCommitCoordinator;

struct Echo;
#[async_trait::async_trait]
impl LlmExecutor for Echo {
    async fn infer(&self, request: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        let text = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == awaken_agent_contract::agent::message::Role::User)
            .map(|message| message.text_content())
            .unwrap_or_default();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("Echo: {text}")),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:3030".into());
    let clock = awaken_time::system_clock();
    let store = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
    let snapshot = ExecutableAgentSnapshot::builder("echo")
        .model(ModelBinding::parse("demo", "stub", "stub")?)
        .build();
    let runtime = Arc::new(
        Runtime::new()
            .with_clock(clock.clone())
            .with_llm(Arc::new(Echo)),
    );
    let runs = Arc::new(ConversationRuns::new(
        runtime,
        snapshot,
        RuntimeRunContext::new().with_store(store),
        clock.clone(),
    )?);
    let mcp = Arc::new(awaken_protocol_mcp::McpToolService::new(
        "sdk",
        env!("CARGO_PKG_VERSION"),
        Arc::new(awaken_protocol_mcp::StaticExports::new(Vec::new())),
    ));
    let router = axum::Router::new()
        .merge(awaken_protocol_ag_ui::router(runs.clone(), "local".into()))
        .merge(awaken_protocol_ai_sdk::router(runs.clone(), "local".into()))
        .merge(awaken_protocol_a2a::router(runs, clock))
        .merge(awaken_protocol_mcp::router(
            mcp,
            awaken_protocol_mcp::McpHttpConfig::default(),
        ));
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("SDK protocols listening on {}", listener.local_addr()?);
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
