//! AG-UI encoder: maps AgentEvent to AG-UI Event.

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::tool::ToolStatus;
use awaken_contract::contract::transport::Transcoder;
use std::collections::HashMap;

use super::types::{BaseEvent, Event, Role};

/// Stateful AG-UI protocol encoder.
///
/// Tracks text message lifecycle (start/end), step naming, and reasoning blocks.
#[derive(Debug)]
pub struct AgUiEncoder {
    message_id: String,
    text_open: bool,
    step_counter: u32,
    reasoning_open: bool,
    finished: bool,
}

impl AgUiEncoder {
    pub fn new() -> Self {
        Self {
            message_id: String::new(),
            text_open: false,
            step_counter: 0,
            reasoning_open: false,
            finished: false,
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
        if self.finished {
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
                    let content = serde_json::to_string(&result.data).unwrap_or_default();
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
                self.finished = true;
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
                self.finished = true;
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
}
