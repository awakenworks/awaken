//! The brain↔hand wire (ADR-0044 D2).
//!
//! The protocol carries the runtime's own neutral tool value objects —
//! [`ToolCall`] in, [`ToolOutput`] out — wrapped in a minimal envelope. It does
//! **not** introduce a parallel execution vocabulary (`ExecRequest`/`Capability`/
//! `FqId`/`Verb`); reusing the domain value objects is fewer types and no adapter
//! between "tool call" and "exec request".

use awaken_runtime_contract::tool::{ToolCall, ToolOutput};
use serde::{Deserialize, Serialize};

/// A monotonic per-executor call id used to match a [`HandReply`] to its
/// [`HandRequest`] and to key the hand's idempotency ledger (ADR-0044 D4).
pub type CorrelationId = u64;

/// One tool call framed for a hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandRequest {
    /// Matches the reply and keys the idempotency ledger; a re-drive reuses the
    /// same id so the effect runs at most once.
    pub correlation_id: CorrelationId,
    /// The run's resolved catalog fingerprint. When both sides carry one and they
    /// differ, the hand fails closed rather than run a mismatched tool (mirrors
    /// the runtime's own fingerprint discipline, G4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_fingerprint: Option<String>,
    /// Absolute wall-clock deadline hint (unix ms). Carried for the hand to abort
    /// a doomed call; enforcement is a later slice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
    /// The already-authorized, already-gated call to run.
    pub call: ToolCall,
}

/// One tool result framed back to the brain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandReply {
    pub correlation_id: CorrelationId,
    pub result: HandResult,
}

/// The outcome of a hand-side invocation.
///
/// `Indeterminate` is a first-class value (ADR-0044 D4 / G26): a call that may or
/// may not have run is never silently coerced to success or failure. It is
/// produced brain-side when the channel drops mid-flight, not sent by the hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum HandResult {
    /// The tool ran and returned a (possibly model-visible-error) output.
    Ok { output: ToolOutput },
    /// The tool could not be dispatched (unknown id, arguments, fingerprint
    /// mismatch). Distinct from a `ToolOutput` with `is_error`, which *did* run.
    Err { error: HandError },
    /// The call's outcome is unknown (channel lost after dispatch). Resolvable by
    /// an idempotent re-drive with the same `correlation_id`.
    Indeterminate,
}

/// A dispatch-level failure on the hand (not a tool's own model-visible error).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandError {
    pub kind: HandErrorKind,
    pub message: String,
}

/// Why the hand could not produce a `ToolOutput` for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandErrorKind {
    /// No tool with the requested id is in the hand's catalog.
    UnknownTool,
    /// The tool's own `invoke` returned an error.
    Execution,
    /// The request's catalog fingerprint did not match the hand's.
    FingerprintMismatch,
    /// The call arrived after its deadline.
    DeadlineExceeded,
}

impl HandError {
    pub fn new(kind: HandErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn unknown_tool(tool_id: &str) -> Self {
        // Matches the runtime's in-process `ToolError::Unknown` display so a
        // remote unknown-tool reads identically to a local one.
        Self::new(HandErrorKind::UnknownTool, format!("unknown tool: {tool_id}"))
    }
}

impl HandRequest {
    /// A request with no fingerprint/deadline constraints.
    pub fn new(correlation_id: CorrelationId, call: ToolCall) -> Self {
        Self {
            correlation_id,
            catalog_fingerprint: None,
            deadline_unix_ms: None,
            call,
        }
    }
}

impl HandResult {
    pub fn ok(output: ToolOutput) -> Self {
        HandResult::Ok { output }
    }

    pub fn err(error: HandError) -> Self {
        HandResult::Err { error }
    }
}
