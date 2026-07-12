//! The AG-UI live-channel transcoder: turn the engine's best-effort progress
//! (`stream::Kind`) into the in-flight *prefix* of an AG-UI event stream —
//! `RUN_STARTED`, streamed `TEXT_MESSAGE_*`, and `TOOL_CALL_START`/`TOOL_CALL_ARGS`
//! — as a turn runs. It deliberately does **not** emit the closing frames
//! (`TOOL_CALL_END`, `TOOL_CALL_RESULT`, `RUN_FINISHED`); those come from the
//! committed step (see [`crate::encoder::encode_close`]) so the live channel never
//! becomes the source of truth (G10/G13). The channel adapter is the shared
//! [`awaken_protocol_transport::ChannelStreamSink`].
//!
//! Tool arguments arrive as *cumulative* snapshots (genai hands a `Value::String`
//! of the JSON accumulated so far), so the transcoder tracks a per-call byte
//! offset and emits only the newly-appended suffix as each `TOOL_CALL_ARGS` delta.

use std::collections::HashMap;

use awaken_agent_contract::stream::event::Kind;

use crate::types::AgUiEvent;

/// Stateful transcoder from the live `stream::Kind` channel to the in-flight
/// prefix of an AG-UI event stream. One instance per streamed turn; it holds the
/// stream's thread/run ids (AG-UI frames carry them) and per-tool arg offsets.
pub struct AgUiLiveTranscoder {
    thread_id: String,
    run_id: String,
    started: bool,
    open_text: Option<String>,
    text_seq: usize,
    tools: HashMap<String, usize>,
}

impl AgUiLiveTranscoder {
    pub fn new(thread_id: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            run_id: run_id.into(),
            started: false,
            open_text: None,
            text_seq: 0,
            tools: HashMap::new(),
        }
    }

    pub fn transcode(&mut self, kind: &Kind) -> Vec<AgUiEvent> {
        match kind {
            Kind::RunStarted => {
                if self.started {
                    Vec::new()
                } else {
                    self.started = true;
                    vec![AgUiEvent::RunStarted {
                        thread_id: self.thread_id.clone(),
                        run_id: self.run_id.clone(),
                    }]
                }
            }
            Kind::OutputText { text } => {
                let mut out = Vec::new();
                let id = match &self.open_text {
                    Some(id) => id.clone(),
                    None => {
                        let id = format!("{}-msg-{}", self.run_id, self.text_seq);
                        self.text_seq += 1;
                        self.open_text = Some(id.clone());
                        out.push(AgUiEvent::TextMessageStart {
                            message_id: id.clone(),
                            role: "assistant".to_string(),
                        });
                        id
                    }
                };
                out.push(AgUiEvent::TextMessageContent {
                    message_id: id,
                    delta: text.clone(),
                });
                out
            }
            Kind::ToolCall {
                call_id,
                tool_id,
                arguments,
            } => {
                let mut out = self.close_text();
                if !self.tools.contains_key(call_id) {
                    self.tools.insert(call_id.clone(), 0);
                    out.push(AgUiEvent::ToolCallStart {
                        tool_call_id: call_id.clone(),
                        tool_call_name: tool_id.clone(),
                    });
                }
                if let Some(s) = arguments.as_str() {
                    let sent = self.tools.get_mut(call_id).expect("just inserted");
                    if s.len() > *sent && s.is_char_boundary(*sent) {
                        out.push(AgUiEvent::ToolCallArgs {
                            tool_call_id: call_id.clone(),
                            delta: s[*sent..].to_string(),
                        });
                        *sent = s.len();
                    }
                }
                out
            }
            Kind::Waiting { .. }
            | Kind::Continuation { .. }
            | Kind::RunFinished
            | Kind::RunFailed { .. } => self.close_text(),
        }
    }

    pub fn has_streamed(&self) -> bool {
        self.started
    }

    fn close_text(&mut self) -> Vec<AgUiEvent> {
        match self.open_text.take() {
            Some(message_id) => vec![AgUiEvent::TextMessageEnd { message_id }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(seq: &[Kind]) -> Vec<AgUiEvent> {
        let mut tc = AgUiLiveTranscoder::new("t1", "r1");
        seq.iter().flat_map(|k| tc.transcode(k)).collect()
    }

    #[test]
    fn streams_tool_args_as_suffix_deltas() {
        let events = run(&[
            Kind::RunStarted,
            Kind::OutputText {
                text: "reading".into(),
            },
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!(""),
            },
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!("{\"path\":"),
            },
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!("{\"path\":\"x\"}"),
            },
            Kind::RunFinished,
        ]);

        // Run start, a bracketed text message, then the tool call streamed as
        // suffix deltas — and no closing frames (those are the committed tail).
        assert_eq!(
            events.first(),
            Some(&AgUiEvent::RunStarted {
                thread_id: "t1".into(),
                run_id: "r1".into(),
            })
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgUiEvent::TextMessageEnd { .. }))
        );
        let deltas: String = events
            .iter()
            .filter_map(|e| match e {
                AgUiEvent::ToolCallArgs {
                    tool_call_id,
                    delta,
                } if tool_call_id == "c1" => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, "{\"path\":\"x\"}");
        assert!(events.iter().all(|e| !matches!(
            e,
            AgUiEvent::ToolCallEnd { .. } | AgUiEvent::RunFinished { .. }
        )));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgUiEvent::ToolCallStart { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn a_repeated_run_started_is_idempotent() {
        let events = run(&[Kind::RunStarted, Kind::RunStarted]);
        assert_eq!(
            events,
            vec![AgUiEvent::RunStarted {
                thread_id: "t1".into(),
                run_id: "r1".into()
            }]
        );
    }

    #[test]
    fn concurrent_tool_calls_stream_independently_by_call_id() {
        let events = run(&[
            Kind::RunStarted,
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!(""),
            },
            Kind::ToolCall {
                call_id: "c2".into(),
                tool_id: "write".into(),
                arguments: json!(""),
            },
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!("{\"a\":1}"),
            },
            Kind::ToolCall {
                call_id: "c2".into(),
                tool_id: "write".into(),
                arguments: json!("{\"b\":2}"),
            },
        ]);
        assert!(events.contains(&AgUiEvent::ToolCallStart {
            tool_call_id: "c1".into(),
            tool_call_name: "read".into(),
        }));
        assert!(events.contains(&AgUiEvent::ToolCallStart {
            tool_call_id: "c2".into(),
            tool_call_name: "write".into(),
        }));
        assert!(events.contains(&AgUiEvent::ToolCallArgs {
            tool_call_id: "c1".into(),
            delta: "{\"a\":1}".into(),
        }));
        assert!(events.contains(&AgUiEvent::ToolCallArgs {
            tool_call_id: "c2".into(),
            delta: "{\"b\":2}".into(),
        }));
    }

    #[test]
    fn text_reopens_with_a_fresh_id_after_a_tool_call() {
        let events = run(&[
            Kind::RunStarted,
            Kind::OutputText { text: "hi".into() },
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!(""),
            },
            Kind::OutputText {
                text: "more".into(),
            },
        ]);
        assert!(events.contains(&AgUiEvent::TextMessageEnd {
            message_id: "r1-msg-0".into()
        }));
        assert!(events.contains(&AgUiEvent::TextMessageStart {
            message_id: "r1-msg-1".into(),
            role: "assistant".into(),
        }));
    }

    #[test]
    fn a_snapshot_boundary_mid_codepoint_is_skipped_without_panicking() {
        // The tracked byte offset falling inside a multibyte char must be skipped
        // (the `is_char_boundary` guard), never sliced — else `s[offset..]` panics.
        let events = run(&[
            Kind::RunStarted,
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "t".into(),
                arguments: json!("a"),
            },
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "t".into(),
                arguments: json!("é"),
            },
        ]);
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgUiEvent::ToolCallArgs { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["a"]);
    }
}
