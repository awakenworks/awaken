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

/// Format epoch milliseconds as canonical RFC 3339 UTC at second precision.
#[must_use]
pub fn epoch_millis_to_rfc3339(epoch_millis: u64) -> String {
    let seconds = (epoch_millis / 1_000) as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        seconds_of_day / 3_600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
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

    #[test]
    fn epoch_milliseconds_have_one_canonical_projection() {
        // Cause/effect table: zero -> epoch; subsecond -> same second;
        // leap-day boundary -> exact Gregorian date.
        assert_eq!(epoch_millis_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_millis_to_rfc3339(999), "1970-01-01T00:00:00Z");
        assert_eq!(
            epoch_millis_to_rfc3339(1_709_164_800_000),
            "2024-02-29T00:00:00Z"
        );
    }
}
