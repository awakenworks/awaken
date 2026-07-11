//! The live-channel machinery: stream the engine's best-effort progress
//! (`stream::Kind`) to AI SDK UI Message Stream parts as a turn runs.
//!
//! Two pieces, both boundary glue:
//!
//! - [`ChannelStreamSink`] is a [`StreamSink`] adapter that forwards the engine's
//!   live events onto an mpsc channel the router drains — a second adapter of the
//!   same port `MemoryStreamSink` implements, so the engine is untouched.
//! - [`LiveTranscoder`] transcodes the live `stream::Kind` sequence into the
//!   in-flight *prefix* of a UI Message Stream: `start`/`start-step`, streamed
//!   `text-*`, and `tool-input-start`/`tool-input-delta`. It deliberately does
//!   **not** emit the authoritative tail (`tool-input-available`, tool output,
//!   `finish`) — that comes from the committed [`StepOutcome`] so the live channel
//!   never becomes the source of truth (G10/G13).
//!
//! The tool-argument fragments arrive as *cumulative* snapshots (genai hands a
//! `Value::String` of the JSON accumulated so far), so the transcoder tracks a
//! per-call byte offset and emits only the newly-appended suffix as each
//! `inputTextDelta`.

use std::collections::HashMap;

use async_trait::async_trait;
use awaken_agent_contract::stream::event::{Event, Kind};
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink as StreamSink};
use tokio::sync::mpsc::UnboundedSender;

use crate::types::UIStreamEvent;

/// A [`StreamSink`] that forwards each live event's `kind` onto an mpsc channel.
/// Best-effort by contract: once the receiver is dropped, `send` reports `Closed`
/// and the engine swallows it (the committed turn is still authoritative).
pub struct ChannelStreamSink {
    tx: UnboundedSender<Kind>,
}

impl ChannelStreamSink {
    pub fn new(tx: UnboundedSender<Kind>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl StreamSink for ChannelStreamSink {
    async fn send(&self, event: Event) -> Result<(), SinkError> {
        self.tx.send(event.kind).map_err(|_| SinkError::Closed)
    }
}

/// Per-call progress: how many bytes of the cumulative argument string we have
/// already emitted as deltas.
#[derive(Default)]
struct ToolProgress {
    sent_len: usize,
}

/// Stateful transcoder from the live `stream::Kind` channel to the in-flight
/// prefix of an AI SDK UI Message Stream. One instance per streamed turn.
#[derive(Default)]
pub struct LiveTranscoder {
    started: bool,
    /// The id of the open text block, if a text run is currently streaming.
    open_text: Option<String>,
    text_seq: usize,
    tools: HashMap<String, ToolProgress>,
}

impl LiveTranscoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Transcode one live kind into zero or more UI Message Stream parts.
    pub fn transcode(&mut self, kind: &Kind) -> Vec<UIStreamEvent> {
        match kind {
            Kind::RunStarted => {
                if self.started {
                    Vec::new()
                } else {
                    self.started = true;
                    vec![UIStreamEvent::Start, UIStreamEvent::StartStep]
                }
            }
            Kind::OutputText { text } => {
                let mut out = Vec::new();
                let id = match &self.open_text {
                    Some(id) => id.clone(),
                    None => {
                        let id = format!("txt-{}", self.text_seq);
                        self.text_seq += 1;
                        self.open_text = Some(id.clone());
                        out.push(UIStreamEvent::TextStart { id: id.clone() });
                        id
                    }
                };
                out.push(UIStreamEvent::TextDelta {
                    id,
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
                    self.tools.insert(call_id.clone(), ToolProgress::default());
                    out.push(UIStreamEvent::ToolInputStart {
                        tool_call_id: call_id.clone(),
                        tool_name: tool_id.clone(),
                    });
                }
                // Arguments arrive as a cumulative `Value::String`; emit only the
                // newly-appended suffix. A non-string or non-continuation snapshot
                // is skipped (best-effort — the committed `tool-input-available`
                // carries the authoritative parsed input).
                if let Some(s) = arguments.as_str() {
                    let progress = self.tools.get_mut(call_id).expect("just inserted");
                    if s.len() > progress.sent_len && s.is_char_boundary(progress.sent_len) {
                        out.push(UIStreamEvent::ToolInputDelta {
                            tool_call_id: call_id.clone(),
                            input_text_delta: s[progress.sent_len..].to_string(),
                        });
                        progress.sent_len = s.len();
                    }
                }
                out
            }
            // Terminal / control kinds close any open text run but never emit the
            // authoritative `finish` — the committed `StepOutcome` owns that.
            Kind::Waiting { .. }
            | Kind::Continuation { .. }
            | Kind::RunFinished
            | Kind::RunFailed { .. } => self.close_text(),
        }
    }

    /// True once any live part has been emitted, so the router knows whether the
    /// stream already carries the `start`/`start-step` prefix (and must append the
    /// committed tail) or saw no live events (and must fall back to the full
    /// committed projection).
    pub fn has_streamed(&self) -> bool {
        self.started
    }

    /// Whether a tool call was streamed live under `call_id` (so the committed
    /// projection can transition the open `input-streaming` part instead of
    /// re-opening one).
    pub fn streamed_tool(&self, call_id: &str) -> bool {
        self.tools.contains_key(call_id)
    }

    fn close_text(&mut self) -> Vec<UIStreamEvent> {
        match self.open_text.take() {
            Some(id) => vec![UIStreamEvent::TextEnd { id }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn run(seq: &[Kind]) -> Vec<UIStreamEvent> {
        let mut tc = LiveTranscoder::new();
        seq.iter().flat_map(|k| tc.transcode(k)).collect()
    }

    #[test]
    fn streams_text_then_tool_input_as_suffix_deltas() {
        let events = run(&[
            Kind::RunStarted,
            Kind::OutputText {
                text: "Let me ".into(),
            },
            Kind::OutputText {
                text: "read".into(),
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

        assert_eq!(
            events,
            vec![
                UIStreamEvent::Start,
                UIStreamEvent::StartStep,
                UIStreamEvent::TextStart { id: "txt-0".into() },
                UIStreamEvent::TextDelta {
                    id: "txt-0".into(),
                    delta: "Let me ".into(),
                },
                UIStreamEvent::TextDelta {
                    id: "txt-0".into(),
                    delta: "read".into(),
                },
                UIStreamEvent::TextEnd { id: "txt-0".into() },
                UIStreamEvent::ToolInputStart {
                    tool_call_id: "c1".into(),
                    tool_name: "read".into(),
                },
                UIStreamEvent::ToolInputDelta {
                    tool_call_id: "c1".into(),
                    input_text_delta: "{\"path\":".into(),
                },
                UIStreamEvent::ToolInputDelta {
                    tool_call_id: "c1".into(),
                    input_text_delta: "\"x\"}".into(),
                },
            ]
        );
    }

    #[test]
    fn never_emits_authoritative_finish_or_input_available() {
        let events = run(&[
            Kind::RunStarted,
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!("{}"),
            },
            Kind::Waiting {
                reason: "tool".into(),
            },
            Kind::RunFinished,
        ]);
        assert!(events.iter().all(|e| !matches!(
            e,
            UIStreamEvent::Finish { .. }
                | UIStreamEvent::FinishStep
                | UIStreamEvent::ToolInputAvailable { .. }
        )));
        assert!(events.contains(&UIStreamEvent::ToolInputStart {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
        }));
    }

    #[tokio::test]
    async fn channel_sink_forwards_event_kind() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = ChannelStreamSink::new(tx);
        sink.send(Event {
            run_id: RunId("r1".into()),
            kind: Kind::OutputText { text: "hi".into() },
        })
        .await
        .unwrap();
        assert_eq!(
            rx.recv().await,
            Some(Kind::OutputText { text: "hi".into() })
        );
    }

    #[tokio::test]
    async fn channel_sink_closed_after_receiver_dropped() {
        let (tx, rx) = mpsc::unbounded_channel();
        let sink = ChannelStreamSink::new(tx);
        drop(rx);
        let err = sink
            .send(Event {
                run_id: RunId("r1".into()),
                kind: Kind::RunFinished,
            })
            .await;
        assert!(matches!(err, Err(SinkError::Closed)));
    }
}
