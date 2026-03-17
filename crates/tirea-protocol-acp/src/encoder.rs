use serde_json::Value;
use tirea_contract::runtime::tool_call::ToolStatus;
use tirea_contract::{AgentEvent, TerminationReason, Transcoder};

use crate::events::AcpEvent;
use crate::types::StopReason;

/// Stateful encoder that maps [`AgentEvent`] to [`AcpEvent`].
///
/// ACP is a JSON-RPC 2.0 protocol; `ToolCallStart` events are buffered until
/// `ToolCallReady` arrives (ACP sends the full tool call at once).
///
/// # Permission flow
///
/// When a `ToolCallReady` for `"PermissionConfirm"` arrives, the encoder emits
/// both a `session/update` (tool_call) and a `session/request_permission` event.
///
/// # Terminal guard
///
/// Once a terminal event (`RunFinish` / `Error`) is emitted, subsequent events
/// are silently consumed.
#[derive(Debug)]
pub struct AcpEncoder {
    finished: bool,
}

impl AcpEncoder {
    /// Create a new encoder.
    pub fn new() -> Self {
        Self { finished: false }
    }

    /// Convert an [`AgentEvent`] to zero or more [`AcpEvent`]s.
    pub fn on_agent_event(&mut self, ev: &AgentEvent) -> Vec<AcpEvent> {
        if self.finished {
            return Vec::new();
        }

        match ev {
            // -- Text streaming --
            AgentEvent::TextDelta { delta } => {
                vec![AcpEvent::agent_message(delta)]
            }

            // -- Reasoning streaming --
            AgentEvent::ReasoningDelta { delta } => {
                vec![AcpEvent::agent_thought(delta)]
            }

            // -- Tool lifecycle --
            // ToolCallStart: buffered (ACP emits on ToolCallReady)
            AgentEvent::ToolCallStart { .. } | AgentEvent::ToolCallDelta { .. } => Vec::new(),

            AgentEvent::ToolCallReady {
                id,
                name,
                arguments,
            } => {
                let mut events = vec![AcpEvent::tool_call(id, name, arguments.clone())];
                if is_permission_confirmation_tool(name) {
                    let tool_name = arguments
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let tool_args = arguments.get("tool_args").cloned().unwrap_or(Value::Null);
                    events.push(AcpEvent::request_permission(id, tool_name, tool_args));
                }
                events
            }

            AgentEvent::ToolCallDone { id, result, .. } => match result.status {
                ToolStatus::Success | ToolStatus::Warning | ToolStatus::Pending => {
                    vec![AcpEvent::tool_call_completed(id, result.to_json())]
                }
                ToolStatus::Error => {
                    let error_text = result
                        .message
                        .clone()
                        .or_else(|| {
                            result
                                .data
                                .get("error")
                                .and_then(|v| v.get("message"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "tool execution error".to_string());
                    vec![AcpEvent::tool_call_errored(id, error_text)]
                }
            },

            // -- Suspension resolution --
            AgentEvent::ToolCallResumed { target_id, result } => {
                map_tool_call_resumed(target_id, result)
            }

            // -- Run lifecycle --
            AgentEvent::RunFinish { termination, .. } => {
                self.finished = true;
                let stop_reason = map_termination(termination);
                match termination {
                    TerminationReason::Error(msg) => {
                        vec![AcpEvent::error(msg, None), AcpEvent::finished(stop_reason)]
                    }
                    _ => vec![AcpEvent::finished(stop_reason)],
                }
            }

            AgentEvent::Error { message, code } => {
                self.finished = true;
                vec![AcpEvent::error(message, code.clone())]
            }

            // -- State --
            AgentEvent::StateSnapshot { snapshot } => {
                vec![AcpEvent::state_snapshot(snapshot.clone())]
            }
            AgentEvent::StateDelta { delta } => {
                vec![AcpEvent::state_delta(delta.clone())]
            }

            // -- Activity --
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

            // -- Events silently consumed by ACP --
            AgentEvent::RunStart { .. }
            | AgentEvent::StepStart { .. }
            | AgentEvent::StepEnd
            | AgentEvent::InferenceComplete { .. }
            | AgentEvent::ReasoningEncryptedValue { .. }
            | AgentEvent::MessagesSnapshot { .. } => Vec::new(),
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
        TerminationReason::Suspended => StopReason::Suspended,
        TerminationReason::Cancelled => StopReason::Cancelled,
        TerminationReason::Error(_) => StopReason::Error,
        TerminationReason::Stopped(stopped) => match stopped.code.as_str() {
            "max_rounds_reached" | "timeout_reached" | "token_budget_exceeded" => {
                StopReason::MaxTokens
            }
            _ => StopReason::EndTurn,
        },
    }
}

fn map_tool_call_resumed(target_id: &str, result: &Value) -> Vec<AcpEvent> {
    if let Some(err) = result.get("error").and_then(Value::as_str) {
        return vec![AcpEvent::tool_call_errored(target_id, err)];
    }
    if tirea_contract::SuspensionResponse::is_denied(result) {
        return vec![AcpEvent::tool_call_denied(target_id)];
    }
    vec![AcpEvent::tool_call_completed(target_id, result.clone())]
}

fn is_permission_confirmation_tool(tool_name: &str) -> bool {
    tool_name.eq_ignore_ascii_case("PermissionConfirm")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::{StoppedReason, TerminationReason};

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
            id: "call_1".into(),
            name: "search".into(),
        });
        assert!(events.is_empty());
    }

    #[test]
    fn tool_call_ready_emits_tool_call() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "call_1".into(),
            name: "search".into(),
            arguments: json!({"q": "rust"}),
        });
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            AcpEvent::tool_call("call_1", "search", json!({"q": "rust"}))
        );
    }

    #[test]
    fn tool_call_done_success_maps_to_completed() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "call_1".into(),
            result: tirea_contract::runtime::tool_call::ToolResult::success(
                "search",
                json!({"items": [1]}),
            ),
            patch: None,
            message_id: "msg_1".into(),
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.id, "call_1");
                assert_eq!(update.status, crate::types::ToolCallStatus::Completed);
            }
            other => panic!("expected SessionUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn tool_call_done_error_maps_to_errored() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "call_err".into(),
            result: tirea_contract::runtime::tool_call::ToolResult::error(
                "search",
                "backend failure",
            ),
            patch: None,
            message_id: "msg_err".into(),
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.id, "call_err");
                assert_eq!(update.status, crate::types::ToolCallStatus::Errored);
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
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], AcpEvent::finished(StopReason::Cancelled));
    }

    #[test]
    fn suspended_maps_to_suspended() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Suspended,
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], AcpEvent::finished(StopReason::Suspended));
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
        assert_eq!(events[1], AcpEvent::finished(StopReason::Error));
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
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], AcpEvent::finished(StopReason::MaxTokens));
    }

    #[test]
    fn terminal_guard_suppresses_events_after_finish() {
        let mut enc = AcpEncoder::new();
        let _ = enc.on_agent_event(&AgentEvent::RunFinish {
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
        let _ = enc.on_agent_event(&AgentEvent::Error {
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
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], AcpEvent::state_snapshot(json!({"key": "value"})));
    }

    #[test]
    fn state_delta_forwarded() {
        let mut enc = AcpEncoder::new();
        let delta = vec![json!({"op": "add", "path": "/x", "value": 1})];
        let events = enc.on_agent_event(&AgentEvent::StateDelta {
            delta: delta.clone(),
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0], AcpEvent::state_delta(delta));
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
    fn interaction_resolved_approved() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "fc_1".into(),
            result: json!({"approved": true}),
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.status, crate::types::ToolCallStatus::Completed);
            }
            other => panic!("expected SessionUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn interaction_resolved_denied() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "fc_1".into(),
            result: json!({"approved": false}),
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.status, crate::types::ToolCallStatus::Denied);
            }
            other => panic!("expected SessionUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn interaction_resolved_error() {
        let mut enc = AcpEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "fc_1".into(),
            result: json!({"error": "validation failed"}),
        });
        assert_eq!(events.len(), 1);
        match &events[0] {
            AcpEvent::SessionUpdate(params) => {
                let update = params.tool_call_update.as_ref().unwrap();
                assert_eq!(update.status, crate::types::ToolCallStatus::Errored);
                assert_eq!(update.error.as_deref(), Some("validation failed"));
            }
            other => panic!("expected SessionUpdate, got: {other:?}"),
        }
    }
}
