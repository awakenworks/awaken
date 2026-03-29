//! AG-UI encoder: maps AgentEvent to AG-UI Event.

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::tool::ToolStatus;
use awaken_contract::contract::transport::Transcoder;
use std::collections::HashMap;

use super::types::{BaseEvent, Event, Role};
use crate::protocols::shared::TerminalGuard;

/// Stateful AG-UI protocol encoder.
///
/// Tracks text message lifecycle (start/end), step naming, and reasoning blocks.
#[derive(Debug)]
pub struct AgUiEncoder {
    message_id: String,
    text_open: bool,
    step_counter: u32,
    reasoning_open: bool,
    guard: TerminalGuard,
}

impl AgUiEncoder {
    pub fn new() -> Self {
        Self {
            message_id: String::new(),
            text_open: false,
            step_counter: 0,
            reasoning_open: false,
            guard: TerminalGuard::new(),
        }
    }

    fn close_text(&mut self) -> Option<Event> {
        if self.text_open {
            self.text_open = false;
            Some(Event::text_message_end(&self.message_id))
        } else {
            None
        }
    }

    fn close_reasoning(&mut self) -> Option<Event> {
        if self.reasoning_open {
            self.reasoning_open = false;
            Some(Event::ReasoningMessageEnd {
                message_id: self.message_id.clone(),
                base: BaseEvent::default(),
            })
        } else {
            None
        }
    }

    pub fn on_agent_event(&mut self, ev: &AgentEvent) -> Vec<Event> {
        if self.guard.is_finished() {
            return Vec::new();
        }

        match ev {
            AgentEvent::RunStart {
                thread_id,
                run_id,
                parent_run_id,
            } => {
                self.message_id = run_id.clone();
                vec![Event::run_started(thread_id, run_id, parent_run_id.clone())]
            }

            AgentEvent::TextDelta { delta } => {
                let mut events = Vec::new();
                if !self.text_open {
                    self.text_open = true;
                    events.push(Event::text_message_start(&self.message_id));
                }
                events.push(Event::text_message_content(&self.message_id, delta));
                events
            }

            AgentEvent::ReasoningDelta { delta } => {
                let mut events = Vec::new();
                if !self.reasoning_open {
                    self.reasoning_open = true;
                    events.push(Event::ReasoningMessageStart {
                        message_id: self.message_id.clone(),
                        role: Role::Assistant,
                        base: BaseEvent::default(),
                    });
                }
                events.push(Event::ReasoningMessageContent {
                    message_id: self.message_id.clone(),
                    delta: delta.clone(),
                    base: BaseEvent::default(),
                });
                events
            }

            AgentEvent::ReasoningEncryptedValue { encrypted_value } => {
                vec![Event::ReasoningEncryptedValue {
                    entity_id: self.message_id.clone(),
                    encrypted_value: encrypted_value.clone(),
                    base: BaseEvent::default(),
                }]
            }

            AgentEvent::ToolCallStart { id, name } => {
                let mut events = Vec::new();
                if let Some(e) = self.close_text() {
                    events.push(e);
                }
                if let Some(e) = self.close_reasoning() {
                    events.push(e);
                }
                events.push(Event::tool_call_start(
                    id,
                    name,
                    Some(self.message_id.clone()),
                ));
                events
            }

            AgentEvent::ToolCallDelta { id, args_delta } => {
                vec![Event::tool_call_args(id, args_delta)]
            }

            AgentEvent::ToolCallReady { id, .. } => {
                vec![Event::tool_call_end(id)]
            }

            AgentEvent::ToolCallDone {
                id,
                message_id,
                result,
                ..
            } => match result.status {
                ToolStatus::Success => {
                    let content = if result.metadata.is_empty() {
                        serde_json::to_string(&result.data).unwrap_or_default()
                    } else {
                        serde_json::to_string(&serde_json::json!({
                            "data": result.data,
                            "metadata": result.metadata,
                        }))
                        .unwrap_or_default()
                    };
                    vec![Event::tool_call_result(message_id, id, content)]
                }
                ToolStatus::Error => {
                    let content = result.message.as_deref().unwrap_or("tool error");
                    vec![Event::tool_call_result(message_id, id, content)]
                }
                ToolStatus::Pending => Vec::new(),
            },

            AgentEvent::StepStart { message_id } => {
                self.step_counter += 1;
                if !message_id.is_empty() {
                    self.message_id = message_id.clone();
                }
                vec![Event::step_started(format!("step_{}", self.step_counter))]
            }

            AgentEvent::StepEnd => {
                let mut events = Vec::new();
                if let Some(e) = self.close_text() {
                    events.push(e);
                }
                if let Some(e) = self.close_reasoning() {
                    events.push(e);
                }
                events.push(Event::step_finished(format!("step_{}", self.step_counter)));
                events
            }

            AgentEvent::RunFinish {
                thread_id,
                run_id,
                result,
                termination,
            } => {
                self.guard.mark_finished();
                let mut events = Vec::new();
                if let Some(e) = self.close_text() {
                    events.push(e);
                }
                if let Some(e) = self.close_reasoning() {
                    events.push(e);
                }
                match termination {
                    TerminationReason::Error(msg) => {
                        events.push(Event::run_error(msg, None));
                    }
                    TerminationReason::Suspended => {
                        events.push(Event::run_interrupted(
                            thread_id,
                            run_id,
                            super::types::InterruptPayload {
                                id: None,
                                reason: Some("tool_approval".into()),
                                payload: None,
                            },
                        ));
                    }
                    _ => {
                        events.push(Event::run_finished(thread_id, run_id, result.clone()));
                    }
                }
                events
            }

            AgentEvent::Error { message, code } => {
                self.guard.mark_finished();
                vec![Event::run_error(message, code.clone())]
            }

            AgentEvent::StateSnapshot { snapshot } => {
                vec![Event::state_snapshot(snapshot.clone())]
            }

            AgentEvent::StateDelta { delta } => {
                vec![Event::state_delta(delta.clone())]
            }

            AgentEvent::MessagesSnapshot { messages } => {
                vec![Event::messages_snapshot(messages.clone())]
            }

            AgentEvent::ActivitySnapshot {
                message_id,
                activity_type,
                content,
                replace,
            } => {
                let map = match content.as_object() {
                    Some(obj) => obj
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<HashMap<_, _>>(),
                    None => {
                        let mut m = HashMap::new();
                        m.insert("value".to_string(), content.clone());
                        m
                    }
                };
                vec![Event::ActivitySnapshot {
                    message_id: message_id.clone(),
                    activity_type: activity_type.clone(),
                    content: map,
                    replace: *replace,
                    base: BaseEvent::default(),
                }]
            }

            AgentEvent::ActivityDelta {
                message_id,
                activity_type,
                patch,
            } => {
                vec![Event::ActivityDelta {
                    message_id: message_id.clone(),
                    activity_type: activity_type.clone(),
                    patch: patch.clone(),
                    base: BaseEvent::default(),
                }]
            }

            AgentEvent::ToolCallResumed { target_id, result } => {
                let content = serde_json::to_string(result).unwrap_or_default();
                vec![Event::tool_call_result(
                    &self.message_id,
                    target_id,
                    content,
                )]
            }

            AgentEvent::ToolCallStreamDelta { id, delta, .. } => {
                vec![Event::ActivityDelta {
                    message_id: id.clone(),
                    activity_type: "tool-stream-output".to_string(),
                    patch: vec![serde_json::json!({
                        "op": "add",
                        "path": "/delta",
                        "value": delta,
                    })],
                    base: BaseEvent::default(),
                }]
            }

            AgentEvent::InferenceComplete { .. } => Vec::new(),
        }
    }
}

impl Default for AgUiEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcoder for AgUiEncoder {
    type Input = AgentEvent;
    type Output = Event;

    fn transcode(&mut self, item: &AgentEvent) -> Vec<Event> {
        self.on_agent_event(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::event::AgentEvent;
    use awaken_contract::contract::lifecycle::TerminationReason;
    use awaken_contract::contract::suspension::ToolCallOutcome;
    use awaken_contract::contract::tool::ToolResult;
    use serde_json::json;

    #[test]
    fn run_start_emits_run_started() {
        let mut enc = AgUiEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::RunStarted { run_id, .. } if run_id == "r1"));
    }

    #[test]
    fn text_delta_opens_text_message() {
        let mut enc = AgUiEncoder::new();
        enc.message_id = "m1".into();
        let events = enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::TextMessageStart { .. }));
        assert!(matches!(&events[1], Event::TextMessageContent { delta, .. } if delta == "hi"));
    }

    #[test]
    fn tool_call_start_closes_text() {
        let mut enc = AgUiEncoder::new();
        enc.message_id = "m1".into();
        enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
        let events = enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: "c1".into(),
            name: "search".into(),
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TextMessageEnd { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolCallStart { .. }))
        );
    }

    #[test]
    fn tool_call_done_success() {
        let mut enc = AgUiEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::success("search", json!(42)),
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::ToolCallResult { content, .. } if content == "42"));
    }

    #[test]
    fn run_finish_closes_text_and_emits_finished() {
        let mut enc = AgUiEncoder::new();
        enc.message_id = "m1".into();
        enc.on_agent_event(&AgentEvent::TextDelta { delta: "hi".into() });
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TextMessageEnd { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::RunFinished { .. }))
        );
    }

    #[test]
    fn error_termination_emits_run_error() {
        let mut enc = AgUiEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Error("boom".into()),
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::RunError { message, .. } if message == "boom"))
        );
    }

    #[test]
    fn terminal_guard_suppresses_events() {
        let mut enc = AgUiEncoder::new();
        enc.on_agent_event(&AgentEvent::Error {
            message: "fatal".into(),
            code: None,
        });
        let events = enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "ignored".into(),
        });
        assert!(events.is_empty());
    }

    #[test]
    fn reasoning_delta_opens_reasoning() {
        let mut enc = AgUiEncoder::new();
        enc.message_id = "m1".into();
        let events = enc.on_agent_event(&AgentEvent::ReasoningDelta {
            delta: "thinking".into(),
        });
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::ReasoningMessageStart { .. }));
    }

    #[test]
    fn step_events_generate_step_names() {
        let mut enc = AgUiEncoder::new();
        let start = enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m1".into(),
        });
        assert!(matches!(
            &start[0],
            Event::StepStarted { step_name, .. } if step_name == "step_1"
        ));

        let end = enc.on_agent_event(&AgentEvent::StepEnd);
        assert!(end.iter().any(|e| matches!(
            e,
            Event::StepFinished { step_name, .. } if step_name == "step_1"
        )));
    }

    #[test]
    fn state_events_pass_through() {
        let mut enc = AgUiEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::StateSnapshot {
            snapshot: json!({"key": "val"}),
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::StateSnapshot { .. }));
    }

    #[test]
    fn transcoder_trait_works() {
        let mut enc = AgUiEncoder::new();
        enc.message_id = "m1".into();
        let events = enc.transcode(&AgentEvent::TextDelta { delta: "hi".into() });
        assert!(!events.is_empty());
    }

    #[test]
    fn step_start_text_step_end_run_finish_emits_both_step_and_run_finished() {
        let mut enc = AgUiEncoder::new();
        enc.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        });
        enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m1".into(),
        });
        enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "hello".into(),
        });

        let step_end_events = enc.on_agent_event(&AgentEvent::StepEnd);
        assert!(
            step_end_events
                .iter()
                .any(|e| matches!(e, Event::StepFinished { .. })),
            "StepEnd must produce StepFinished"
        );

        let run_events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        assert!(
            run_events
                .iter()
                .any(|e| matches!(e, Event::RunFinished { .. })),
            "RunFinish must produce RunFinished"
        );
    }

    #[test]
    fn missing_step_end_before_run_finish_still_emits_run_finished() {
        // Simulates the pre-fix scenario where Terminated skipped StepEnd.
        // The encoder itself doesn't inject a missing StepFinished — it just
        // produces RunFinished. This test documents that behavior.
        let mut enc = AgUiEncoder::new();
        enc.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        });
        enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m1".into(),
        });
        enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "hello".into(),
        });

        // No StepEnd — jump straight to RunFinish
        let run_events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        });
        // Encoder closes open text message but does NOT emit StepFinished
        assert!(
            !run_events
                .iter()
                .any(|e| matches!(e, Event::StepFinished { .. })),
            "Encoder should not inject StepFinished on its own"
        );
        assert!(
            run_events
                .iter()
                .any(|e| matches!(e, Event::RunFinished { .. })),
            "RunFinished must still be emitted"
        );
    }

    #[test]
    fn full_tool_call_lifecycle_event_ordering() {
        // Simulates: RunStart -> StepStart -> TextDelta -> ToolCallStart -> ToolCallDelta ->
        //            ToolCallReady -> ToolCallDone -> StepEnd -> StepStart -> TextDelta -> StepEnd -> RunFinish
        // Verifies: every STEP_STARTED has a matching STEP_FINISHED, RUN_FINISHED is last
        let mut enc = AgUiEncoder::new();
        let mut all_events = Vec::new();

        // Step 1: inference + tool call
        all_events.extend(enc.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        }));
        all_events.extend(enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m1".into(),
        }));
        all_events.extend(enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "Let me check.".into(),
        }));
        all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallStart {
            id: "c1".into(),
            name: "weather".into(),
        }));
        all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallDelta {
            id: "c1".into(),
            args_delta: "{}".into(),
        }));
        all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallReady {
            id: "c1".into(),
            name: "weather".into(),
            arguments: json!({}),
        }));
        all_events.extend(enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::success("weather", json!({"temp": 70})),
            outcome: ToolCallOutcome::Succeeded,
        }));
        all_events.extend(enc.on_agent_event(&AgentEvent::StepEnd));

        // Step 2: summary
        all_events.extend(enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m2".into(),
        }));
        all_events.extend(enc.on_agent_event(&AgentEvent::TextDelta {
            delta: "It's 70F.".into(),
        }));
        all_events.extend(enc.on_agent_event(&AgentEvent::StepEnd));

        // Run finish
        all_events.extend(enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        }));

        // Verify ordering
        let types: Vec<&str> = all_events
            .iter()
            .map(|e| match e {
                Event::RunStarted { .. } => "RUN_STARTED",
                Event::RunFinished { .. } => "RUN_FINISHED",
                Event::StepStarted { .. } => "STEP_STARTED",
                Event::StepFinished { .. } => "STEP_FINISHED",
                Event::TextMessageStart { .. } => "TEXT_MESSAGE_START",
                Event::TextMessageContent { .. } => "TEXT_MESSAGE_CONTENT",
                Event::TextMessageEnd { .. } => "TEXT_MESSAGE_END",
                Event::ToolCallStart { .. } => "TOOL_CALL_START",
                Event::ToolCallArgs { .. } => "TOOL_CALL_ARGS",
                Event::ToolCallEnd { .. } => "TOOL_CALL_END",
                Event::ToolCallResult { .. } => "TOOL_CALL_RESULT",
                _ => "OTHER",
            })
            .collect();

        // RUN_STARTED must be first
        assert_eq!(types[0], "RUN_STARTED");

        // RUN_FINISHED must be last
        assert_eq!(*types.last().unwrap(), "RUN_FINISHED");

        // Count: 2 STEP_STARTED, 2 STEP_FINISHED
        assert_eq!(types.iter().filter(|t| **t == "STEP_STARTED").count(), 2);
        assert_eq!(types.iter().filter(|t| **t == "STEP_FINISHED").count(), 2);

        // Every STEP_STARTED has a matching STEP_FINISHED before the next STEP_STARTED
        let mut step_depth = 0i32;
        for t in &types {
            match *t {
                "STEP_STARTED" => {
                    step_depth += 1;
                    assert_eq!(step_depth, 1, "nested steps detected");
                }
                "STEP_FINISHED" => {
                    step_depth -= 1;
                    assert!(step_depth >= 0, "extra STEP_FINISHED");
                }
                _ => {}
            }
        }
        assert_eq!(step_depth, 0, "unclosed step");
    }

    #[test]
    fn error_termination_still_produces_valid_sequence() {
        let mut enc = AgUiEncoder::new();
        let mut all = Vec::new();
        all.extend(enc.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t".into(),
            run_id: "r".into(),
            parent_run_id: None,
        }));
        all.extend(enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m".into(),
        }));
        all.extend(enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t".into(),
            run_id: "r".into(),
            result: None,
            termination: TerminationReason::Error("boom".into()),
        }));
        // Should have RUN_STARTED, STEP_STARTED, RUN_ERROR
        assert!(all.iter().any(|e| matches!(e, Event::RunStarted { .. })));
        assert!(all.iter().any(|e| matches!(e, Event::StepStarted { .. })));
        assert!(all.iter().any(|e| matches!(e, Event::RunError { .. })));
    }

    #[test]
    fn suspended_termination_emits_interrupt() {
        let mut enc = AgUiEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Suspended,
        });
        let finished = events
            .iter()
            .find(|e| matches!(e, Event::RunFinished { .. }));
        assert!(finished.is_some());
        if let Some(Event::RunFinished {
            outcome, interrupt, ..
        }) = finished
        {
            assert_eq!(outcome.as_deref(), Some("interrupt"));
            assert!(interrupt.is_some());
            let int = interrupt.as_ref().unwrap();
            assert_eq!(int.reason.as_deref(), Some("tool_approval"));
        }
    }

    // ── State machine and event ordering tests ─────────────────────────

    /// Helper: extract AG-UI event type name from an Event variant.
    fn event_type_name(e: &Event) -> &'static str {
        match e {
            Event::RunStarted { .. } => "RUN_STARTED",
            Event::RunFinished { .. } => "RUN_FINISHED",
            Event::RunError { .. } => "RUN_ERROR",
            Event::StepStarted { .. } => "STEP_STARTED",
            Event::StepFinished { .. } => "STEP_FINISHED",
            Event::TextMessageStart { .. } => "TEXT_MESSAGE_START",
            Event::TextMessageContent { .. } => "TEXT_MESSAGE_CONTENT",
            Event::TextMessageEnd { .. } => "TEXT_MESSAGE_END",
            Event::ReasoningMessageStart { .. } => "REASONING_MESSAGE_START",
            Event::ReasoningMessageContent { .. } => "REASONING_MESSAGE_CONTENT",
            Event::ReasoningMessageEnd { .. } => "REASONING_MESSAGE_END",
            Event::ReasoningEncryptedValue { .. } => "REASONING_ENCRYPTED_VALUE",
            Event::ToolCallStart { .. } => "TOOL_CALL_START",
            Event::ToolCallArgs { .. } => "TOOL_CALL_ARGS",
            Event::ToolCallEnd { .. } => "TOOL_CALL_END",
            Event::ToolCallResult { .. } => "TOOL_CALL_RESULT",
            Event::StateSnapshot { .. } => "STATE_SNAPSHOT",
            Event::StateDelta { .. } => "STATE_DELTA",
            Event::MessagesSnapshot { .. } => "MESSAGES_SNAPSHOT",
            Event::ActivitySnapshot { .. } => "ACTIVITY_SNAPSHOT",
            Event::ActivityDelta { .. } => "ACTIVITY_DELTA",
            Event::Custom { .. } => "CUSTOM",
        }
    }

    /// Collect all event type names from an encoder fed a sequence of AgentEvents.
    fn collect_types(enc: &mut AgUiEncoder, inputs: &[AgentEvent]) -> Vec<&'static str> {
        let mut all = Vec::new();
        for ev in inputs {
            all.extend(enc.on_agent_event(ev).iter().map(event_type_name));
        }
        all
    }

    #[test]
    fn multi_tool_parallel_calls_in_one_step() {
        let mut enc = AgUiEncoder::new();
        let types = collect_types(
            &mut enc,
            &[
                AgentEvent::StepStart {
                    message_id: "m1".into(),
                },
                AgentEvent::TextDelta {
                    delta: "Let me search.".into(),
                },
                AgentEvent::ToolCallStart {
                    id: "c1".into(),
                    name: "search".into(),
                },
                AgentEvent::ToolCallStart {
                    id: "c2".into(),
                    name: "fetch".into(),
                },
                AgentEvent::ToolCallDone {
                    id: "c1".into(),
                    message_id: "m1".into(),
                    result: ToolResult::success("search", json!({"hits": 3})),
                    outcome: ToolCallOutcome::Succeeded,
                },
                AgentEvent::ToolCallDone {
                    id: "c2".into(),
                    message_id: "m1".into(),
                    result: ToolResult::success("fetch", json!({"status": 200})),
                    outcome: ToolCallOutcome::Succeeded,
                },
                AgentEvent::StepEnd,
            ],
        );

        // TEXT_MESSAGE_END must appear before the first TOOL_CALL_START
        let text_end_pos = types
            .iter()
            .position(|t| *t == "TEXT_MESSAGE_END")
            .expect("TEXT_MESSAGE_END missing");
        let first_tool_start_pos = types
            .iter()
            .position(|t| *t == "TOOL_CALL_START")
            .expect("TOOL_CALL_START missing");
        assert!(
            text_end_pos < first_tool_start_pos,
            "TEXT_MESSAGE_END ({text_end_pos}) must precede TOOL_CALL_START ({first_tool_start_pos})"
        );

        // Both tool results appear
        assert_eq!(
            types.iter().filter(|t| **t == "TOOL_CALL_RESULT").count(),
            2
        );
        // Two TOOL_CALL_START events
        assert_eq!(types.iter().filter(|t| **t == "TOOL_CALL_START").count(), 2);
    }

    #[test]
    fn consecutive_tool_steps_then_summary() {
        let mut enc = AgUiEncoder::new();
        let types = collect_types(
            &mut enc,
            &[
                // Step 1: tool
                AgentEvent::StepStart {
                    message_id: "m1".into(),
                },
                AgentEvent::ToolCallStart {
                    id: "c1".into(),
                    name: "search".into(),
                },
                AgentEvent::ToolCallDone {
                    id: "c1".into(),
                    message_id: "m1".into(),
                    result: ToolResult::success("search", json!("result1")),
                    outcome: ToolCallOutcome::Succeeded,
                },
                AgentEvent::StepEnd,
                // Step 2: tool
                AgentEvent::StepStart {
                    message_id: "m2".into(),
                },
                AgentEvent::ToolCallStart {
                    id: "c2".into(),
                    name: "fetch".into(),
                },
                AgentEvent::ToolCallDone {
                    id: "c2".into(),
                    message_id: "m2".into(),
                    result: ToolResult::success("fetch", json!("result2")),
                    outcome: ToolCallOutcome::Succeeded,
                },
                AgentEvent::StepEnd,
                // Step 3: summary text
                AgentEvent::StepStart {
                    message_id: "m3".into(),
                },
                AgentEvent::TextDelta {
                    delta: "Summary".into(),
                },
                AgentEvent::StepEnd,
                // Run finish
                AgentEvent::RunFinish {
                    thread_id: "t1".into(),
                    run_id: "r1".into(),
                    result: None,
                    termination: TerminationReason::NaturalEnd,
                },
            ],
        );

        assert_eq!(
            types.iter().filter(|t| **t == "STEP_STARTED").count(),
            3,
            "expected 3 STEP_STARTED"
        );
        assert_eq!(
            types.iter().filter(|t| **t == "STEP_FINISHED").count(),
            3,
            "expected 3 STEP_FINISHED"
        );

        // Verify proper nesting: no STEP_STARTED while a step is open
        let mut depth = 0i32;
        for t in &types {
            match *t {
                "STEP_STARTED" => {
                    depth += 1;
                    assert_eq!(depth, 1, "nested STEP_STARTED detected");
                }
                "STEP_FINISHED" => {
                    depth -= 1;
                    assert!(depth >= 0, "extra STEP_FINISHED");
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0, "unclosed step at end");
    }

    #[test]
    fn reasoning_then_text_does_not_auto_close_reasoning() {
        // The encoder does NOT auto-close reasoning when text starts.
        // Both reasoning and text can be open simultaneously.
        let mut enc = AgUiEncoder::new();
        enc.message_id = "m1".into();
        let types = collect_types(
            &mut enc,
            &[
                AgentEvent::StepStart {
                    message_id: "m1".into(),
                },
                AgentEvent::ReasoningDelta {
                    delta: "think".into(),
                },
                AgentEvent::TextDelta {
                    delta: "answer".into(),
                },
            ],
        );

        // Reasoning opens first
        let reasoning_start_pos = types
            .iter()
            .position(|t| *t == "REASONING_MESSAGE_START")
            .expect("REASONING_MESSAGE_START missing");
        let text_start_pos = types
            .iter()
            .position(|t| *t == "TEXT_MESSAGE_START")
            .expect("TEXT_MESSAGE_START missing");
        assert!(reasoning_start_pos < text_start_pos);

        // No REASONING_MESSAGE_END emitted yet (encoder doesn't auto-close reasoning for text)
        assert!(
            !types.contains(&"REASONING_MESSAGE_END"),
            "reasoning should NOT be auto-closed when text starts"
        );

        // Both are still open, so StepEnd should close both
        let step_end_types: Vec<&str> = enc
            .on_agent_event(&AgentEvent::StepEnd)
            .iter()
            .map(event_type_name)
            .collect();
        assert!(step_end_types.contains(&"TEXT_MESSAGE_END"));
        assert!(step_end_types.contains(&"REASONING_MESSAGE_END"));
        assert!(step_end_types.contains(&"STEP_FINISHED"));
    }

    #[test]
    fn step_end_closes_both_text_and_reasoning() {
        let mut enc = AgUiEncoder::new();
        enc.message_id = "m1".into();
        let types = collect_types(
            &mut enc,
            &[
                AgentEvent::StepStart {
                    message_id: "m1".into(),
                },
                AgentEvent::TextDelta {
                    delta: "hello".into(),
                },
                AgentEvent::ReasoningDelta {
                    delta: "thinking".into(),
                },
                AgentEvent::StepEnd,
            ],
        );

        // Both ends must appear before STEP_FINISHED
        let text_end_pos = types
            .iter()
            .position(|t| *t == "TEXT_MESSAGE_END")
            .expect("TEXT_MESSAGE_END missing");
        let reasoning_end_pos = types
            .iter()
            .position(|t| *t == "REASONING_MESSAGE_END")
            .expect("REASONING_MESSAGE_END missing");
        let step_finished_pos = types
            .iter()
            .position(|t| *t == "STEP_FINISHED")
            .expect("STEP_FINISHED missing");

        assert!(
            text_end_pos < step_finished_pos,
            "TEXT_MESSAGE_END must precede STEP_FINISHED"
        );
        assert!(
            reasoning_end_pos < step_finished_pos,
            "REASONING_MESSAGE_END must precede STEP_FINISHED"
        );
    }

    #[test]
    fn tool_call_closes_both_text_and_reasoning() {
        let mut enc = AgUiEncoder::new();
        enc.message_id = "m1".into();
        let types = collect_types(
            &mut enc,
            &[
                AgentEvent::StepStart {
                    message_id: "m1".into(),
                },
                AgentEvent::TextDelta {
                    delta: "planning".into(),
                },
                AgentEvent::ReasoningDelta {
                    delta: "hmm".into(),
                },
                AgentEvent::ToolCallStart {
                    id: "c1".into(),
                    name: "exec".into(),
                },
            ],
        );

        // Both TEXT_MESSAGE_END and REASONING_MESSAGE_END must appear before TOOL_CALL_START
        let text_end_pos = types
            .iter()
            .position(|t| *t == "TEXT_MESSAGE_END")
            .expect("TEXT_MESSAGE_END missing");
        let reasoning_end_pos = types
            .iter()
            .position(|t| *t == "REASONING_MESSAGE_END")
            .expect("REASONING_MESSAGE_END missing");
        let tool_start_pos = types
            .iter()
            .position(|t| *t == "TOOL_CALL_START")
            .expect("TOOL_CALL_START missing");

        assert!(
            text_end_pos < tool_start_pos,
            "TEXT_MESSAGE_END must precede TOOL_CALL_START"
        );
        assert!(
            reasoning_end_pos < tool_start_pos,
            "REASONING_MESSAGE_END must precede TOOL_CALL_START"
        );
    }

    #[test]
    fn suspended_termination_closes_text_and_emits_interrupt() {
        let mut enc = AgUiEncoder::new();
        enc.message_id = "m1".into();
        let types = collect_types(
            &mut enc,
            &[
                AgentEvent::StepStart {
                    message_id: "m1".into(),
                },
                AgentEvent::TextDelta {
                    delta: "working...".into(),
                },
                AgentEvent::RunFinish {
                    thread_id: "t1".into(),
                    run_id: "r1".into(),
                    result: None,
                    termination: TerminationReason::Suspended,
                },
            ],
        );

        // TEXT_MESSAGE_END must be emitted
        assert!(
            types.contains(&"TEXT_MESSAGE_END"),
            "TEXT_MESSAGE_END missing on suspended termination"
        );

        // RUN_FINISHED with outcome="interrupt"
        assert!(
            types.contains(&"RUN_FINISHED"),
            "RUN_FINISHED missing on suspended termination"
        );

        // Verify the interrupt payload on the actual event
        let mut enc2 = AgUiEncoder::new();
        enc2.message_id = "m1".into();
        enc2.on_agent_event(&AgentEvent::StepStart {
            message_id: "m1".into(),
        });
        enc2.on_agent_event(&AgentEvent::TextDelta {
            delta: "working...".into(),
        });
        let events = enc2.on_agent_event(&AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::Suspended,
        });
        let finished = events
            .iter()
            .find(|e| matches!(e, Event::RunFinished { .. }));
        if let Some(Event::RunFinished { outcome, .. }) = finished {
            assert_eq!(outcome.as_deref(), Some("interrupt"));
        } else {
            panic!("RUN_FINISHED event not found");
        }
    }

    #[test]
    fn terminal_guard_blocks_all_subsequent_events() {
        let mut enc = AgUiEncoder::new();
        let mut all = Vec::new();

        // RunStart then Error sets finished=true
        all.extend(enc.on_agent_event(&AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        }));
        all.extend(enc.on_agent_event(&AgentEvent::Error {
            message: "fatal".into(),
            code: Some("INTERNAL".into()),
        }));

        // All subsequent events must produce empty output
        assert!(
            enc.on_agent_event(&AgentEvent::TextDelta {
                delta: "ignored".into()
            })
            .is_empty(),
            "TextDelta after Error must be empty"
        );
        assert!(
            enc.on_agent_event(&AgentEvent::StepStart {
                message_id: "m1".into()
            })
            .is_empty(),
            "StepStart after Error must be empty"
        );
        assert!(
            enc.on_agent_event(&AgentEvent::ToolCallStart {
                id: "c1".into(),
                name: "search".into()
            })
            .is_empty(),
            "ToolCallStart after Error must be empty"
        );
        assert!(
            enc.on_agent_event(&AgentEvent::ReasoningDelta {
                delta: "think".into()
            })
            .is_empty(),
            "ReasoningDelta after Error must be empty"
        );
        assert!(
            enc.on_agent_event(&AgentEvent::RunFinish {
                thread_id: "t1".into(),
                run_id: "r1".into(),
                result: None,
                termination: TerminationReason::NaturalEnd,
            })
            .is_empty(),
            "RunFinish after Error must be empty"
        );

        // Verify the pre-guard events are correct
        let pre_types: Vec<&str> = all.iter().map(event_type_name).collect();
        assert_eq!(pre_types, vec!["RUN_STARTED", "RUN_ERROR"]);
    }

    #[test]
    fn empty_step_produces_only_started_and_finished() {
        let mut enc = AgUiEncoder::new();
        let types = collect_types(
            &mut enc,
            &[
                AgentEvent::StepStart {
                    message_id: "m1".into(),
                },
                AgentEvent::StepEnd,
            ],
        );

        assert_eq!(
            types,
            vec!["STEP_STARTED", "STEP_FINISHED"],
            "empty step should only produce STEP_STARTED + STEP_FINISHED"
        );
    }

    #[test]
    fn tool_call_resumed_produces_tool_call_result() {
        let mut enc = AgUiEncoder::new();
        enc.message_id = "r1".into();
        let events = enc.on_agent_event(&AgentEvent::StepStart {
            message_id: "m1".into(),
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::StepStarted { .. }))
        );

        let events = enc.on_agent_event(&AgentEvent::ToolCallResumed {
            target_id: "c1".into(),
            result: json!({"ok": true}),
        });

        assert_eq!(events.len(), 1);
        if let Event::ToolCallResult {
            tool_call_id,
            content,
            message_id,
            ..
        } = &events[0]
        {
            assert_eq!(tool_call_id, "c1");
            assert_eq!(message_id, "m1"); // message_id updated by StepStart
            let parsed: serde_json::Value = serde_json::from_str(content).unwrap();
            assert_eq!(parsed, json!({"ok": true}));
        } else {
            panic!("expected TOOL_CALL_RESULT, got {:?}", events[0]);
        }
    }

    #[test]
    fn activity_snapshot_and_delta_passthrough() {
        let mut enc = AgUiEncoder::new();

        // ActivitySnapshot
        let events = enc.on_agent_event(&AgentEvent::ActivitySnapshot {
            message_id: "a1".into(),
            activity_type: "progress".into(),
            content: json!({"pct": 50}),
            replace: Some(false),
        });
        assert_eq!(events.len(), 1);
        if let Event::ActivitySnapshot {
            message_id,
            activity_type,
            content,
            replace,
            ..
        } = &events[0]
        {
            assert_eq!(message_id, "a1");
            assert_eq!(activity_type, "progress");
            assert_eq!(content.get("pct"), Some(&json!(50)));
            assert_eq!(*replace, Some(false));
        } else {
            panic!("expected ACTIVITY_SNAPSHOT");
        }

        // ActivityDelta
        let events = enc.on_agent_event(&AgentEvent::ActivityDelta {
            message_id: "a1".into(),
            activity_type: "progress".into(),
            patch: vec![json!({"op": "replace", "path": "/pct", "value": 75})],
        });
        assert_eq!(events.len(), 1);
        if let Event::ActivityDelta {
            message_id,
            activity_type,
            patch,
            ..
        } = &events[0]
        {
            assert_eq!(message_id, "a1");
            assert_eq!(activity_type, "progress");
            assert_eq!(patch.len(), 1);
            assert_eq!(patch[0]["op"], "replace");
        } else {
            panic!("expected ACTIVITY_DELTA");
        }
    }

    #[test]
    fn messages_snapshot_passthrough() {
        let mut enc = AgUiEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::MessagesSnapshot {
            messages: vec![json!({"role": "user", "content": "hi"})],
        });

        assert_eq!(events.len(), 1);
        if let Event::MessagesSnapshot { messages, .. } = &events[0] {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["role"], "user");
            assert_eq!(messages[0]["content"], "hi");
        } else {
            panic!("expected MESSAGES_SNAPSHOT");
        }
    }

    #[test]
    fn tool_call_done_success_without_metadata_serializes_data_only() {
        let mut enc = AgUiEncoder::new();
        let result = ToolResult::success("search", json!({"items": [1, 2]}));
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result,
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(events.len(), 1);
        if let Event::ToolCallResult { content, .. } = &events[0] {
            let parsed: serde_json::Value = serde_json::from_str(content).unwrap();
            assert_eq!(parsed, json!({"items": [1, 2]}));
        } else {
            panic!("expected ToolCallResult");
        }
    }

    #[test]
    fn tool_call_done_success_with_metadata_includes_both() {
        let mut enc = AgUiEncoder::new();
        let mut result = ToolResult::success("search", json!({"items": [1]}));
        result
            .metadata
            .insert("mcp.server".into(), json!("my-server"));
        result
            .metadata
            .insert("mcp.ui.content".into(), json!({"type": "image"}));
        let events = enc.on_agent_event(&AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result,
            outcome: ToolCallOutcome::Succeeded,
        });
        assert_eq!(events.len(), 1);
        if let Event::ToolCallResult { content, .. } = &events[0] {
            let parsed: serde_json::Value = serde_json::from_str(content).unwrap();
            assert_eq!(parsed["data"], json!({"items": [1]}));
            assert_eq!(parsed["metadata"]["mcp.server"], json!("my-server"));
            assert_eq!(
                parsed["metadata"]["mcp.ui.content"],
                json!({"type": "image"})
            );
        } else {
            panic!("expected ToolCallResult");
        }
    }

    #[test]
    fn inference_complete_is_dropped() {
        let mut enc = AgUiEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::InferenceComplete {
            model: "gpt-5.4".into(),
            usage: Some(awaken_contract::contract::inference::TokenUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                total_tokens: Some(150),
                ..Default::default()
            }),
            duration_ms: 1000,
        });

        assert!(
            events.is_empty(),
            "InferenceComplete must produce empty Vec (AG-UI has no matching event type)"
        );
    }

    #[test]
    fn tool_call_stream_delta_emits_activity_delta() {
        let mut enc = AgUiEncoder::new();
        let events = enc.on_agent_event(&AgentEvent::ToolCallStreamDelta {
            id: "c1".into(),
            name: "json_render".into(),
            delta: "{\"type\":\"text\"".into(),
        });
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::ActivityDelta {
            message_id,
            activity_type,
            ..
        } if message_id == "c1" && activity_type == "tool-stream-output"));

        // Verify the patch contains the delta
        if let Event::ActivityDelta { patch, .. } = &events[0] {
            assert_eq!(patch.len(), 1);
            assert_eq!(patch[0]["op"], "add");
            assert_eq!(patch[0]["value"], "{\"type\":\"text\"");
        } else {
            panic!("expected ActivityDelta");
        }
    }

    #[test]
    fn tool_call_stream_delta_in_full_lifecycle() {
        let mut enc = AgUiEncoder::new();
        let types = collect_types(
            &mut enc,
            &[
                AgentEvent::RunStart {
                    thread_id: "t1".into(),
                    run_id: "r1".into(),
                    parent_run_id: None,
                },
                AgentEvent::StepStart {
                    message_id: "m1".into(),
                },
                AgentEvent::ToolCallStart {
                    id: "c1".into(),
                    name: "json_render".into(),
                },
                AgentEvent::ToolCallDelta {
                    id: "c1".into(),
                    args_delta: "{}".into(),
                },
                AgentEvent::ToolCallReady {
                    id: "c1".into(),
                    name: "json_render".into(),
                    arguments: json!({}),
                },
                // Stream deltas during execution
                AgentEvent::ToolCallStreamDelta {
                    id: "c1".into(),
                    name: "json_render".into(),
                    delta: "chunk1".into(),
                },
                AgentEvent::ToolCallStreamDelta {
                    id: "c1".into(),
                    name: "json_render".into(),
                    delta: "chunk2".into(),
                },
                AgentEvent::ToolCallDone {
                    id: "c1".into(),
                    message_id: "m1".into(),
                    result: ToolResult::success("json_render", json!({"rendered": true})),
                    outcome: ToolCallOutcome::Succeeded,
                },
                AgentEvent::StepEnd,
                AgentEvent::RunFinish {
                    thread_id: "t1".into(),
                    run_id: "r1".into(),
                    result: None,
                    termination: TerminationReason::NaturalEnd,
                },
            ],
        );

        // Two ACTIVITY_DELTA events from stream deltas
        assert_eq!(
            types.iter().filter(|t| **t == "ACTIVITY_DELTA").count(),
            2,
            "expected 2 ACTIVITY_DELTA from stream deltas"
        );
        // Standard lifecycle events still present
        assert!(types.contains(&"RUN_STARTED"));
        assert!(types.contains(&"TOOL_CALL_START"));
        assert!(types.contains(&"TOOL_CALL_END"));
        assert!(types.contains(&"TOOL_CALL_RESULT"));
        assert!(types.contains(&"RUN_FINISHED"));
    }
}
