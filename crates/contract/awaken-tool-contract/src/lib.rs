//! Tool vocabulary and ports: tool implementations, the executor and admission
//! ports, the durable tool-call batch, and the tool-discovery input.
//!
//! `awaken-runtime-contract` re-exports this crate under its historical module
//! paths, so consumers may name either path while they migrate.

pub mod tool;
pub mod tool_batch;
pub mod tool_discovery;

pub use tool::{
    RawTool, RawToolRegistry, Tool, ToolConcurrency, ToolExecutionAdmission, ToolExecutionPermit,
    ToolExecutionTarget, ToolExecutionWave, ToolExecutor, ToolOperationToken, ToolOutput,
    ToolRecoveryCapability, ToolRecoveryMode, ToolRecoveryPolicy, ToolResource, ToolResourceAccess,
    ToolTaskHandle, ToolTaskPoll, ToolTaskStart, tool_execution_waves,
};
pub use tool_batch::{
    ActiveToolBatch, ToolBatch, ToolBatchId, ToolBatchPhase, ToolCallPhase, ToolWait, ToolWaitKind,
};
pub use tool_discovery::{ToolSearchInput, ToolSearchLimit, ToolSearchLimitError};
