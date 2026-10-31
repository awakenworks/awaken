// @formal entrypoint remote_tool.protocol_version_requires_stable_v2_operation_identity
// @formal evidence remote_tool.stable_operation_identity
// @formal entrypoint remote_tool.controls_never_admit_invocation
//! The brain↔hand wire (ADR-0044 D2).
//!
//! The protocol carries the runtime's own neutral tool value objects —
//! [`ToolCall`] in, [`ToolOutput`] out — wrapped in a minimal envelope. It does
//! **not** introduce a parallel execution vocabulary (`ExecRequest`/`Capability`/
//! `FqId`/`Verb`); reusing the domain value objects is fewer types and no adapter
//! between "tool call" and "exec request".

use awaken_tool_contract::tool::{ToolCall, ToolOutput};
use serde::{Deserialize, Serialize};

/// A monotonic per-connection request id used only to match a [`HandReply`] to
/// its [`HandRequest`]. It is not an effect identity.
pub type CorrelationId = u64;

pub const CURRENT_HAND_PROTOCOL_VERSION: u16 = 3;

/// Control of an existing operation, never a second execution path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandAction {
    #[default]
    Invoke,
    Observe,
    Cancel,
}

impl HandAction {
    fn is_invoke(&self) -> bool {
        *self == Self::Invoke
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandDispatchPlan {
    Invoke,
    Observe,
    Cancel,
    Refuse,
}

/// The sole routing decision, before any ledger admission or registry call.
pub(crate) fn hand_dispatch_plan(request: &HandRequest) -> HandDispatchPlan {
    if !hand_protocol_envelope_admitted(request.protocol_version, !request.operation_id.is_empty())
    {
        return HandDispatchPlan::Refuse;
    }
    match request.action {
        HandAction::Invoke => HandDispatchPlan::Invoke,
        HandAction::Observe if request.protocol_version >= 3 => HandDispatchPlan::Observe,
        HandAction::Cancel if request.protocol_version >= 3 => HandDispatchPlan::Cancel,
        _ => HandDispatchPlan::Refuse,
    }
}

#[must_use]
pub(crate) const fn hand_protocol_envelope_admitted(
    protocol_version: u16,
    has_operation_id: bool,
) -> bool {
    protocol_version >= 1
        && protocol_version <= CURRENT_HAND_PROTOCOL_VERSION
        && (protocol_version == 1 || has_operation_id)
}

/// One tool call framed for a hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandRequest {
    #[serde(default, skip_serializing_if = "HandAction::is_invoke")]
    pub action: HandAction,
    /// Version of the operation-identity contract. Legacy peers deserialize to
    /// v1; v2 requires an explicit stable operation id for every request.
    #[serde(default = "legacy_hand_protocol_version")]
    pub protocol_version: u16,
    /// Matches one transport reply. A re-drive normally receives a fresh value.
    pub correlation_id: CorrelationId,
    /// Stable effect identity. Every transport retry of the same logical tool
    /// call carries the same value, even across a new connection/process.
    #[serde(default)]
    pub operation_id: String,
    /// The run's resolved catalog fingerprint. When both sides carry one and they
    /// differ, the hand fails closed rather than run a mismatched tool (mirrors
    /// the runtime's own fingerprint discipline, G4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_fingerprint: Option<String>,
    /// Absolute wall-clock deadline hint (unix ms). The Hand rejects an already
    /// expired request before ledger admission so it cannot consume or poison an
    /// operation identity.
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
/// produced brain-side when a request write or reply read is uncertain, not sent
/// by the hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum HandResult {
    /// A control read of the original cached invocation, not a control error.
    Observed { result: Box<HandResult> },
    /// The original invocation is live. Cancellation acceptance proves no stop.
    Running { cancel_requested: bool },
    /// The tool ran and returned a (possibly model-visible-error) output.
    Ok { output: ToolOutput },
    /// The tool could not be dispatched (unknown id, arguments, fingerprint
    /// mismatch). Distinct from a `ToolOutput` with `is_error`, which *did* run.
    Err { error: HandError },
    /// The call's outcome is unknown (channel lost after dispatch). Resolvable by
    /// an idempotent re-drive with the same stable `operation_id`; the transport
    /// correlation id may be fresh.
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
    /// The tool certainly did not run, so a retry is safe (ADR-0103).
    UnavailableBeforeDispatch,
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
        // Matches the runtime's in-process `ToolError::UnknownTool` display so a
        // remote unknown-tool reads identically to a local one.
        Self::new(
            HandErrorKind::UnknownTool,
            format!("unknown tool: {tool_id}"),
        )
    }
}

impl HandRequest {
    /// A request with no fingerprint/deadline constraints.
    pub fn new(correlation_id: CorrelationId, call: ToolCall) -> Self {
        Self {
            action: HandAction::Invoke,
            protocol_version: CURRENT_HAND_PROTOCOL_VERSION,
            correlation_id,
            operation_id: call.call_id.clone(),
            catalog_fingerprint: None,
            deadline_unix_ms: None,
            call,
        }
    }
}

const fn legacy_hand_protocol_version() -> u16 {
    1
}

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation remote_tool.controls_never_admit_invocation
    #[kani::proof]
    #[kani::unwind(16)]
    fn recovery_control_plan_never_selects_invocation_admission() {
        // Raw request fields, not free admission facts: arbitrary wire version,
        // each control action, and absent/present operation identity. The same
        // production plan routes serve_hand before its only ledger.begin call.
        let action = if kani::any::<bool>() {
            HandAction::Observe
        } else {
            HandAction::Cancel
        };
        let mut request = HandRequest::new(
            1,
            ToolCall {
                call_id: "c".into(),
                tool_id: "t".into(),
                arguments: serde_json::Value::Null,
            },
        );
        request.action = action;
        request.protocol_version = kani::any();
        if kani::any::<bool>() {
            request.operation_id.clear();
        }
        let plan = hand_dispatch_plan(&request);
        assert_ne!(plan, HandDispatchPlan::Invoke);
        if matches!(plan, HandDispatchPlan::Observe | HandDispatchPlan::Cancel) {
            assert_eq!(request.protocol_version, 3);
            assert!(!request.operation_id.is_empty());
        }
    }

    /// @obligation remote_tool.protocol_version_requires_stable_v2_operation_identity
    #[kani::proof]
    #[kani::unwind(16)]
    fn hand_protocol_requires_supported_version_and_v2_operation_identity() {
        // The production caller derives identity presence from wire bytes.
        // Empty/nonempty strings are the two equivalence classes of that
        // derivation; do not substitute an unconstrained admission Boolean.
        let mut request = HandRequest::new(
            1,
            ToolCall {
                call_id: "c".into(),
                tool_id: "t".into(),
                arguments: serde_json::Value::Null,
            },
        );
        request.protocol_version = kani::any();
        if kani::any::<bool>() {
            request.operation_id.clear();
        }
        let admitted = hand_protocol_envelope_admitted(
            request.protocol_version,
            !request.operation_id.is_empty(),
        );
        if admitted {
            assert!(request.protocol_version >= 1);
            assert!(request.protocol_version <= CURRENT_HAND_PROTOCOL_VERSION);
            assert!(request.protocol_version == 1 || !request.operation_id.is_empty());
        }
        if request.protocol_version == 1 {
            assert!(admitted);
        }
        if request.protocol_version == 2 && request.operation_id.is_empty() {
            assert!(!admitted);
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
