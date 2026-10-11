//! An application-owned model adapter, using only the public SDK port.
use awaken_runtime::model::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};

pub struct DemoModel;

#[async_trait::async_trait]
impl LlmExecutor for DemoModel {
    async fn infer(&self, _: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("Hello from the SDK"),
            usage: None,
            stop_reason: None,
        })
    }
}
