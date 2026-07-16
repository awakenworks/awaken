//! The live-preview sink: the runtime's best-effort progress channel (the neutral
//! `AgentEvent` live vocabulary) projected into stream-only Managed `event_start` /
//! `event_delta` preview frames, published to a session's live broadcast as a run
//! streams.
//!
//! It previews **agent.message text only** — awaken's live stream carries no
//! thinking or tool-input channel, and the official wire never previews tool use.
//! Each contiguous text run opens one previewed `agent.message`: the sink mints
//! that message's committed id up front (from the shared event-id counter) so
//! `event_start.event.id` equals the id the buffered `agent.message` will carry,
//! and records the minted ids in order so `append_step` reuses them for the
//! committed messages — letting the SDK reconcile preview → buffered by id.
//! Previews are best-effort: a failed broadcast never touches committed truth.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink};
use tokio::sync::broadcast;

use crate::types::{PreviewContent, PreviewDelta, PreviewFrame, PreviewTarget, StreamFrame};

/// Per-run sink: projects the live `AgentEvent` channel into `agent.message`
/// previews on the session's broadcast, and remembers the ids it minted for the
/// committed log.
pub struct PreviewSink {
    live: broadcast::Sender<StreamFrame>,
    event_seq: Arc<AtomicU64>,
    inner: std::sync::Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The id of the currently open previewed `agent.message`, if a text run is
    /// streaming. `None` between runs — before the first text, or after a tool
    /// call / terminal kind closes the run.
    open_id: Option<String>,
    /// The ids minted for each previewed message, in order — consumed by
    /// `append_step` so the committed `agent.message` events reuse them.
    allocated: Vec<String>,
}

impl PreviewSink {
    pub fn new(live: broadcast::Sender<StreamFrame>, event_seq: Arc<AtomicU64>) -> Self {
        Self {
            live,
            event_seq,
            inner: std::sync::Mutex::new(Inner::default()),
        }
    }

    /// The ids minted for previewed `agent.message` events, in emission order,
    /// draining the record. `append_step` assigns these to the buffered messages
    /// so a preview and its committed event share an id.
    pub fn take_allocated_ids(&self) -> Vec<String> {
        std::mem::take(&mut self.inner.lock().unwrap().allocated)
    }

    /// Mint the next committed event id from the shared counter (`evt_N`), the
    /// same sequence `ManagedState::next_event_id` draws from.
    fn next_id(&self) -> String {
        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
    }

    fn publish(&self, frame: PreviewFrame) {
        // Best-effort: no subscribers (or a lagging one) is never an error.
        let _ = self.live.send(StreamFrame::Preview(frame));
    }
}

#[async_trait::async_trait]
impl Sink for PreviewSink {
    async fn send(&self, event: StreamEvent) -> Result<(), SinkError> {
        match &event.kind {
            AgentEvent::Delta(Delta::TextDelta { delta: text }) => {
                let id = {
                    let mut inner = self.inner.lock().unwrap();
                    if let Some(id) = inner.open_id.clone() {
                        id
                    } else {
                        let id = self.next_id();
                        inner.open_id = Some(id.clone());
                        inner.allocated.push(id.clone());
                        drop(inner);
                        self.publish(PreviewFrame::EventStart {
                            event: PreviewTarget {
                                event_type: "agent.message".into(),
                                id: id.clone(),
                            },
                        });
                        id
                    }
                };
                self.publish(PreviewFrame::EventDelta {
                    event_id: id,
                    delta: PreviewDelta::ContentDelta {
                        index: 0,
                        content: PreviewContent::Text { text: text.clone() },
                    },
                });
            }
            // A tool call, reasoning, or any committed lifecycle event closes the
            // current text run; the next text opens a fresh previewed message. Tool
            // use and reasoning are never previewed (matches the official wire).
            AgentEvent::Delta(Delta::ToolCallDelta { .. })
            | AgentEvent::Delta(Delta::ReasoningDelta { .. })
            | AgentEvent::Fact(_) => {
                self.inner.lock().unwrap().open_id = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::event::Fact;

    fn run_started() -> AgentEvent {
        AgentEvent::Fact(Fact::RunStarted)
    }
    fn run_finished() -> AgentEvent {
        AgentEvent::Fact(Fact::RunFinished { exhausted: false })
    }
    fn text(t: &str) -> AgentEvent {
        AgentEvent::Delta(Delta::TextDelta { delta: t.into() })
    }
    fn tool(id: &str, name: &str, args: &str) -> AgentEvent {
        AgentEvent::Delta(Delta::ToolCallDelta {
            id: id.into(),
            name: name.into(),
            args_delta: args.into(),
        })
    }

    fn ev(kind: AgentEvent) -> StreamEvent {
        StreamEvent {
            run_id: RunId("r1".into()),
            kind,
        }
    }

    async fn drive(seq: &[AgentEvent]) -> (Vec<StreamFrame>, Vec<String>) {
        let (tx, mut rx) = broadcast::channel(256);
        let sink = PreviewSink::new(tx.clone(), Arc::new(AtomicU64::new(0)));
        for k in seq {
            sink.send(ev(k.clone())).await.unwrap();
        }
        drop(tx);
        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }
        (frames, sink.take_allocated_ids())
    }

    fn preview(frames: &[StreamFrame]) -> Vec<&PreviewFrame> {
        frames
            .iter()
            .filter_map(|f| match f {
                StreamFrame::Preview(p) => Some(p),
                StreamFrame::Committed(_) => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn text_run_opens_one_message_and_streams_suffix_deltas() {
        let (frames, ids) = drive(&[run_started(), text("Hel"), text("lo"), run_finished()]).await;
        let p = preview(&frames);
        // One event_start (id evt_0) then two content_delta frames on it.
        assert_eq!(ids, vec!["evt_0".to_string()]);
        assert!(matches!(
            p[0],
            PreviewFrame::EventStart { event } if event.event_type == "agent.message" && event.id == "evt_0"
        ));
        let text: String = p[1..]
            .iter()
            .filter_map(|f| match f {
                PreviewFrame::EventDelta {
                    event_id,
                    delta: PreviewDelta::ContentDelta { index, content },
                } => {
                    assert_eq!(event_id, "evt_0");
                    assert_eq!(*index, 0);
                    let PreviewContent::Text { text } = content;
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
    }

    #[tokio::test]
    async fn a_tool_call_closes_the_run_and_the_next_text_opens_a_fresh_message() {
        let (frames, ids) = drive(&[text("before"), tool("c1", "read", "{}"), text("after")]).await;
        // Two distinct messages: evt_0 (before the tool) and evt_1 (after).
        assert_eq!(ids, vec!["evt_0".to_string(), "evt_1".to_string()]);
        let starts: Vec<&str> = preview(&frames)
            .iter()
            .filter_map(|p| match p {
                PreviewFrame::EventStart { event } => Some(event.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec!["evt_0", "evt_1"]);
    }

    #[tokio::test]
    async fn tool_use_is_never_previewed() {
        let (frames, ids) = drive(&[tool("c1", "read", "{\"path\":\"x\"}")]).await;
        assert!(preview(&frames).is_empty(), "no preview for a tool call");
        assert!(ids.is_empty());
    }
}
