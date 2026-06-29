//! `RunWithSnapshotExecutor`: the runtime-facing entry that accepts an inline
//! executable snapshot or an executable snapshot id and submits execution.
//!
//! `AgentId` alone is never a complete run configuration (G28); a command always
//! carries an `AgentSnapshotInput` that the runtime validates before execution.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::activation::{RunOptions, TraceContext};
use crate::execution::{Error, RunOutcome};
use crate::runtime_context::RuntimeRunContext;
use crate::snapshot::AgentSnapshotInput;

/// Neutral run input where the configuration identity is a snapshot *input*
/// (inline or by id), not a pinned snapshot. Pure data (G3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunWithSnapshotCommand {
    pub run_id: awaken_agent_contract::agent::run::Id,
    pub thread_id: awaken_agent_contract::agent::thread::Id,
    pub snapshot: AgentSnapshotInput,
    pub input: Vec<awaken_agent_contract::agent::message::Message>,
    pub options: RunOptions,
    pub trace: TraceContext,
}

/// Accept inline or by-id snapshot data and submit execution after validation.
#[async_trait]
pub trait RunWithSnapshotExecutor: Send + Sync {
    async fn run_with_snapshot(
        &self,
        command: RunWithSnapshotCommand,
        context: RuntimeRunContext,
    ) -> Result<RunOutcome, Error>;
}
