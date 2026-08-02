//! The async agent execution loop.
//!
//! `execute` resolves the activation, runs the model/tool step loop, stages a
//! `ThreadCommit`, and commits durable truth. A run can await on a gate `Suspend`
//! by committing a `ResumeTicket`, then `resume` validates a `ResumeCommand`
//! against it and continues. Live progress is best-effort and never the replay
//! source (G1/G13).

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::{AwaitReason, PendingTool, ResumeTicket};
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::delegation::{
    DelegationId, DelegationOrigin, DelegationRegistry, RequestDelegation,
};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{
    Action as StateAction, Command as StateCommand, Scope, StateKey, Store, validate_batch,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft as EventDraft;
use awaken_agent_contract::audit::run_event::RunEvent;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::stream::checkpoint::{
    PartialToolCall, StreamCheckpoint, StreamCheckpointStore,
};
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::boundary::{BoundaryOutcome, evaluate_boundary};
use awaken_runtime_contract::delegation::{
    ChildRunResult, DelegationExecutionError, DelegationRequest, DelegationResume, DelegationStep,
    PendingChildRunResults, ResultRecord, RunDelegations,
};
use awaken_runtime_contract::execution::{Error, Result, RunAttemptExecutor, RunExecutor};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatMessage, ChatRequest, ChatResponse, DeltaSink, StopReason, ThreadUsage,
    ThreadUsageKey, ToolCall,
};
use awaken_runtime_contract::permission::GateOutcome;
use awaken_runtime_contract::plugin::{
    AfterToolContext, ContextMessages, ContextWindow, PhaseContext, PhaseHookPoint, PhaseKind,
    ResolvedExecutionEnv, RunEndContext, RunEndDecision,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ResolvedRun, ToolKind, ToolPresentation,
};
use awaken_runtime_contract::resolver::{self, RunResolver};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult, validate_resume};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshotId;
use awaken_runtime_contract::tool::{
    ToolError, ToolExecutionTarget, ToolExecutor, ToolOperationContext, ToolOutput,
    with_tool_operation_context,
};
use awaken_runtime_contract::tool::{ToolRecoveryCapability, ToolRecoveryMode, ToolRecoveryPolicy};
use awaken_runtime_contract::tool_batch::{
    ActiveToolBatch, ToolBatch, ToolBatchId, ToolBatchPhase, ToolCallPhase, ToolWaitKind,
};

use crate::runtime::Runtime;

mod content;
mod convert;
mod delegation;
mod dispatch;
mod finalize;
mod inference;
mod progress;
mod resume;
pub(crate) mod run_commands;
mod run_loop;
mod tool_execution;
pub(crate) use convert::*;
pub(crate) use delegation::reconcile_delegation_cancellations;
use delegation::{
    DelegationInvocation, DelegationParent, delegation_error_output, invoke_delegation,
    persist_child_run_result, resume_delegation, run_delegation, stage_delegation_awaiting,
    stage_delegation_completed, stage_delegation_request, stage_delegation_requests,
};
use finalize::{finalize, finish};
use inference::*;
use progress::*;
use resume::drive_resumed;
use run_loop::*;
use tool_execution::*;

/// Best-effort live emission. A sink failure is swallowed: committed truth is
/// authoritative, not the live stream (G10/G13).
async fn emit(context: &RuntimeRunContext, run_id: &RunId, kind: AgentEvent) {
    if let Some(sink) = &context.stream_sink {
        let _ = sink
            .send(StreamEvent {
                run_id: run_id.clone(),
                kind,
            })
            .await;
    }
}

fn map_resolver_error(err: resolver::Error) -> Error {
    Error::Resolution(err.to_string())
}

#[cfg(test)]
mod tests;
