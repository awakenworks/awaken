//! Agent loop streaming events and IO contracts.

use super::inference::TokenUsage;
use super::lifecycle::TerminationReason;
use super::suspension::ToolCallOutcome;
use super::tool::ToolResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Agent loop events for streaming execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Run started.
    RunStart {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_run_id: Option<String>,
    },

    /// Run finished.
    RunFinish {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        termination: TerminationReason,
    },

    /// LLM text delta.
    TextDelta { delta: String },

    /// LLM reasoning delta.
    ReasoningDelta { delta: String },

    /// Tool call started.
    ToolCallStart { id: String, name: String },

    /// Tool call arguments delta.
    ToolCallDelta { id: String, args_delta: String },

    /// Tool call input is complete.
    ToolCallReady {
        id: String,
        name: String,
        arguments: Value,
    },

    /// Tool call completed.
    ToolCallDone {
        id: String,
        message_id: String,
        result: ToolResult,
        outcome: ToolCallOutcome,
    },

    /// Tool call streaming output delta (incremental text from tool execution).
    ToolCallStreamDelta {
        id: String,
        name: String,
        delta: String,
    },

    /// Encrypted reasoning delta (opaque token).
    ReasoningEncryptedValue { encrypted_value: String },

    /// Snapshot of all messages in the thread.
    MessagesSnapshot { messages: Vec<Value> },

    /// Activity state snapshot (e.g. tool progress).
    ActivitySnapshot {
        message_id: String,
        activity_type: String,
        content: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        replace: Option<bool>,
    },

    /// Incremental activity update.
    ActivityDelta {
        message_id: String,
        activity_type: String,
        patch: Vec<Value>,
    },

    /// Tool call resumed from suspension.
    ToolCallResumed { target_id: String, result: Value },

    /// Step started.
    StepStart {
        #[serde(default)]
        message_id: String,
    },

    /// Step completed.
    StepEnd,

    /// LLM inference completed.
    InferenceComplete {
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        duration_ms: u64,
    },

    /// State snapshot.
    StateSnapshot { snapshot: Value },

    /// State delta.
    StateDelta { delta: Vec<Value> },

    /// Error occurred.
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
}

impl AgentEvent {
    /// Extract the response text from a `RunFinish` result value.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_contract::contract::event::AgentEvent;
    /// use serde_json::json;
    ///
    /// let result = Some(json!({"response": "Hello world"}));
    /// let text = AgentEvent::extract_response(&result);
    /// assert_eq!(text, "Hello world");
    ///
    /// let empty = AgentEvent::extract_response(&None);
    /// assert_eq!(empty, "");
    /// ```
    pub fn extract_response(result: &Option<Value>) -> String {
        result
            .as_ref()
            .and_then(|v| v.get("response"))
            .and_then(|r| r.as_str())
            .unwrap_or_default()
            .to_string()
    }
}

/// Output stream type alias for a run.
pub type RunOutput = futures::stream::BoxStream<'static, AgentEvent>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_start_serde_roundtrip() {
        let event = AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"run_start\""));
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentEvent::RunStart { .. }));
    }

    #[test]
    fn run_finish_serde_roundtrip() {
        let event = AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: Some(json!({"response": "hello"})),
            termination: TerminationReason::NaturalEnd,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::RunFinish {
            result,
            termination,
            ..
        } = parsed
        {
            assert_eq!(AgentEvent::extract_response(&result), "hello");
            assert_eq!(termination, TerminationReason::NaturalEnd);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn text_delta_serde_roundtrip() {
        let event = AgentEvent::TextDelta {
            delta: "hello ".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentEvent::TextDelta { delta } if delta == "hello "));
    }

    #[test]
    fn reasoning_delta_serde_roundtrip() {
        let event = AgentEvent::ReasoningDelta {
            delta: "think".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentEvent::ReasoningDelta { delta } if delta == "think"));
    }

    #[test]
    fn tool_call_start_delta_and_ready_roundtrip() {
        let start = AgentEvent::ToolCallStart {
            id: "c1".into(),
            name: "search".into(),
        };
        let start_json = serde_json::to_string(&start).unwrap();
        let start_parsed: AgentEvent = serde_json::from_str(&start_json).unwrap();
        assert!(
            matches!(start_parsed, AgentEvent::ToolCallStart { id, name } if id == "c1" && name == "search")
        );

        let delta = AgentEvent::ToolCallDelta {
            id: "c1".into(),
            args_delta: "{\"q\":\"rust\"}".into(),
        };
        let delta_json = serde_json::to_string(&delta).unwrap();
        let delta_parsed: AgentEvent = serde_json::from_str(&delta_json).unwrap();
        assert!(
            matches!(delta_parsed, AgentEvent::ToolCallDelta { id, args_delta } if id == "c1" && args_delta.contains("rust"))
        );

        let ready = AgentEvent::ToolCallReady {
            id: "c1".into(),
            name: "search".into(),
            arguments: json!({"q": "rust"}),
        };
        let ready_json = serde_json::to_string(&ready).unwrap();
        let ready_parsed: AgentEvent = serde_json::from_str(&ready_json).unwrap();
        assert!(
            matches!(ready_parsed, AgentEvent::ToolCallReady { id, name, arguments } if id == "c1" && name == "search" && arguments["q"] == "rust")
        );
    }

    #[test]
    fn tool_call_done_serde_roundtrip() {
        let event = AgentEvent::ToolCallDone {
            id: "c1".into(),
            message_id: "m1".into(),
            result: ToolResult::success("calc", json!(42)),
            outcome: ToolCallOutcome::Succeeded,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentEvent::ToolCallDone { .. }));
    }

    #[test]
    fn step_end_serde_roundtrip() {
        let event = AgentEvent::StepEnd;
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentEvent::StepEnd));
    }

    #[test]
    fn step_start_serde_roundtrip() {
        let event = AgentEvent::StepStart {
            message_id: "m1".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentEvent::StepStart { message_id } if message_id == "m1"));
    }

    #[test]
    fn inference_complete_serde_roundtrip() {
        let event = AgentEvent::InferenceComplete {
            model: "gpt-4o".into(),
            usage: Some(TokenUsage {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                total_tokens: Some(150),
                ..Default::default()
            }),
            duration_ms: 1234,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::InferenceComplete {
            model,
            usage,
            duration_ms,
        } = parsed
        {
            assert_eq!(model, "gpt-4o");
            assert_eq!(usage.unwrap().total_tokens, Some(150));
            assert_eq!(duration_ms, 1234);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn error_event_serde_roundtrip() {
        let event = AgentEvent::Error {
            message: "something failed".into(),
            code: Some("INTERNAL".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, AgentEvent::Error { .. }));
    }

    #[test]
    fn extract_response_from_none() {
        assert_eq!(AgentEvent::extract_response(&None), "");
    }

    #[test]
    fn extract_response_from_missing_field() {
        assert_eq!(
            AgentEvent::extract_response(&Some(json!({"other": "data"}))),
            ""
        );
    }

    #[test]
    fn extract_response_from_non_string_field() {
        assert_eq!(
            AgentEvent::extract_response(&Some(json!({"response": {"text": "hello"}}))),
            ""
        );
    }

    #[test]
    fn state_snapshot_and_delta_roundtrip() {
        let snapshot = AgentEvent::StateSnapshot {
            snapshot: json!({"messages": 2}),
        };
        let snapshot_json = serde_json::to_string(&snapshot).unwrap();
        let snapshot_parsed: AgentEvent = serde_json::from_str(&snapshot_json).unwrap();
        assert!(
            matches!(snapshot_parsed, AgentEvent::StateSnapshot { snapshot } if snapshot["messages"] == 2)
        );

        let delta = AgentEvent::StateDelta {
            delta: vec![json!({"op": "replace", "path": "/messages", "value": 3})],
        };
        let delta_json = serde_json::to_string(&delta).unwrap();
        let delta_parsed: AgentEvent = serde_json::from_str(&delta_json).unwrap();
        assert!(
            matches!(delta_parsed, AgentEvent::StateDelta { delta } if delta.len() == 1 && delta[0]["op"] == "replace")
        );
    }

    #[test]
    fn error_event_omits_none_code() {
        let json = serde_json::to_string(&AgentEvent::Error {
            message: "failed".into(),
            code: None,
        })
        .unwrap();

        assert!(!json.contains("code"));
    }

    #[test]
    fn all_event_types_serialize() {
        let events: Vec<AgentEvent> = vec![
            AgentEvent::RunStart {
                thread_id: "t".into(),
                run_id: "r".into(),
                parent_run_id: None,
            },
            AgentEvent::RunFinish {
                thread_id: "t".into(),
                run_id: "r".into(),
                result: None,
                termination: TerminationReason::NaturalEnd,
            },
            AgentEvent::TextDelta { delta: "x".into() },
            AgentEvent::ReasoningDelta { delta: "y".into() },
            AgentEvent::ToolCallStart {
                id: "c".into(),
                name: "t".into(),
            },
            AgentEvent::ToolCallDelta {
                id: "c".into(),
                args_delta: "{}".into(),
            },
            AgentEvent::ToolCallReady {
                id: "c".into(),
                name: "t".into(),
                arguments: json!({}),
            },
            AgentEvent::ToolCallDone {
                id: "c".into(),
                message_id: "m".into(),
                result: ToolResult::success("t", json!(null)),
                outcome: ToolCallOutcome::Succeeded,
            },
            AgentEvent::ToolCallStreamDelta {
                id: "c".into(),
                name: "t".into(),
                delta: "chunk".into(),
            },
            AgentEvent::ReasoningEncryptedValue {
                encrypted_value: "enc".into(),
            },
            AgentEvent::MessagesSnapshot {
                messages: vec![json!({"role": "user"})],
            },
            AgentEvent::ActivitySnapshot {
                message_id: "m".into(),
                activity_type: "tool".into(),
                content: json!({}),
                replace: None,
            },
            AgentEvent::ActivityDelta {
                message_id: "m".into(),
                activity_type: "tool".into(),
                patch: vec![json!({"op": "add"})],
            },
            AgentEvent::ToolCallResumed {
                target_id: "c".into(),
                result: json!({}),
            },
            AgentEvent::StepStart {
                message_id: "m".into(),
            },
            AgentEvent::StepEnd,
            AgentEvent::InferenceComplete {
                model: "m".into(),
                usage: None,
                duration_ms: 0,
            },
            AgentEvent::StateSnapshot {
                snapshot: json!({}),
            },
            AgentEvent::StateDelta { delta: vec![] },
            AgentEvent::Error {
                message: "err".into(),
                code: None,
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let _parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn tool_call_stream_delta_serde_roundtrip() {
        let event = AgentEvent::ToolCallStreamDelta {
            id: "c1".into(),
            name: "json_render".into(),
            delta: "{\"type\":\"text\"".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"tool_call_stream_delta\""));
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, AgentEvent::ToolCallStreamDelta { id, name, delta } if id == "c1" && name == "json_render" && delta.contains("text"))
        );
    }

    #[test]
    fn reasoning_encrypted_value_serde_roundtrip() {
        let event = AgentEvent::ReasoningEncryptedValue {
            encrypted_value: "opaque-token-abc".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event_type\":\"reasoning_encrypted_value\""));
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, AgentEvent::ReasoningEncryptedValue { encrypted_value } if encrypted_value == "opaque-token-abc")
        );
    }

    #[test]
    fn messages_snapshot_serde_roundtrip() {
        let event = AgentEvent::MessagesSnapshot {
            messages: vec![json!({"role": "user", "content": "hi"})],
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::MessagesSnapshot { messages } = parsed {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["role"], "user");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn activity_snapshot_serde_roundtrip() {
        let event = AgentEvent::ActivitySnapshot {
            message_id: "m1".into(),
            activity_type: "tool_progress".into(),
            content: json!({"percent": 50}),
            replace: Some(true),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::ActivitySnapshot {
            message_id,
            activity_type,
            content,
            replace,
        } = parsed
        {
            assert_eq!(message_id, "m1");
            assert_eq!(activity_type, "tool_progress");
            assert_eq!(content["percent"], 50);
            assert_eq!(replace, Some(true));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn activity_delta_serde_roundtrip() {
        let event = AgentEvent::ActivityDelta {
            message_id: "m1".into(),
            activity_type: "tool_progress".into(),
            patch: vec![json!({"op": "replace", "path": "/percent", "value": 75})],
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::ActivityDelta {
            message_id,
            activity_type,
            patch,
        } = parsed
        {
            assert_eq!(message_id, "m1");
            assert_eq!(activity_type, "tool_progress");
            assert_eq!(patch.len(), 1);
            assert_eq!(patch[0]["op"], "replace");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn tool_call_resumed_serde_roundtrip() {
        let event = AgentEvent::ToolCallResumed {
            target_id: "c1".into(),
            result: json!({"approved": true}),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::ToolCallResumed { target_id, result } = parsed {
            assert_eq!(target_id, "c1");
            assert_eq!(result["approved"], true);
        } else {
            panic!("wrong variant");
        }
    }

    // ── Wire format detail tests ──

    #[test]
    fn run_start_wire_format_has_event_type() {
        let event = AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: Some("parent".into()),
        };
        let wire: Value = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["event_type"], "run_start");
        assert_eq!(wire["run_id"], "r1");
        assert_eq!(wire["thread_id"], "t1");
        assert_eq!(wire["parent_run_id"], "parent");
    }

    #[test]
    fn run_finish_wire_format() {
        let event = AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: Some(json!({"response": "hello"})),
            termination: TerminationReason::NaturalEnd,
        };
        let wire: Value = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["event_type"], "run_finish");
        assert_eq!(wire["run_id"], "r1");
        assert_eq!(wire["thread_id"], "t1");
    }

    #[test]
    fn run_start_omits_null_parent_run_id() {
        let event = AgentEvent::RunStart {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            parent_run_id: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("parent_run_id"));
    }

    #[test]
    fn run_finish_omits_null_result() {
        let event = AgentEvent::RunFinish {
            thread_id: "t1".into(),
            run_id: "r1".into(),
            result: None,
            termination: TerminationReason::NaturalEnd,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("result"));
    }

    #[test]
    fn activity_snapshot_omits_null_replace() {
        let event = AgentEvent::ActivitySnapshot {
            message_id: "m1".into(),
            activity_type: "thinking".into(),
            content: json!({}),
            replace: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("replace"));
    }

    #[test]
    fn inference_complete_omits_null_usage() {
        let event = AgentEvent::InferenceComplete {
            model: "gpt-4".into(),
            usage: None,
            duration_ms: 500,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("usage"));
    }

    #[test]
    fn all_event_types_deserialize_from_serialized() {
        let events: Vec<AgentEvent> = vec![
            AgentEvent::RunStart {
                thread_id: "t".into(),
                run_id: "r".into(),
                parent_run_id: None,
            },
            AgentEvent::RunFinish {
                thread_id: "t".into(),
                run_id: "r".into(),
                result: None,
                termination: TerminationReason::NaturalEnd,
            },
            AgentEvent::TextDelta { delta: "hi".into() },
            AgentEvent::ReasoningDelta {
                delta: "think".into(),
            },
            AgentEvent::ReasoningEncryptedValue {
                encrypted_value: "enc".into(),
            },
            AgentEvent::ToolCallStart {
                id: "c".into(),
                name: "t".into(),
            },
            AgentEvent::ToolCallDelta {
                id: "c".into(),
                args_delta: "{}".into(),
            },
            AgentEvent::ToolCallReady {
                id: "c".into(),
                name: "t".into(),
                arguments: json!({}),
            },
            AgentEvent::ToolCallDone {
                id: "c".into(),
                message_id: "m".into(),
                result: ToolResult::success("t", json!(null)),
                outcome: ToolCallOutcome::Succeeded,
            },
            AgentEvent::ToolCallStreamDelta {
                id: "c".into(),
                name: "t".into(),
                delta: "streaming".into(),
            },
            AgentEvent::StepStart {
                message_id: "m".into(),
            },
            AgentEvent::StepEnd,
            AgentEvent::InferenceComplete {
                model: "gpt".into(),
                usage: None,
                duration_ms: 100,
            },
            AgentEvent::StateSnapshot {
                snapshot: json!({}),
            },
            AgentEvent::StateDelta {
                delta: vec![json!({})],
            },
            AgentEvent::MessagesSnapshot {
                messages: vec![json!({})],
            },
            AgentEvent::ActivitySnapshot {
                message_id: "m".into(),
                activity_type: "a".into(),
                content: json!({}),
                replace: None,
            },
            AgentEvent::ActivityDelta {
                message_id: "m".into(),
                activity_type: "a".into(),
                patch: vec![],
            },
            AgentEvent::ToolCallResumed {
                target_id: "t".into(),
                result: json!({}),
            },
            AgentEvent::Error {
                message: "oops".into(),
                code: Some("LLM_ERROR".into()),
            },
        ];

        for event in &events {
            let wire = serde_json::to_value(event).expect("serialize");
            let restored: AgentEvent = serde_json::from_value(wire).expect("deserialize");
            // Verify the variant tag survived roundtrip
            let original_json = serde_json::to_value(event).unwrap();
            let restored_json = serde_json::to_value(&restored).unwrap();
            assert_eq!(
                original_json["event_type"], restored_json["event_type"],
                "event_type mismatch"
            );
        }
    }
}
