//! Per-connection projection of one logical Thread's Runtime-owned live
//! observations into stream-only Managed `event_start` / `event_delta` frames.
//!
//! RuntimeHost's `ThreadEventHub` remains the sole live fan-out owner and
//! [`awaken_session_contract::SessionThreadLiveSubscription`] is its neutral port.
//! This module owns only disposable wire state: root text is emitted immediately,
//! while child text is buffered until ordinary-message proof prevents a terminal
//! cross-Thread report from leaking as an `agent.message` preview. Stable ids come
//! exclusively from the shared Run/Step/response-coordinate projector.

use std::collections::{HashMap, HashSet};

use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::{
    AssistantResponseCoordinate, Observation as StreamObservation,
};

use crate::types::{PreviewContent, PreviewDelta, PreviewFrame, PreviewTarget};

/// Per-connection wire state for one exact logical Thread. Root and child
/// responses deliberately differ: root text is always an ordinary assistant
/// message, while child text may instead become a terminal cross-Thread report.
pub(crate) struct ThreadPreviewProjector {
    session_id: String,
    thread_id: String,
    state: ThreadPreviewState,
}

enum ThreadPreviewState {
    Root {
        started: HashSet<String>,
    },
    Child {
        responses: HashMap<String, BufferedResponse>,
    },
}

#[derive(Default)]
struct BufferedResponse {
    frames: Vec<PreviewFrame>,
    started: bool,
    released: bool,
}

impl ThreadPreviewProjector {
    pub(crate) fn new(session_id: String, thread_id: String) -> Self {
        let state = if session_id == thread_id {
            ThreadPreviewState::Root {
                started: HashSet::new(),
            }
        } else {
            ThreadPreviewState::Child {
                responses: HashMap::new(),
            }
        };
        Self {
            session_id,
            thread_id,
            state,
        }
    }

    fn message_id(&self, run_id: &str, response: &AssistantResponseCoordinate) -> String {
        crate::state::managed_assistant_event_id(
            &self.session_id,
            &self.thread_id,
            run_id,
            response.step,
            response.response,
            "agent.message",
        )
    }

    pub(crate) fn project(&mut self, observation: StreamObservation) -> Vec<PreviewFrame> {
        let Some(response) = observation.assistant_response else {
            return Vec::new();
        };
        if response.thread_id.0 != self.thread_id {
            return Vec::new();
        }
        let event = observation.event;
        let id = self.message_id(&event.run_id.0, &response);
        match (&mut self.state, event.kind) {
            (
                ThreadPreviewState::Root { started },
                AgentEvent::Delta(Delta::TextDelta { delta }),
            ) => {
                let mut frames = Vec::new();
                if started.insert(id.clone()) {
                    frames.push(PreviewFrame::EventStart {
                        event: PreviewTarget {
                            event_type: "agent.message".into(),
                            id: id.clone(),
                        },
                    });
                }
                frames.push(PreviewFrame::EventDelta {
                    event_id: id,
                    delta: PreviewDelta::ContentDelta {
                        index: 0,
                        content: PreviewContent::Text { text: delta },
                    },
                });
                frames
            }
            (
                ThreadPreviewState::Child { responses },
                AgentEvent::Delta(Delta::TextDelta { delta }),
            ) => {
                let buffered = responses.entry(id.clone()).or_default();
                let mut next = Vec::new();
                if !buffered.started {
                    buffered.started = true;
                    next.push(PreviewFrame::EventStart {
                        event: PreviewTarget {
                            event_type: "agent.message".into(),
                            id: id.clone(),
                        },
                    });
                }
                next.push(PreviewFrame::EventDelta {
                    event_id: id,
                    delta: PreviewDelta::ContentDelta {
                        index: 0,
                        content: PreviewContent::Text { text: delta },
                    },
                });
                if buffered.released {
                    next
                } else {
                    buffered.frames.extend(next);
                    Vec::new()
                }
            }
            // A tool call proves this was an ordinary assistant response rather
            // than the terminal cross-Thread report. The tool itself remains
            // invisible in preview.
            (
                ThreadPreviewState::Child { responses },
                AgentEvent::Delta(Delta::ToolCallDelta { .. }),
            ) => {
                let buffered = responses.entry(id).or_default();
                buffered.released = true;
                std::mem::take(&mut buffered.frames)
            }
            // Official Managed preview is plain assistant text only. Reasoning,
            // tool input, and lifecycle facts never cross this wire projection.
            (_, _) => Vec::new(),
        }
    }

    /// Release a candidate only when durable child truth classified that exact
    /// deterministic id as an ordinary `agent.message`.
    pub(crate) fn take_for_committed(&mut self, event_id: &str) -> Vec<PreviewFrame> {
        let ThreadPreviewState::Child { responses } = &mut self.state else {
            return Vec::new();
        };
        // The durable broadcast and Runtime live subscription are independent
        // receivers. Remember proof even when the committed event wins their
        // race, so a delayed live chunk cannot remain buffered forever.
        let buffered = responses.entry(event_id.to_owned()).or_default();
        buffered.released = true;
        std::mem::take(&mut buffered.frames)
    }

    /// A committed report or terminal boundary discards child candidates that
    /// were never classified as ordinary messages. Root state is only bounded.
    pub(crate) fn discard_uncommitted(&mut self) {
        match &mut self.state {
            ThreadPreviewState::Root { started } => started.clear(),
            ThreadPreviewState::Child { responses } => responses.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;

    fn text(value: &str) -> AgentEvent {
        AgentEvent::Delta(Delta::TextDelta {
            delta: value.into(),
        })
    }

    fn reasoning(value: &str) -> AgentEvent {
        AgentEvent::Delta(Delta::ReasoningDelta {
            delta: value.into(),
        })
    }

    fn tool() -> AgentEvent {
        AgentEvent::Delta(Delta::ToolCallDelta {
            id: "call".into(),
            name: "read".into(),
            args_delta: "{}".into(),
        })
    }

    fn observation(thread: &str, response: usize, kind: AgentEvent) -> StreamObservation {
        StreamObservation::assistant_delta(
            RunId("run-1".into()),
            awaken_agent_contract::agent::thread::Id(thread.into()),
            2,
            response,
            kind,
        )
    }

    /// Root projection cause/effect graph: C1 exact coordinates, C2 first or
    /// repeated chunk for one response, C3 another response, C4 missing/foreign
    /// coordinates, C5 non-text detail. Effects: E1 exact text emits immediately;
    /// E2 the first chunk emits one stable start plus delta; E3 repeats emit only
    /// a delta with the same id; E4 C3 gets a distinct stable id; E5 C4/C5 emit
    /// nothing. Constraint: no process-local allocator or broadcast participates.
    ///
    /// | Rule | Coordinate | Detail | Response state | Effect |
    /// |---|---|---|---|---|
    /// | R1 | exact | text | first | E1,E2 |
    /// | R2 | exact | text | repeated | E1,E3 |
    /// | R3 | exact | text | new | E1,E4 |
    /// | R4 | missing/foreign | text | any | E5 |
    /// | R5 | exact | reasoning/tool | any | E5 |
    #[test]
    fn root_projector_immediately_emits_stable_message_previews() {
        // Causes: the fixtures below establish `root projector immediately` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        let mut projector = ThreadPreviewProjector::new("s1".into(), "s1".into());
        let first_id =
            crate::state::managed_assistant_event_id("s1", "s1", "run-1", 2, 0, "agent.message");
        let first = projector.project(observation("s1", 0, text("a")));
        assert!(
            matches!(
                first.as_slice(),
                [PreviewFrame::EventStart { event }, PreviewFrame::EventDelta { event_id, .. }]
                    if event.id == first_id && event_id == &first_id
            ),
            "R1/E1-E2"
        );

        let repeated = projector.project(observation("s1", 0, text("b")));
        assert!(
            matches!(
                repeated.as_slice(),
                [PreviewFrame::EventDelta { event_id, .. }] if event_id == &first_id
            ),
            "R2/E1,E3"
        );

        let next = projector.project(observation("s1", 1, text("c")));
        assert!(
            matches!(
                next.as_slice(),
                [PreviewFrame::EventStart { event }, PreviewFrame::EventDelta { event_id, .. }]
                    if event.id != first_id && event_id == &event.id
            ),
            "R3/E1,E4"
        );
        assert!(
            projector
                .project(observation("other", 0, text("leak")))
                .is_empty(),
            "R4/E5"
        );
        assert!(
            projector
                .project(StreamObservation::from(
                    awaken_agent_contract::stream::event::Event {
                        run_id: RunId("run-1".into()),
                        kind: text("ambiguous"),
                    },
                ))
                .is_empty(),
            "R4/E5"
        );
        assert!(
            projector
                .project(observation("s1", 2, reasoning("private")))
                .is_empty(),
            "R5/E5"
        );
        assert!(
            projector.project(observation("s1", 2, tool())).is_empty(),
            "R5/E5"
        );
    }

    /// Child projection cause/effect graph: C1 exact text, C2 foreign/missing
    /// coordinates, C3 durable matching `agent.message`, C4 tool call in the
    /// response, C5 reasoning, C6 durable proof wins the independent-receiver
    /// race before text. Effects: E1 C1 remains buffered; E2 C2/C5 emits nothing;
    /// E3 C3 releases one canonical start plus every text delta; E4 C4 releases
    /// an ordinary response immediately; E5 C6 makes later exact text immediate.
    ///
    /// | Rule | Coordinate/detail | Ordinary proof | Effect |
    /// |---|---|---|---|
    /// | R1 | exact text | absent | E1 |
    /// | R2 | foreign/missing text | any | E2 |
    /// | R3 | exact text | committed id | E3 |
    /// | R4 | exact text | tool delta | E4 |
    /// | R5 | exact reasoning | any | E2 |
    /// | R6 | exact text after committed id | committed first | E5 |
    #[test]
    fn child_projector_buffers_until_ordinary_and_never_previews_reasoning() {
        // Causes: the fixtures below establish `child projector buffers until ordinary and` with
        // the concrete inputs, state, dependencies, and failure triggers used by this case.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        let mut projector = ThreadPreviewProjector::new("s1".into(), "child-1".into());
        assert!(
            projector
                .project(observation("other", 0, text("leak")))
                .is_empty(),
            "R2/E2"
        );
        assert!(
            projector
                .project(StreamObservation::from(
                    awaken_agent_contract::stream::event::Event {
                        run_id: RunId("run-1".into()),
                        kind: text("ambiguous"),
                    },
                ))
                .is_empty(),
            "R2/E2"
        );
        assert!(
            projector
                .project(observation("child-1", 0, text("a")))
                .is_empty(),
            "R1/E1"
        );
        assert!(
            projector
                .project(observation("child-1", 0, text("b")))
                .is_empty(),
            "R1/E1"
        );
        let first_id = crate::state::managed_assistant_event_id(
            "s1",
            "child-1",
            "run-1",
            2,
            0,
            "agent.message",
        );
        let committed = projector.take_for_committed(&first_id);
        assert!(
            matches!(
                committed.as_slice(),
                [PreviewFrame::EventStart { event }, PreviewFrame::EventDelta { event_id: first, .. }, PreviewFrame::EventDelta { event_id: second, .. }]
                    if event.id == first_id && first == &first_id && second == &first_id
            ),
            "R3/E3"
        );

        assert!(
            projector
                .project(observation("child-1", 1, text("c")))
                .is_empty(),
            "R1/E1"
        );
        let ordinary = projector.project(observation("child-1", 1, tool()));
        assert!(
            matches!(
                ordinary.as_slice(),
                [PreviewFrame::EventStart { event }, PreviewFrame::EventDelta { event_id, .. }]
                    if event.id != first_id && event_id == &event.id
            ),
            "R4/E4"
        );
        assert!(
            projector
                .project(observation("child-1", 2, reasoning("private")))
                .is_empty(),
            "R5/E2"
        );

        let committed_first_id = crate::state::managed_assistant_event_id(
            "s1",
            "child-1",
            "run-1",
            2,
            3,
            "agent.message",
        );
        assert!(
            projector.take_for_committed(&committed_first_id).is_empty(),
            "R6 has no buffered frame"
        );
        let delayed = projector.project(observation("child-1", 3, text("delayed")));
        assert!(
            matches!(
                delayed.as_slice(),
                [PreviewFrame::EventStart { event }, PreviewFrame::EventDelta { event_id, .. }]
                    if event.id == committed_first_id && event_id == &committed_first_id
            ),
            "R6/E5"
        );
    }

    /// Child terminal decision table: R1=exact text without ordinary proof keeps
    /// one candidate (C1→E1); R2=C1 plus committed report/terminal discards it
    /// (C1+C2→E2), so a later exact-id lookup cannot leak start or delta frames.
    #[test]
    fn child_terminal_report_candidate_is_discarded_without_preview() {
        // Causes: the fixtures below establish `child terminal report candidate` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `is discarded without preview` and every asserted state
        // transition or side effect must hold.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        let mut projector = ThreadPreviewProjector::new("s1".into(), "child-1".into());
        assert!(
            projector
                .project(observation("child-1", 0, text("report")))
                .is_empty(),
            "R1/E1"
        );
        projector.discard_uncommitted();
        let id = crate::state::managed_assistant_event_id(
            "s1",
            "child-1",
            "run-1",
            2,
            0,
            "agent.message",
        );
        assert!(projector.take_for_committed(&id).is_empty(), "R2/E2");
    }
}
