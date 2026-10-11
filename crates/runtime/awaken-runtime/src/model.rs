//! The model adapter port and its inputs, outputs and stream callback.
//! These are the canonical contract types, with no second DTO or conversion.
pub use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, DeltaSink, Error, LlmExecutor, Result, StopReason,
    ToolCall,
};
