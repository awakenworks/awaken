//! Protocol-neutral Run application API shared by every public adapter.
//!
//! This module is the one application waist for submit/resume/control/query.
//! Protocol crates own only wire translation; Runtime and Coordinator adapters
//! implement this port over the same committed Run authority.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::event::AgentEvent;
use awaken_agent_contract::page::{UnknownCursor, paginate_by_id};
use awaken_agent_contract::stream::event::Event;
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink as StreamSink};

use crate::{Pending, RunError, StepOutcome};
use awaken_agent_contract::agent::run::Id as RunId;

pub use awaken_agent_contract::page::{CursorParams, HistoryPage};

/// The application error surface consumed by public protocol adapters.
/// This is an alias, not a second error authority.
pub type RunApplicationError = RunError;

/// One neutral resume command for a pending Run.
#[derive(Debug, Clone)]
pub enum RunResume {
    Permission(PermissionDecision),
    ClientResult {
        content: Vec<ContentBlock>,
        is_error: bool,
    },
}

/// The one neutral application port driven by AG-UI, AI SDK, A2A and ACP-facing
/// adapters. Session creation and realization stay on their narrower Session
/// application ports.
#[async_trait]
pub trait RunApplication: Send + Sync {
    async fn run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError>;

    async fn run_streaming(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let _ = sink;
        self.run(operation_id, thread, agent, messages).await
    }

    async fn resume(
        &self,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError>;

    async fn interrupt(&self, _thread: &str) -> Result<(), RunApplicationError> {
        Ok(())
    }

    async fn pending(&self, thread: &str) -> Result<Option<Pending>, RunApplicationError>;
    async fn history(&self, thread: &str) -> Result<Vec<Message>, RunApplicationError>;
    fn model(&self) -> String;

    async fn usage(&self, _thread: &str) -> Result<(u64, u64), RunApplicationError> {
        Ok((0, 0))
    }
}

/// Narrow application port for an explicit newest-wins Session Run.
///
/// Operational HTTP may expose this command, but only the Session application
/// may turn it into a reservation/activity/activation sequence. Keeping it
/// separate from ordinary [`RunApplication`] prevents public protocols from
/// silently changing append semantics.
#[async_trait]
pub trait SessionRunReplacementApplication: Send + Sync {
    async fn supersede_session_run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError>;
}

/// Narrow application port for publishing a Session Run without observing it.
///
/// Background submission changes only the caller's observation policy. It must
/// still reserve the Run, commit the exact Session activity receipt, and publish
/// that reservation through the same Session-owned admission state machine as a
/// foreground protocol request.
#[async_trait]
pub trait SessionRunBackgroundApplication: Send + Sync {
    async fn submit_session_run_background(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        traceparent: Option<String>,
    ) -> Result<RunId, RunApplicationError>;
}

/// Concatenate direct text blocks, dropping non-text blocks. Nested tool-result
/// content is deliberately not flattened into the enclosing protocol message.
#[must_use]
pub fn blocks_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub fn paginate_history<'a>(
    history: &'a [Message],
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<HistoryPage<'a, Message>, UnknownCursor> {
    paginate_by_id(history, cursor, limit, |message| message.id.0.as_str())
}

/// Shared best-effort live-event channel adapter.
pub struct EventForwardingSink {
    forward: Arc<dyn Fn(AgentEvent) -> Result<(), SinkError> + Send + Sync>,
}

impl EventForwardingSink {
    #[must_use]
    pub fn new(
        forward: impl Fn(AgentEvent) -> Result<(), SinkError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            forward: Arc::new(forward),
        }
    }
}

#[async_trait]
impl StreamSink for EventForwardingSink {
    async fn send(&self, event: Event) -> Result<(), SinkError> {
        (self.forward)(event.kind)
    }
}

/// Format epoch milliseconds without losing subsecond precision. Whole seconds
/// retain their historical spelling. Calendar arithmetic is owned by the same
/// UTC codec as Work receipts. Values after year 9999 saturate only in this
/// display projection; persisted milliseconds and lease arithmetic are unchanged.
#[must_use]
pub fn epoch_millis_to_rfc3339(epoch_millis: u64) -> String {
    let nanos = crate::work_queue::bounded_rfc3339_nanos(i128::from(epoch_millis) * 1_000_000);
    let text = crate::work_queue::format_rfc3339_utc_nanos(nanos / 1_000_000 * 1_000_000);
    let (whole, fraction) = text
        .strip_suffix('Z')
        .expect("UTC codec")
        .split_once('.')
        .expect("fraction codec");
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{whole}Z")
    } else {
        format!("{whole}.{fraction}Z")
    }
}

/// Decode the same persisted UTC vocabulary; malformed/unknown times remain absent.
#[must_use]
pub fn rfc3339_to_epoch_millis(value: &str) -> Option<u64> {
    crate::work_queue::parse_rfc3339_utc_nanos(value)
        .filter(|nanos| *nanos >= 0)
        .and_then(|nanos| u64::try_from(nanos / 1_000_000).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::{EndCause, Failure, RunState};

    #[test]
    fn terminal_fact_is_derived_from_one_run_state_authority() {
        // Cause/effect decision table:
        // R1 awaiting + pending -> Awaiting fact;
        // R2 MaxSteps -> exhausted finish;
        // R3 classified failure -> RunFailed with the same code;
        // R4 natural end -> ordinary finish; R5 indeterminate -> explicit
        // RunFailed, never success. No second Terminal enum exists.
        // Constraints/invariants: RunState is the sole terminal authority and
        // projection cannot turn Indeterminate or a classified error into success.
        let pending = Pending {
            tool_use_id: "call-1".into(),
            name: "tool".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        let awaiting = StepOutcome::awaiting(Vec::new(), Some(pending.clone()));
        assert_eq!(awaiting.state(), &RunState::Awaiting, "R1");
        assert!(
            matches!(
                awaiting.terminal_event(),
                awaken_agent_contract::event::Fact::Awaiting { .. }
            ),
            "R1"
        );

        let exhausted = StepOutcome::ended(Vec::new(), EndCause::MaxSteps);
        assert_eq!(
            exhausted.state(),
            &RunState::Ended(EndCause::MaxSteps),
            "R2"
        );
        assert_eq!(
            exhausted.terminal_event(),
            awaken_agent_contract::event::Fact::RunFinished { exhausted: true },
            "R2"
        );

        let failed = StepOutcome::ended(
            Vec::new(),
            EndCause::Error(Failure::Inference {
                code: "upstream".into(),
                message: "down".into(),
            }),
        );
        assert!(
            matches!(failed.terminal_event(), awaken_agent_contract::event::Fact::RunFailed { ref code, .. } if code == "upstream"),
            "R3"
        );

        let finished = StepOutcome::ended(Vec::new(), EndCause::NaturalEnd);
        assert_eq!(
            finished.state(),
            &RunState::Ended(EndCause::NaturalEnd),
            "R4"
        );

        let indeterminate = StepOutcome::ended(Vec::new(), EndCause::Indeterminate);
        assert!(
            matches!(
                indeterminate.terminal_event(),
                awaken_agent_contract::event::Fact::RunFailed { ref code, .. }
                    if code == "indeterminate"
            ),
            "R5"
        );
    }

    proptest::proptest! {
        // D1 every legal millisecond round-trips; D2 every larger u64 saturates
        // to the last representable millisecond. A fixed regression below also
        // checks known dates independently of the round-trip oracle.
        #[test]
        fn timestamp_projection_round_trips_its_declared_domain(millis in proptest::prelude::any::<u64>()) {
            let wire = epoch_millis_to_rfc3339(millis);
            proptest::prop_assert_eq!(rfc3339_to_epoch_millis(&wire), Some(millis.min(253_402_300_799_999)));
        }
    }

    #[test]
    fn timestamp_input_and_calendar_boundary_decision_table() {
        // D3 invalid syntax/date -> absent; D4 calendar boundaries -> exact
        // independent golden value; D5 formatter output always remains in the
        // declared four-digit UTC vocabulary, including u64/SQL extremes.
        for invalid in [
            "2026-09-06T00:00:00.Z",
            "2026-09-06T00:00:00..1Z",
            "2026-02-29T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-00-01T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T00:00:00.1234567890Z",
            "2026-01-01T00:00:00+00:00",
            "２０２６-01-01T00:00:00Z",
        ] {
            assert_eq!(rfc3339_to_epoch_millis(invalid), None, "D3 {invalid}");
        }
        for (millis, expected) in [
            (946_684_800_000, "2000-01-01T00:00:00Z"),
            (951_782_400_000, "2000-02-29T00:00:00Z"),
            (4_107_542_400_000, "2100-03-01T00:00:00Z"),
            (253_402_300_799_999, "9999-12-31T23:59:59.999Z"),
        ] {
            assert_eq!(epoch_millis_to_rfc3339(millis), expected, "D4");
            assert_eq!(rfc3339_to_epoch_millis(expected), Some(millis), "D4");
        }
        for millis in [253_402_300_800_000, i64::MAX as u64, u64::MAX] {
            assert_eq!(
                epoch_millis_to_rfc3339(millis),
                "9999-12-31T23:59:59.999Z",
                "D5"
            );
        }
    }

    #[test]
    fn epoch_milliseconds_have_one_canonical_projection() {
        // Cause/effect table: zero -> epoch; subsecond -> exact fraction;
        // leap-day boundary -> exact Gregorian date; fractional trailing zeros
        // normalize and round-trip; malformed/pre-epoch input -> no observation.
        for millis in [0, 1, 10, 100, 999, 1_800_000_000_123] {
            assert_eq!(
                rfc3339_to_epoch_millis(&epoch_millis_to_rfc3339(millis)),
                Some(millis)
            );
        }
        assert_eq!(epoch_millis_to_rfc3339(100), "1970-01-01T00:00:00.1Z");
        assert_eq!(rfc3339_to_epoch_millis("invalid"), None);
        assert_eq!(rfc3339_to_epoch_millis("1969-12-31T23:59:59Z"), None);
        assert_eq!(rfc3339_to_epoch_millis("1969-12-31T23:59:59.9995Z"), None);
        assert_eq!(epoch_millis_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_millis_to_rfc3339(999), "1970-01-01T00:00:00.999Z");
        assert_eq!(
            epoch_millis_to_rfc3339(1_709_164_800_000),
            "2024-02-29T00:00:00Z"
        );
    }
}
