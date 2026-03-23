//! Tool interception types for the BeforeToolExecute phase.
//!
//! [`ToolInterceptAction`]: scheduled action that controls whether a tool call
//! proceeds, is blocked, suspended, or short-circuited with a pre-built result.

use serde::{Deserialize, Serialize};

use crate::contract::suspension::SuspendTicket;
use crate::contract::tool::ToolResult;
use crate::model::Phase;

/// Payload for the [`ToolInterceptAction`] scheduled action.
///
/// BeforeToolExecute phase hooks schedule this to control tool execution flow.
/// If no intercept is scheduled, the tool executes normally (implicit proceed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolInterceptPayload {
    /// Block tool execution and terminate the run.
    Block { reason: String },
    /// Suspend tool execution pending external decision (permission, frontend, etc.).
    Suspend(SuspendTicket),
    /// Skip execution and use this result directly (frontend tool resume, deny with message).
    SetResult(ToolResult),
}

/// Scheduled action spec for tool interception.
///
/// Hooks schedule this during `BeforeToolExecute` to intercept tool calls.
/// Multiple hooks may schedule intercepts; the handler aggregates by priority:
/// `Block > Suspend > SetResult`.
pub struct ToolInterceptAction;

impl crate::model::ScheduledActionSpec for ToolInterceptAction {
    const KEY: &'static str = "tool_intercept";
    const PHASE: Phase = Phase::BeforeToolExecute;
    type Payload = ToolInterceptPayload;
}
