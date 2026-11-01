//! Tool vocabulary and ports: tool implementations, the executor and admission
//! ports, the durable tool-call batch, and the tool-discovery input.
//!
//! Consumers name these types and ports directly through this defining crate.
//!
//! # Boundary
//!
//! Context: `agent-runtime`.
//!
//! Published SDK contract (ADR-0086): every context may read it, so its readers are not listed
//! here; `cargo tree -e normal -i awaken-tool-contract` prints them.

pub mod tool;
pub mod tool_batch;
pub mod tool_configuration;
pub mod tool_discovery;
pub use tool_configuration::{
    ToolConfigurationCompiler, ToolConfigurationError, ToolConfigurationInput,
    compile_toolset_configurations,
};
pub mod typed;

pub use tool::{
    DurableTaskExecutor, DurableTaskTool, RawTool, RawToolRegistry, Tool, ToolConcurrency,
    ToolExecutionAdmission, ToolExecutionPermit, ToolExecutionTarget, ToolExecutionWave,
    ToolExecutor, ToolOperationToken, ToolOutput, ToolRecoveryCapability, ToolRecoveryMode,
    ToolRecoveryPolicy, ToolResource, ToolResourceAccess, ToolTaskContinuation, ToolTaskHandle,
    ToolTaskPoll, ToolTaskStart, start_executor_task, start_tool_task, tool_execution_waves,
};
pub use tool_batch::{
    ActiveToolBatch, ToolBatch, ToolBatchId, ToolBatchPhase, ToolCallPhase, ToolWait, ToolWaitKind,
};
pub use tool_discovery::{ToolSearchInput, ToolSearchLimit, ToolSearchLimitError};
