//! The live progress channel payload: a run id plus one neutral [`AgentEvent`].
//!
//! The live stream is in-process and best-effort (G10/G13); it carries the one
//! neutral event vocabulary directly — `Fact::RunStarted` opens the stream,
//! then `Live` increments flow as the model produces them. The authoritative
//! end always comes from the committed fold, never this channel, so there is
//! no separate live event enum (ADR-0058: `stream::Kind` folded into
//! [`AgentEvent`]).

use crate::event::AgentEvent;
use serde::{Deserialize, Serialize};

/// One live progress event: which run it belongs to, and the neutral event itself.
///
/// Keep this two-field shape source-compatible with external adapters that
/// construct `Event { run_id, kind }` directly. Runtime-only delivery context
/// belongs to [`Observation`], not to this public event value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub run_id: crate::agent::run::Id,
    pub kind: AgentEvent,
}

/// Stable live coordinate of one model response inside an assistant Step.
///
/// `response` is zero for the ordinary response and advances for every
/// `MaxTokens` continuation of that same Step. It is observation metadata, not
/// committed state: committed Message ids remain the authority and can rebuild
/// the same coordinate after a restart.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssistantResponseCoordinate {
    pub thread_id: crate::agent::thread::Id,
    pub step: usize,
    pub response: usize,
}

/// One delivery through the existing best-effort live channel.
///
/// [`Event`] remains the source-compatible neutral payload. This envelope is
/// the sole owner of optional Runtime delivery context; it does not add another
/// event vocabulary, channel, registry, or durable source of truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub event: Event,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_response: Option<AssistantResponseCoordinate>,
}

impl Observation {
    /// Construct a model-output observation with the exact logical Thread and
    /// response coordinate needed by isolated live observers.
    #[must_use]
    pub fn assistant_delta(
        run_id: crate::agent::run::Id,
        thread_id: crate::agent::thread::Id,
        step: usize,
        response: usize,
        kind: AgentEvent,
    ) -> Self {
        Self {
            event: Event { run_id, kind },
            assistant_response: Some(AssistantResponseCoordinate {
                thread_id,
                step,
                response,
            }),
        }
    }
}

impl From<Event> for Observation {
    fn from(event: Event) -> Self {
        Self {
            event,
            assistant_response: None,
        }
    }
}
