// @formal surface requirement=control.admin_publication.audited_publication
// @formal reason: This source implements the repository-controlled publication product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Per-connection projection of one logical Thread's Runtime-owned live
//! observations into stream-only Managed `event_start` / `event_delta` frames.
//!
//! RuntimeHost's `ThreadEventHub` remains the sole live fan-out owner and
//! [`awaken_session_execution_contract::SessionThreadLiveSubscription`] is its neutral port.
//! This module owns only disposable wire state: root text is emitted immediately,
//! while child text is buffered until ordinary-message proof prevents a terminal
//! cross-Thread report from leaking as an `agent.message` preview. Stable ids come
//! exclusively from the shared Run/Step/response-coordinate projector.

use std::collections::{HashMap, HashSet};

use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::{
    AssistantResponseCoordinate, Observation as StreamObservation,
};
use awaken_lease_contract::fence::Epoch;

use crate::types::{
    Event, OutboundKind, PreviewContent, PreviewDelta, PreviewFrame, PreviewTarget,
};

/// Per-connection wire state for one exact logical Thread. Root and child
/// responses deliberately differ: root text is always an ordinary assistant
/// message, while child text may instead become a terminal cross-Thread report.
pub(crate) struct ThreadPreviewProjector {
    session_id: String,
    thread_id: String,
    state: ThreadPreviewState,
    closed: HashSet<String>,
    thinking: HashSet<String>,
    thinking_enabled: bool,
    epochs: HashMap<String, Epoch>,
    response_epochs: HashMap<String, Epoch>,
    pending_bytes: usize,
    interrupted: bool,
    exhausted: bool,
}

// A preview is optional. When the bound is reached, suppress new previews
// instead of forgetting a closed id and allowing a late frame to reopen it.
const MAX_PREVIEW_IDENTITIES: usize = 1024;
const MAX_PENDING_FRAMES: usize = 64;
const MAX_PENDING_BYTES: usize = 128 * 1024;

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
    bytes: usize,
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
            closed: HashSet::new(),
            thinking: HashSet::new(),
            thinking_enabled: false,
            epochs: HashMap::new(),
            response_epochs: HashMap::new(),
            pending_bytes: 0,
            interrupted: false,
            exhausted: false,
        }
    }

    pub(crate) fn with_thinking(mut self, enabled: bool) -> Self {
        self.thinking_enabled = enabled;
        self
    }

    fn response_id(
        &self,
        run_id: &str,
        response: &AssistantResponseCoordinate,
        kind: &'static str,
    ) -> String {
        crate::state::managed_assistant_event_id(
            &self.session_id,
            &self.thread_id,
            run_id,
            response.step,
            response.response,
            kind,
        )
    }

    #[cfg(test)]
    pub(crate) fn project(&mut self, observation: StreamObservation) -> Vec<PreviewFrame> {
        self.project_checked(observation, None, |_| false)
    }

    pub(crate) fn project_checked(
        &mut self,
        observation: StreamObservation,
        epoch: Option<Epoch>,
        mut published: impl FnMut(&str) -> bool,
    ) -> Vec<PreviewFrame> {
        let Some(response) = observation.assistant_response else {
            return Vec::new();
        };
        if self.exhausted || self.interrupted || response.thread_id.0 != self.thread_id {
            return Vec::new();
        }
        let event = observation.event;
        let id = self.response_id(&event.run_id.0, &response, "agent.message");
        if !self.accept_epoch(&event.run_id.0, &id, epoch)
            || published(&id)
            || self.closed.contains(&id)
        {
            return Vec::new();
        }
        if matches!(event.kind, AgentEvent::Delta(Delta::ReasoningDelta { .. })) {
            if !self.thinking_enabled {
                return Vec::new();
            }
            let id = self.response_id(&event.run_id.0, &response, "agent.thinking");
            if published(&id) || self.closed.contains(&id) || !self.thinking.insert(id.clone()) {
                return Vec::new();
            }
            if self.thinking.len() >= MAX_PREVIEW_IDENTITIES {
                self.exhausted = true;
            }
            return vec![PreviewFrame::EventStart {
                event: PreviewTarget {
                    event_type: "agent.thinking".into(),
                    id,
                },
            }];
        }
        match (&mut self.state, event.kind) {
            (
                ThreadPreviewState::Root { started },
                AgentEvent::Delta(Delta::TextDelta { delta }),
            ) => {
                let mut frames = Vec::new();
                if started.insert(id.clone()) {
                    if started.len() >= MAX_PREVIEW_IDENTITIES {
                        self.exhausted = true;
                    }
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
                if responses.len() >= MAX_PREVIEW_IDENTITIES {
                    self.exhausted = true;
                    return Vec::new();
                }
                let buffered = responses.entry(id.clone()).or_default();
                let bytes = delta.len();
                if !buffered.released
                    && (buffered.frames.len() >= MAX_PENDING_FRAMES
                        || self.pending_bytes.saturating_add(bytes) > MAX_PENDING_BYTES)
                {
                    return Vec::new();
                }
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
                    buffered.bytes += bytes;
                    self.pending_bytes += bytes;
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
                if responses.len() >= MAX_PREVIEW_IDENTITIES {
                    self.exhausted = true;
                    return Vec::new();
                }
                let buffered = responses.entry(id).or_default();
                buffered.released = true;
                self.pending_bytes -= std::mem::take(&mut buffered.bytes);
                std::mem::take(&mut buffered.frames)
            }
            // Only assistant text and the contentless thinking start are visible.
            (_, _) => Vec::new(),
        }
    }

    fn accept_epoch(&mut self, run: &str, id: &str, epoch: Option<Epoch>) -> bool {
        let Some(epoch) = epoch else {
            return !self.epochs.contains_key(run);
        };
        let newest = self.epochs.entry(run.to_owned()).or_insert(epoch);
        if epoch < *newest {
            return false;
        }
        *newest = epoch;
        // The public event id survives takeover. Don't splice a replacement
        // attempt's text into a preview already attributed to its predecessor.
        let matched = *self.response_epochs.entry(id.to_owned()).or_insert(epoch) == epoch;
        self.exhausted = self.response_epochs.len() >= MAX_PREVIEW_IDENTITIES;
        matched && !self.exhausted
    }

    pub(crate) fn observe_committed(&mut self, event: &Event) -> Vec<PreviewFrame> {
        match &event.kind {
            OutboundKind::AgentMessage { .. } => self.take_for_committed(&event.id),
            OutboundKind::AgentThinking {} => {
                self.close(&event.id);
                Vec::new()
            }
            OutboundKind::SpanModelRequestEnd {
                preview_response_id: Some(id),
                ..
            } => {
                // Another provider can reuse the same response id. Close its
                // preview and let the final message render the complete output.
                let _ = self.take_for_committed(id);
                Vec::new()
            }
            OutboundKind::AgentThreadMessageSent { .. }
            | OutboundKind::SessionThreadStatusIdle { .. }
            | OutboundKind::SessionThreadStatusTerminated { .. } => {
                self.discard_uncommitted();
                Vec::new()
            }
            OutboundKind::UserInterrupt { session_thread_id }
                if session_thread_id
                    .as_ref()
                    .map_or(self.thread_id == self.session_id, |id| {
                        id == &self.thread_id
                    }) =>
            {
                self.interrupted = true;
                match &self.state {
                    ThreadPreviewState::Root { started } => {
                        self.closed.extend(started.iter().cloned())
                    }
                    ThreadPreviewState::Child { responses } => {
                        self.closed.extend(responses.keys().cloned())
                    }
                }
                self.exhausted |= self.closed.len() >= MAX_PREVIEW_IDENTITIES;
                self.discard_uncommitted();
                Vec::new()
            }
            OutboundKind::SessionStatusRunning {}
            | OutboundKind::SessionThreadStatusRunning { .. } => {
                self.interrupted = false;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Release a candidate only when durable child truth classified that exact
    /// deterministic id as an ordinary `agent.message`.
    pub(crate) fn take_for_committed(&mut self, event_id: &str) -> Vec<PreviewFrame> {
        self.close(event_id);
        let ThreadPreviewState::Child { responses } = &mut self.state else {
            return Vec::new();
        };
        responses
            .remove(event_id)
            .map_or_else(Vec::new, |response| {
                self.pending_bytes -= response.bytes;
                response.frames
            })
    }

    pub(crate) fn close(&mut self, event_id: &str) {
        if !self.exhausted {
            self.closed.insert(event_id.to_owned());
            self.exhausted = self.closed.len() >= MAX_PREVIEW_IDENTITIES;
        }
    }

    /// A committed report or terminal boundary discards child candidates that
    /// were never classified as ordinary messages. Root state is only bounded.
    pub(crate) fn discard_uncommitted(&mut self) {
        self.pending_bytes = 0;
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
    /// an ordinary response immediately; E5 C6 closes the preview before late text.
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
        assert!(delayed.is_empty(), "R6/E5 committed output stays closed");
    }

    /// Child terminal decision table: R1=exact text without ordinary proof keeps
    /// one candidate (C1鈫扙1); R2=C1 plus committed report/terminal discards it
    /// (C1+C2鈫扙2), so a later exact-id lookup cannot leak start or delta frames.
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

    #[test]
    fn a_request_end_and_a_new_claim_suppress_only_their_old_previews() {
        let mut projector =
            ThreadPreviewProjector::new("s1".into(), "s1".into()).with_thinking(true);
        let emit = |projector: &mut ThreadPreviewProjector, response, epoch, kind| {
            projector.project_checked(
                observation("s1", response, kind),
                Epoch::from_persisted(epoch),
                |_| false,
            )
        };
        let mark = emit(&mut projector, 0, 1, reasoning("never on wire"));
        assert!(matches!(
            mark.as_slice(),
            [PreviewFrame::EventStart { event }] if event.event_type == "agent.thinking"
        ));
        assert!(emit(&mut projector, 0, 1, reasoning("private")).is_empty());
        let response_id =
            crate::state::managed_assistant_event_id("s1", "s1", "run-1", 2, 0, "agent.message");
        let closed = Event {
            id: "end-id".into(),
            processed_at: None,
            kind: OutboundKind::SpanModelRequestEnd {
                model_request_start_id: "start-id".into(),
                is_error: Some(true),
                model_usage: Default::default(),
                preview_response_id: Some(response_id),
            },
        };
        assert!(projector.observe_committed(&closed).is_empty());
        assert!(emit(&mut projector, 0, 1, text("too late")).is_empty());
        assert_eq!(emit(&mut projector, 1, 2, text("live")).len(), 2);
        assert!(emit(&mut projector, 2, 1, text("too late")).is_empty());
        assert_eq!(emit(&mut projector, 1, 2, text("still live")).len(), 1);
    }
}
