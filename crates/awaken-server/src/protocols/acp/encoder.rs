//! ACP encoder: maps AgentEvent to AcpEvent (JSON-RPC 2.0).
//!
//! Aligned with the ACP specification:
//! - Tool call updates use spec-compliant status (pending/in_progress/completed/failed)
//! - Permission requests use structured PermissionOption with optionId/name/kind
//! - StopReason uses spec values (end_turn, max_tokens, max_turn_requests, refusal, cancelled)
//! - Tool calls include kind, title, and locations where available

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::tool::ToolStatus;
use awaken_contract::contract::transport::Transcoder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{
    FileDiff, FileLocation, PermissionOption, PermissionOptionKind, StopReason, ToolCallKind,
    ToolCallStatus,
};
use crate::protocols::shared::{self, TerminalGuard};

/// ACP protocol events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum AcpEvent {
    #[serde(rename = "session/update")]
    SessionUpdate(Box<SessionUpdateParams>),
    #[serde(rename = "session/request_permission")]
    RequestPermission(Box<RequestPermissionParams>),
}

/// Payload for `session/update` notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_message_chunk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_thought_chunk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_update: Option<AcpToolCallUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<AcpFinished>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AcpError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_snapshot: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_delta: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<AcpActivity>,
}

impl SessionUpdateParams {
    pub fn empty() -> Self {
        Self {
            agent_message_chunk: None,
            agent_thought_chunk: None,
            tool_call_update: None,
            finished: None,
            error: None,
            state_snapshot: None,
            state_delta: None,
            activity: None,
        }
    }
}

/// ACP tool call update — spec-compliant with toolCallId, title, kind, status,
/// content, locations, rawInput, rawOutput, diffs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpToolCallUpdate {
    pub tool_call_id: String,
    pub status: ToolCallStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolCallKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<FileLocation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diffs: Option<Vec<FileDiff>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AcpToolCallUpdate {
    fn new(tool_call_id: impl Into<String>, status: ToolCallStatus) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            status,
            title: None,
            kind: None,
            content: None,
            locations: None,
            raw_input: None,
            raw_output: None,
            diffs: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpFinished {
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpError {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpActivity {
    pub message_id: String,
    pub activity_type: String,
    pub content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Vec<Value>>,
}

/// Parameters for `session/request_permission` RPC (Agent → Client).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    pub session_id: String,
    pub tool_call: AcpToolCallUpdate,
    pub options: Vec<PermissionOption>,
}

// ── Factory methods ─────────────────────────────────────────────────

impl AcpEvent {
    pub fn agent_message(chunk: impl Into<String>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            agent_message_chunk: Some(chunk.into()),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn agent_thought(chunk: impl Into<String>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            agent_thought_chunk: Some(chunk.into()),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn tool_call_pending(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        let name_str: String = name.into();
        let kind = infer_tool_kind(&name_str);
        let mut update = AcpToolCallUpdate::new(id, ToolCallStatus::Pending);
        update.title = Some(name_str);
        update.kind = Some(kind);
        update.raw_input = Some(arguments);
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            tool_call_update: Some(update),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn tool_call_completed(id: impl Into<String>, result: Value) -> Self {
        let mut update = AcpToolCallUpdate::new(id, ToolCallStatus::Completed);
        update.raw_output = Some(result);
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            tool_call_update: Some(update),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn tool_call_failed(id: impl Into<String>, error: impl Into<String>) -> Self {
        let mut update = AcpToolCallUpdate::new(id, ToolCallStatus::Failed);
        update.error = Some(error.into());
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            tool_call_update: Some(update),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn finished(stop_reason: StopReason) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            finished: Some(AcpFinished { stop_reason }),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn error(message: impl Into<String>, code: Option<String>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            error: Some(AcpError {
                message: message.into(),
                code,
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn state_snapshot(snapshot: Value) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            state_snapshot: Some(snapshot),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn state_delta(delta: Vec<Value>) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            state_delta: Some(delta),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn activity_snapshot(
        message_id: impl Into<String>,
        activity_type: impl Into<String>,
        content: Value,
        replace: Option<bool>,
    ) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            activity: Some(AcpActivity {
                message_id: message_id.into(),
                activity_type: activity_type.into(),
                content,
                replace,
                patch: None,
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn activity_delta(
        message_id: impl Into<String>,
        activity_type: impl Into<String>,
        patch: Vec<Value>,
    ) -> Self {
        Self::SessionUpdate(Box::new(SessionUpdateParams {
            activity: Some(AcpActivity {
                message_id: message_id.into(),
                activity_type: activity_type.into(),
                content: Value::Null,
                replace: None,
                patch: Some(patch),
            }),
            ..SessionUpdateParams::empty()
        }))
    }

    pub fn request_permission(
        session_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_args: Value,
    ) -> Self {
        let name_str: String = tool_name.into();
        let mut tc = AcpToolCallUpdate::new(tool_call_id, ToolCallStatus::Pending);
        tc.title = Some(name_str.clone());
        tc.kind = Some(infer_tool_kind(&name_str));
        tc.raw_input = Some(tool_args);

        Self::RequestPermission(Box::new(RequestPermissionParams {
            session_id: session_id.into(),
            tool_call: tc,
            options: default_permission_options(),
        }))
    }
}

/// Build the default set of permission options with stable IDs.
fn default_permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: "opt_allow_once".into(),
            name: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
        },
        PermissionOption {
            option_id: "opt_allow_always".into(),
            name: "Allow always".into(),
            kind: PermissionOptionKind::AllowAlways,
        },
        PermissionOption {
            option_id: "opt_reject_once".into(),
            name: "Reject once".into(),
            kind: PermissionOptionKind::RejectOnce,
        },
        PermissionOption {
            option_id: "opt_reject_always".into(),
            name: "Reject always".into(),
            kind: PermissionOptionKind::RejectAlways,
        },
    ]
}

/// Infer a tool call kind from the tool name using common heuristics.
fn infer_tool_kind(name: &str) -> ToolCallKind {
    let lower = name.to_ascii_lowercase();
    if lower.contains("read") || lower.contains("cat") || lower.contains("view") {
        ToolCallKind::Read
    } else if lower.contains("edit") || lower.contains("write") || lower.contains("patch") {
        ToolCallKind::Edit
    } else if lower.contains("delete") || lower.contains("remove") || lower.contains("rm") {
        ToolCallKind::Delete
    } else if lower.contains("move") || lower.contains("rename") || lower.contains("mv") {
        ToolCallKind::Move
    } else if lower.contains("search") || lower.contains("grep") || lower.contains("find") {
        ToolCallKind::Search
    } else if lower.contains("bash")
        || lower.contains("exec")
        || lower.contains("run")
        || lower.contains("shell")
    {
        ToolCallKind::Execute
    } else if lower.contains("think") || lower.contains("reason") || lower.contains("plan") {
        ToolCallKind::Think
    } else if lower.contains("fetch") || lower.contains("http") || lower.contains("curl") {
        ToolCallKind::Fetch
    } else {
        ToolCallKind::Other
    }
}

// ── Stateful encoder ────────────────────────────────────────────────

/// Stateful ACP encoder.
#[derive(Debug)]
pub struct AcpEncoder {
    guard: TerminalGuard,
    /// Session ID to attach to permission requests.
    session_id: String,
}

impl AcpEncoder {
    pub fn new() -> Self {
        Self {
            guard: TerminalGuard::new(),
            session_id: String::new(),
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    pub fn on_agent_event(&mut self, ev: &AgentEvent) -> Vec<AcpEvent> {
        if self.guard.is_finished() {
            return Vec::new();
        }

        match ev {
            AgentEvent::TextDelta { delta } => vec![AcpEvent::agent_message(delta)],
            AgentEvent::ReasoningDelta { delta } => vec![AcpEvent::agent_thought(delta)],

            // ACP bundles tool lifecycle into a single pending → completed flow;
            // streaming start/delta events have no ACP-protocol equivalent.
            AgentEvent::ToolCallStart { .. } | AgentEvent::ToolCallDelta { .. } => Vec::new(),

            AgentEvent::ToolCallReady {
                id,
                name,
                arguments,
            } => {
                let mut events = vec![AcpEvent::tool_call_pending(id, name, arguments.clone())];
                if name.eq_ignore_ascii_case("PermissionConfirm") {
                    let tool_name = arguments
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let tool_args = arguments.get("tool_args").cloned().unwrap_or(Value::Null);
                    events.push(AcpEvent::request_permission(
                        &self.session_id,
                        id,
                        tool_name,
                        tool_args,
                    ));
                }
                events
            }

            AgentEvent::ToolCallDone { id, result, .. } => match result.status {
                ToolStatus::Success | ToolStatus::Pending => {
                    vec![AcpEvent::tool_call_completed(id, result.to_json())]
                }
                ToolStatus::Error => {
                    let error_text = result
                        .message
                        .clone()
                        .unwrap_or_else(|| "tool execution error".to_string());
                    vec![AcpEvent::tool_call_failed(id, error_text)]
                }
            },

            AgentEvent::ToolCallResumed { target_id, result } => {
                match shared::classify_resumed_result(result) {
                    shared::ResumedOutcome::Error { message } => {
                        vec![AcpEvent::tool_call_failed(target_id, message)]
                    }
                    shared::ResumedOutcome::Denied => {
                        // Denied is reported as failed with a denial message per spec
                        // (spec has no "denied" status — only pending/in_progress/completed/failed)
                        vec![AcpEvent::tool_call_failed(target_id, "permission denied")]
                    }
                    shared::ResumedOutcome::Success => {
                        vec![AcpEvent::tool_call_completed(target_id, result.clone())]
                    }
                }
            }

            AgentEvent::RunFinish { termination, .. } => {
                self.guard.mark_finished();
                let stop_reason = map_termination(termination);
                match termination {
                    TerminationReason::Error(msg) => {
                        vec![AcpEvent::error(msg, None), AcpEvent::finished(stop_reason)]
                    }
                    _ => vec![AcpEvent::finished(stop_reason)],
                }
            }

            AgentEvent::Error { message, code } => {
                self.guard.mark_finished();
                vec![AcpEvent::error(message, code.clone())]
            }

            AgentEvent::StateSnapshot { snapshot } => {
                vec![AcpEvent::state_snapshot(snapshot.clone())]
            }
            AgentEvent::StateDelta { delta } => vec![AcpEvent::state_delta(delta.clone())],

            AgentEvent::ActivitySnapshot {
                message_id,
                activity_type,
                content,
                replace,
            } => {
                vec![AcpEvent::activity_snapshot(
                    message_id,
                    activity_type,
                    content.clone(),
                    *replace,
                )]
            }

            AgentEvent::ActivityDelta {
                message_id,
                activity_type,
                patch,
            } => {
                vec![AcpEvent::activity_delta(
                    message_id,
                    activity_type,
                    patch.clone(),
                )]
            }

            // ACP has no run/step lifecycle events; session boundary is implicit.
            AgentEvent::RunStart { .. } => Vec::new(),
            // ACP has no step concept; inference steps are not surfaced.
            AgentEvent::StepStart { .. } | AgentEvent::StepEnd => Vec::new(),
            // ACP has no inference telemetry event; model usage is not surfaced.
            AgentEvent::InferenceComplete { .. } => Vec::new(),
            // ACP has no encrypted reasoning transport; reasoning is sent as plaintext thoughts.
            AgentEvent::ReasoningEncryptedValue { .. } => Vec::new(),
            // ACP has no messages-snapshot equivalent; state is managed via state_snapshot/state_delta.
            AgentEvent::MessagesSnapshot { .. } => Vec::new(),
            // ACP tool results are sent atomically via tool_call_update; streaming deltas are not surfaced.
            AgentEvent::ToolCallStreamDelta { .. } => Vec::new(),
        }
    }
}

impl Default for AcpEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcoder for AcpEncoder {
    type Input = AgentEvent;
    type Output = AcpEvent;

    fn transcode(&mut self, item: &AgentEvent) -> Vec<AcpEvent> {
        self.on_agent_event(item)
    }
}

fn map_termination(reason: &TerminationReason) -> StopReason {
    match reason {
        TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested => StopReason::EndTurn,
        // Suspended is not in ACP spec; map to cancelled as the closest equivalent
        TerminationReason::Suspended => StopReason::Cancelled,
        TerminationReason::Cancelled => StopReason::Cancelled,
        // Error terminations map to end_turn with a preceding error event
        TerminationReason::Error(_) => StopReason::EndTurn,
        // Blocked maps to refusal
        TerminationReason::Blocked(_) => StopReason::Refusal,
        TerminationReason::Stopped(stopped) => match stopped.code.as_str() {
            "max_rounds_reached" | "timeout_reached" | "token_budget_exceeded" => {
                StopReason::MaxTokens
            }
            _ => StopReason::EndTurn,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::event::AgentEvent;
    use awaken_contract::contract::lifecycle::{StoppedReason, TerminationReason};
    use awaken_contract::contract::suspension::ToolCallOutcome;
    use awaken_contract::contract::tool::ToolResult;
    use serde_json::json;

    #[test]
    fn text_delta_maps_to_agent_message() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "hello".into(),
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], AcpEvent::agent_message("hello"));
    }

    #[test]
    fn reasoning_delta_maps_to_agent_thought() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
            delta: "thinking".into(),
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], AcpEvent::agent_thought("thinking"));
    }

    #[test]
    fn tool_call_start_is_buffered() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: "c1".into(),
            name: "search".into(),
        });
        assert!(events.is_empty());
    }

    #[test]
    fn tool_call_ready_emits_pending_update() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "c1".into(),
            name: "search".into(),
            arguments: json!({"q": "rust"}),
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.tool_call_id, "c1");
                assert_eq!(update.status, ToolCallStatus::Pending);
                assert_eq!(update.title.as_deref(), Some("search"));
                assert_eq!(update.kind, Some(ToolCallKind::Search));
                assert_eq!(update.raw_input, Some(json!({"q": "rust"})));
            }
            other => panic!("expected SessionUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn tool_call_done_success_maps_to_completed() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::success("search", json!({"items": [1]})),
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.tool_call_id, "c1");
                assert_eq!(update.status, ToolCallStatus::Completed);
            }
            other => panic!("expected SessionUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn tool_call_done_error_maps_to_failed() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::error("search", "backend failure"),
            outcome: ToolCallOutcome::Failed,
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.status, ToolCallStatus::Failed);
                assert_eq!(update.error.as_deref(), Some("backend failure"));
            }
            other => panic!("expected SessionUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn natural_end_maps_to_end_turn() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], AcpEvent::finished(StopReason::EndTurn));
    }

    #[test]
    fn cancelled_maps_to_cancelled() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Cancelled,
        });
        assert_eq!(events[0], AcpEvent::finished(StopReason::Cancelled));
    }

    #[test]
    fn suspended_maps_to_cancelled() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Suspended,
        });
        assert_eq!(events[0], AcpEvent::finished(StopReason::Cancelled));
    }

    #[test]
    fn error_termination_emits_error_then_finished() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Error("boom".into()),
        });
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], AcpEvent::error("boom", None));
        assert_eq!(events[1], AcpEvent::finished(StopReason::EndTurn));
    }

    #[test]
    fn blocked_maps_to_refusal() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Blocked("unsafe tool".into()),
        });
        assert_eq!(events[0], AcpEvent::finished(StopReason::Refusal));
    }

    #[test]
    fn max_rounds_stopped_maps_to_max_tokens() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Stopped(StoppedReason::new("max_rounds_reached")),
        });
        assert_eq!(events[0], AcpEvent::finished(StopReason::MaxTokens));
    }

    #[test]
    fn terminal_guard_suppresses_events_after_finish() {
        let mut enc = AcpEncoder::new();
        enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        let events = enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "ignored".into(),
        });
        assert!(events.is_empty());
    }

    #[test]
    fn error_event_sets_terminal_guard() {
        let mut enc = AcpEncoder::new();
        enc.on_agent_event(&AgentEvent::Error {
            message: "fatal".into(),
            code: Some("E001".into()),
        });
        let events = enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "ignored".into(),
        });
        assert!(events.is_empty());
    }

    #[test]
    fn state_snapshot_forwarded() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
            snapshot: json!({"key": "value"}),
        });
        assert_eq!(events[0], AcpEvent::state_snapshot(json!({"key": "value"})));
    }

    #[test]
    fn run_start_silently_consumed() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        });
        assert!(events.is_empty());
    }

    #[test]
    fn session_update_roundtrip() {
        let event = AcpEvent::agent_message("hello");
        let json = serde_json::to_string(&event).unwrap();
        let restored: AcpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, restored);
    }

    #[test]
    fn request_permission_roundtrip() {
        let event =
            AcpEvent::request_permission("sess_1", "fc_1", "bash", json!({"command": "rm"}));
        let json = serde_json::to_string(&event).unwrap();
        let restored: AcpEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, restored);
    }

    #[test]
    fn finished_serializes_stop_reason() {
        let event = AcpEvent::finished(StopReason::EndTurn);
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["params"]["finished"]["stopReason"], "end_turn");
    }

    #[test]
    fn transcoder_trait_delegates() {
        let mut enc = AcpEncoder::new();
        let events = enc.transcode(&AgentEvent::TextDelta { delta: "hi".into() });
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn permission_confirm_tool_emits_request_permission() {
        let mut enc = AcpEncoder::new().with_session_id("sess_test");
        let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "c1".into(),
            name: "PermissionConfirm".into(),
            arguments: json!({"tool_name": "bash", "tool_args": {"cmd": "ls"}}),
        });
        assert_eq!(events.len(), 2);
        match &events[1] {
            AcpEvent::RequestPermission(params) => {
                assert_eq!(params.session_id, "sess_test");
                assert_eq!(params.tool_call.tool_call_id, "c1");
                assert_eq!(params.options.len(), 4);
                assert_eq!(params.options[0].option_id, "opt_allow_once");
                assert_eq!(params.options[0].kind, PermissionOptionKind::AllowOnce);
            }
            other => panic!("expected RequestPermission, got: {other:?}"),
        }
    }

    #[test]
    fn tool_call_resumed_approved() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "fc_1".into(),
            result: json!({"approved": true}),
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.status, ToolCallStatus::Completed);
            }
            other => panic!("expected SessionUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn tool_call_resumed_denied() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "fc_1".into(),
            result: json!({"approved": false}),
        });
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.status, ToolCallStatus::Failed);
                assert_eq!(update.error.as_deref(), Some("permission denied"));
            }
            other => panic!("expected SessionUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn file_activity_snapshot_forwarded() {
        let mut enc = AcpEncoder::new();
        let file_content = json!({
            "path": "src/main.rs",
            "operation": "created",
            "size": 1024
        });
        let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "call-1:src/main.rs".into(),
            activity_type: "file".into(),
            content: file_content.clone(),
            replace: Some(true),
        });
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            AcpEvent::activity_snapshot("call-1:src/main.rs", "file", file_content, Some(true))
        );
    }

    #[test]
    fn tool_call_progress_activity_forwarded() {
        let mut enc = AcpEncoder::new();
        let progress_content = json!({
            "schema": "tool-call-progress.v1",
            "node_id": "call-1",
            "call_id": "call-1",
            "tool_name": "search",
            "status": "running",
            "progress": 0.5,
            "message": "Searching..."
        });
        let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "call-1".into(),
            activity_type: "tool-call-progress".into(),
            content: progress_content.clone(),
            replace: Some(true),
        });
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            AcpEvent::activity_snapshot(
                "call-1",
                "tool-call-progress",
                progress_content,
                Some(true)
            )
        );
    }

    #[test]
    fn infer_tool_kind_heuristics() {
        assert_eq!(infer_tool_kind("read_file"), ToolCallKind::Read);
        assert_eq!(infer_tool_kind("edit_file"), ToolCallKind::Edit);
        assert_eq!(infer_tool_kind("bash"), ToolCallKind::Execute);
        assert_eq!(infer_tool_kind("search"), ToolCallKind::Search);
        assert_eq!(infer_tool_kind("grep"), ToolCallKind::Search);
        assert_eq!(infer_tool_kind("http_fetch"), ToolCallKind::Fetch);
        assert_eq!(infer_tool_kind("think"), ToolCallKind::Think);
        assert_eq!(infer_tool_kind("unknown_tool"), ToolCallKind::Other);
    }
}
