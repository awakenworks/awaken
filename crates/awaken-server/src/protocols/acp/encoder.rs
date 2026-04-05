//! ACP encoder: maps AgentEvent → ACP schema types for JSON-RPC 2.0 transport.
//!
//! Uses the official `agent-client-protocol-schema` crate for all wire types.

use std::collections::HashMap;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::suspension::ToolCallOutcome;
use awaken_contract::contract::tool::ToolStatus;
use awaken_contract::contract::transport::Transcoder;
use serde_json::Value;

use super::types::{
    ContentBlock, ContentChunk, RequestPermissionRequest, SessionNotification, SessionUpdate,
    StopReason, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    default_permission_options, infer_tool_kind,
};
use crate::protocols::shared::{self, TerminalGuard};

/// Output type for the ACP encoder.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum AcpOutput {
    /// A `session/update` notification.
    Notification(SessionNotification),
    /// A `session/request_permission` RPC.
    PermissionRequest(RequestPermissionRequest),
    /// The run has finished with a stop reason.
    Finished(StopReason),
    /// An error occurred.
    Error {
        message: String,
        code: Option<String>,
    },
}

/// Stateful ACP encoder that maps `AgentEvent` to `AcpOutput`.
#[derive(Debug)]
pub struct AcpEncoder {
    guard: TerminalGuard,
    session_id: String,
    pending_tool_calls: HashMap<String, PendingToolCallState>,
}

#[derive(Debug, Clone)]
struct PendingToolCallState {
    title: String,
    kind: super::types::ToolKind,
    raw_input: Value,
}

impl AcpEncoder {
    pub fn new() -> Self {
        Self {
            guard: TerminalGuard::new(),
            session_id: String::new(),
            pending_tool_calls: HashMap::new(),
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    fn notif(&self, update: SessionUpdate) -> AcpOutput {
        AcpOutput::Notification(SessionNotification::new(self.session_id.clone(), update))
    }

    pub fn on_agent_event(&mut self, ev: &AgentEvent) -> Vec<AcpOutput> {
        if self.guard.is_finished() {
            return Vec::new();
        }

        match ev {
            AgentEvent::TextDelta { delta } => {
                vec![
                    self.notif(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                        ContentBlock::from(delta.clone()),
                    ))),
                ]
            }
            AgentEvent::ReasoningDelta { delta } => {
                vec![
                    self.notif(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                        ContentBlock::from(delta.clone()),
                    ))),
                ]
            }

            AgentEvent::ToolCallStart { .. } | AgentEvent::ToolCallDelta { .. } => Vec::new(),

            AgentEvent::ToolCallReady {
                id,
                name,
                arguments,
            } => {
                let kind = infer_tool_kind(name);
                self.pending_tool_calls.insert(
                    id.clone(),
                    PendingToolCallState {
                        title: name.clone(),
                        kind,
                        raw_input: arguments.clone(),
                    },
                );
                let tc = ToolCall::new(id.clone(), name.clone())
                    .kind(kind)
                    .status(ToolCallStatus::Pending)
                    .raw_input(arguments.clone());
                vec![self.notif(SessionUpdate::ToolCall(tc))]
            }

            AgentEvent::ToolCallDone {
                id,
                result,
                outcome,
                ..
            } => match result.status {
                ToolStatus::Success => {
                    self.pending_tool_calls.remove(id);
                    let fields = ToolCallUpdateFields::new()
                        .status(ToolCallStatus::Completed)
                        .raw_output(result.to_json());
                    let update = ToolCallUpdate::new(id.clone(), fields);
                    vec![self.notif(SessionUpdate::ToolCallUpdate(update))]
                }
                ToolStatus::Pending if *outcome == ToolCallOutcome::Suspended => {
                    let Some(ticket) = result.suspension.as_ref() else {
                        return vec![AcpOutput::Error {
                            message:
                                "ACP stdio does not support suspended tool calls without a suspension ticket"
                                    .to_string(),
                            code: None,
                        }];
                    };

                    if ticket.suspension.action != "tool:PermissionConfirm" {
                        return vec![AcpOutput::Error {
                            message: format!(
                                "ACP stdio does not support suspended tool action '{}'",
                                ticket.suspension.action
                            ),
                            code: None,
                        }];
                    }

                    let info = self.pending_tool_calls.get(id).cloned().unwrap_or_else(|| {
                        PendingToolCallState {
                            title: result.tool_name.clone(),
                            kind: infer_tool_kind(&result.tool_name),
                            raw_input: Value::Null,
                        }
                    });
                    let fields = ToolCallUpdateFields::new()
                        .kind(info.kind)
                        .status(ToolCallStatus::Pending)
                        .title(info.title)
                        .raw_input(info.raw_input);
                    let update = ToolCallUpdate::new(id.clone(), fields);
                    vec![AcpOutput::PermissionRequest(RequestPermissionRequest::new(
                        self.session_id.clone(),
                        update,
                        default_permission_options(),
                    ))]
                }
                ToolStatus::Pending => Vec::new(),
                ToolStatus::Error => {
                    self.pending_tool_calls.remove(id);
                    let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
                    let update = ToolCallUpdate::new(id.clone(), fields);
                    vec![self.notif(SessionUpdate::ToolCallUpdate(update))]
                }
            },

            AgentEvent::ToolCallResumed { target_id, result } => {
                match shared::classify_resumed_result(result) {
                    shared::ResumedOutcome::Error { .. } | shared::ResumedOutcome::Denied => {
                        self.pending_tool_calls.remove(target_id);
                        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
                        let update = ToolCallUpdate::new(target_id.clone(), fields);
                        vec![self.notif(SessionUpdate::ToolCallUpdate(update))]
                    }
                    shared::ResumedOutcome::Success => {
                        self.pending_tool_calls.remove(target_id);
                        let fields = ToolCallUpdateFields::new()
                            .status(ToolCallStatus::Completed)
                            .raw_output(result.clone());
                        let update = ToolCallUpdate::new(target_id.clone(), fields);
                        vec![self.notif(SessionUpdate::ToolCallUpdate(update))]
                    }
                }
            }

            AgentEvent::RunFinish { termination, .. } => {
                if matches!(termination, TerminationReason::Suspended) {
                    return Vec::new();
                }

                self.guard.mark_finished();
                let stop_reason = map_termination(termination);
                match termination {
                    TerminationReason::Error(msg) => {
                        vec![
                            AcpOutput::Error {
                                message: msg.clone(),
                                code: None,
                            },
                            AcpOutput::Finished(stop_reason),
                        ]
                    }
                    _ => vec![AcpOutput::Finished(stop_reason)],
                }
            }

            AgentEvent::Error { message, code } => {
                self.guard.mark_finished();
                vec![AcpOutput::Error {
                    message: message.clone(),
                    code: code.clone(),
                }]
            }

            // Events with no direct ACP SessionUpdate equivalent
            AgentEvent::StateSnapshot { .. }
            | AgentEvent::StateDelta { .. }
            | AgentEvent::ActivitySnapshot { .. }
            | AgentEvent::ActivityDelta { .. }
            | AgentEvent::RunStart { .. }
            | AgentEvent::StepStart { .. }
            | AgentEvent::StepEnd
            | AgentEvent::InferenceComplete { .. }
            | AgentEvent::ReasoningEncryptedValue { .. }
            | AgentEvent::MessagesSnapshot { .. }
            | AgentEvent::ToolCallStreamDelta { .. } => Vec::new(),
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
    type Output = AcpOutput;

    fn transcode(&mut self, item: &AgentEvent) -> Vec<AcpOutput> {
        self.on_agent_event(item)
    }
}

fn map_termination(reason: &TerminationReason) -> StopReason {
    match reason {
        TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested => StopReason::EndTurn,
        TerminationReason::Suspended => StopReason::Cancelled,
        TerminationReason::Cancelled => StopReason::Cancelled,
        TerminationReason::Error(_) => StopReason::EndTurn,
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
    use awaken_contract::contract::suspension::{
        PendingToolCall, SuspendTicket, Suspension, ToolCallOutcome, ToolCallResumeMode,
    };
    use awaken_contract::contract::tool::ToolResult;
    use serde_json::json;

    use super::super::types::ToolKind;

    fn enc() -> AcpEncoder {
        AcpEncoder::new().with_session_id("sess_test")
    }

    fn assert_notification(output: &AcpOutput) -> &SessionNotification {
        match output {
            AcpOutput::Notification(n) => n,
            other => panic!("expected Notification, got: {other:?}"),
        }
    }

    fn assert_finished(output: &AcpOutput) -> StopReason {
        match output {
            AcpOutput::Finished(r) => *r,
            other => panic!("expected Finished, got: {other:?}"),
        }
    }

    #[test]
    fn text_delta_maps_to_agent_message_chunk() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "hello".into(),
        });
        assert_eq!(events.len(), 1);
        let notif = assert_notification(&events[0]);
        assert!(matches!(&notif.update, SessionUpdate::AgentMessageChunk(_)));
    }

    #[test]
    fn reasoning_delta_maps_to_agent_thought_chunk() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
            delta: "thinking".into(),
        });
        assert_eq!(events.len(), 1);
        let notif = assert_notification(&events[0]);
        assert!(matches!(&notif.update, SessionUpdate::AgentThoughtChunk(_)));
    }

    #[test]
    fn tool_call_ready_emits_tool_call_with_kind() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "c1".into(),
            name: "search".into(),
            arguments: json!({"q": "rust"}),
        });
        assert_eq!(events.len(), 1);
        let notif = assert_notification(&events[0]);
        match &notif.update {
            SessionUpdate::ToolCall(tc) => {
                assert_eq!(tc.status, ToolCallStatus::Pending);
                assert_eq!(tc.kind, ToolKind::Search);
            }
            other => panic!("expected ToolCall, got: {other:?}"),
        }
    }

    #[test]
    fn tool_call_done_success_maps_to_completed() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::success("search", json!({"items": [1]})),
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(events.len(), 1);
        let notif = assert_notification(&events[0]);
        match &notif.update {
            SessionUpdate::ToolCallUpdate(u) => {
                assert_eq!(u.fields.status, Some(ToolCallStatus::Completed));
            }
            other => panic!("expected ToolCallUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn tool_call_done_error_maps_to_failed() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::error("search", "backend failure"),
            outcome: ToolCallOutcome::Failed,
        });
        assert_eq!(events.len(), 1);
        let notif = assert_notification(&events[0]);
        match &notif.update {
            SessionUpdate::ToolCallUpdate(u) => {
                assert_eq!(u.fields.status, Some(ToolCallStatus::Failed));
            }
            other => panic!("expected ToolCallUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn tool_call_resumed_success_clears_pending_state() {
        let mut enc = enc();
        enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "c1".into(),
            name: "search".into(),
            arguments: json!({"q": "rust"}),
        });

        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "c1".into(),
            result: json!({"approved": true}),
        });

        assert_eq!(events.len(), 1);
        assert!(!enc.pending_tool_calls.contains_key("c1"));
        let notif = assert_notification(&events[0]);
        match &notif.update {
            SessionUpdate::ToolCallUpdate(u) => {
                assert_eq!(u.fields.status, Some(ToolCallStatus::Completed));
            }
            other => panic!("expected ToolCallUpdate, got: {other:?}"),
        }
    }

    #[test]
    fn natural_end_maps_to_end_turn() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        assert_eq!(events.len(), 1);
        assert_eq!(assert_finished(&events[0]), StopReason::EndTurn);
    }

    #[test]
    fn cancelled_maps_to_cancelled() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Cancelled,
        });
        assert_eq!(assert_finished(&events[0]), StopReason::Cancelled);
    }

    #[test]
    fn error_termination_emits_error_then_finished() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Error("boom".into()),
        });
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AcpOutput::Error { .. }));
        assert_eq!(assert_finished(&events[1]), StopReason::EndTurn);
    }

    #[test]
    fn blocked_maps_to_refusal() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Blocked("unsafe".into()),
        });
        assert_eq!(assert_finished(&events[0]), StopReason::Refusal);
    }

    #[test]
    fn max_rounds_stopped_maps_to_max_tokens() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Stopped(StoppedReason::new("max_rounds_reached")),
        });
        assert_eq!(assert_finished(&events[0]), StopReason::MaxTokens);
    }

    #[test]
    fn terminal_guard_suppresses_after_finish() {
        let mut enc = enc();
        enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        assert!(
            enc.on_agent_event(&AgentEvent::TextDelta {
                delta: "ignored".into()
            })
            .is_empty()
        );
    }

    #[test]
    fn suspended_permission_tool_emits_request_permission() {
        let mut enc = enc();
        let ready_events = enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "c1".into(),
            name: "bash".into(),
            arguments: json!({"cmd": "ls"}),
        });
        assert_eq!(ready_events.len(), 1);

        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::suspended_with(
                "bash",
                "awaiting approval",
                SuspendTicket::new(
                    Suspension {
                        action: "tool:PermissionConfirm".into(),
                        ..Default::default()
                    },
                    PendingToolCall::new("perm_c1", "permission_confirm", json!({"cmd": "ls"})),
                    ToolCallResumeMode::ReplayToolCall,
                ),
            ),
            outcome: ToolCallOutcome::Suspended,
        });

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AcpOutput::PermissionRequest(_)));
    }

    #[test]
    fn suspended_run_does_not_finish_stream() {
        let mut enc = enc();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Suspended,
        });
        assert!(events.is_empty());
        assert_eq!(
            enc.on_agent_event(&AgentEvent::TextDelta {
                delta: "still-open".into()
            })
            .len(),
            1
        );
    }

    #[test]
    fn silently_consumed_events() {
        let mut enc = enc();
        assert!(
            enc.on_agent_event(&AgentEvent::RunStart {
                thread_id: "t1".into(),
                run_id: "r1".into(),
                parent_run_id: None,
            })
            .is_empty()
        );
        assert!(
            enc.on_agent_event(&AgentEvent::StepStart {
                message_id: "m".into()
            })
            .is_empty()
        );
        assert!(enc.on_agent_event(&AgentEvent::StepEnd).is_empty());
    }

    #[test]
    fn transcoder_trait_delegates() {
        let mut enc = enc();
        let events = enc.transcode(&AgentEvent::TextDelta { delta: "hi".into() });
        assert_eq!(events.len(), 1);
    }
}
