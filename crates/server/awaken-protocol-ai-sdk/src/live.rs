//! The AI SDK live-channel transcoder: turn the engine's best-effort progress
//! (`stream::Kind`) into the in-flight *prefix* of a UI Message Stream —
//! `start`/`start-step`, streamed `text-*`, and
//! `tool-input-start`/`tool-input-delta` — as a turn runs. It deliberately does
//! **not** emit the authoritative tail (`tool-input-available`, tool output,
//! `finish`); that comes from the committed [`StepOutcome`], so the live channel
//! never becomes the source of truth (G10/G13). The channel adapter itself is the
//! shared [`awaken_protocol_transport::ChannelStreamSink`].
//!
//! Tool-argument fragments arrive already de-accumulated: each `ToolCallDelta`
//! carries only the newly-appended suffix (the provider adapter owns the
//! de-accumulation), which the transcoder forwards verbatim as an `inputTextDelta`.

use std::collections::HashSet;

use awaken_agent_contract::stream::event::Kind;

use crate::types::UIStreamEvent;

/// Stateful transcoder from the live `stream::Kind` channel to the in-flight
/// prefix of an AI SDK UI Message Stream. One instance per streamed turn.
#[derive(Default)]
pub struct LiveTranscoder {
    started: bool,
    /// The id of the open text block, if a text run is currently streaming.
    open_text: Option<String>,
    text_seq: usize,
    /// Call ids that already emitted `tool-input-start`. Arg deltas arrive already
    /// de-accumulated (the provider adapter owns that), so no offset is kept.
    tools: HashSet<String>,
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
            Kind::ToolCallDelta {
                call_id,
                tool_id,
                args_delta,
            } => {
                let mut out = self.close_text();
                if self.tools.insert(call_id.clone()) {
                    out.push(UIStreamEvent::ToolInputStart {
                        tool_call_id: call_id.clone(),
                        tool_name: tool_id.clone(),
                    });
                }
                // `args_delta` is already the newly-appended suffix (the provider
                // adapter de-accumulated genai's cumulative snapshots), so forward it
                // verbatim; the committed `tool-input-available` carries the parsed input.
                if !args_delta.is_empty() {
                    out.push(UIStreamEvent::ToolInputDelta {
                        tool_call_id: call_id.clone(),
                        input_text_delta: args_delta.clone(),
                    });
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

    fn run(seq: &[Kind]) -> Vec<UIStreamEvent> {
        let mut tc = LiveTranscoder::new();
        seq.iter().flat_map(|k| tc.transcode(k)).collect()
    }

    #[test]
    fn streams_text_then_tool_input_forwarding_suffix_deltas() {
        let events = run(&[
            Kind::RunStarted,
            Kind::OutputText {
                text: "Let me ".into(),
            },
            Kind::OutputText {
                text: "read".into(),
            },
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "".into(),
            },
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "{\"path\":".into(),
            },
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "\"x\"}".into(),
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
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "{}".into(),
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

    #[test]
    fn a_repeated_run_started_is_idempotent() {
        let events = run(&[Kind::RunStarted, Kind::RunStarted]);
        // Only the first RunStarted opens the stream.
        assert_eq!(events, vec![UIStreamEvent::Start, UIStreamEvent::StartStep]);
    }

    #[test]
    fn concurrent_tool_calls_stream_independently_by_call_id() {
        let events = run(&[
            Kind::RunStarted,
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "".into(),
            },
            Kind::ToolCallDelta {
                call_id: "c2".into(),
                tool_id: "write".into(),
                args_delta: "".into(),
            },
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "{\"a\":1}".into(),
            },
            Kind::ToolCallDelta {
                call_id: "c2".into(),
                tool_id: "write".into(),
                args_delta: "{\"b\":2}".into(),
            },
        ]);
        assert!(events.contains(&UIStreamEvent::ToolInputStart {
            tool_call_id: "c1".into(),
            tool_name: "read".into(),
        }));
        assert!(events.contains(&UIStreamEvent::ToolInputStart {
            tool_call_id: "c2".into(),
            tool_name: "write".into(),
        }));
        assert!(events.contains(&UIStreamEvent::ToolInputDelta {
            tool_call_id: "c1".into(),
            input_text_delta: "{\"a\":1}".into(),
        }));
        assert!(events.contains(&UIStreamEvent::ToolInputDelta {
            tool_call_id: "c2".into(),
            input_text_delta: "{\"b\":2}".into(),
        }));
    }

    #[test]
    fn text_reopens_with_a_fresh_id_after_a_tool_call() {
        let events = run(&[
            Kind::RunStarted,
            Kind::OutputText { text: "hi".into() },
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "".into(),
            },
            Kind::OutputText {
                text: "more".into(),
            },
        ]);
        // The first run closes on the tool call; the second opens txt-1.
        assert!(events.contains(&UIStreamEvent::TextEnd { id: "txt-0".into() }));
        assert!(events.contains(&UIStreamEvent::TextStart { id: "txt-1".into() }));
        assert!(events.contains(&UIStreamEvent::TextDelta {
            id: "txt-1".into(),
            delta: "more".into(),
        }));
    }
}
