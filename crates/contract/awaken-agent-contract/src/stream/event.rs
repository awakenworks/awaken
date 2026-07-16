//! The live progress channel payload: a run id plus one neutral [`AgentEvent`].
//!
//! The live stream is in-process and best-effort (G10/G13); it carries the one
//! neutral event vocabulary directly — `Committed::RunStarted` opens the stream,
//! then `Live` increments flow as the model produces them. The authoritative
//! terminus always comes from the committed fold, never this channel, so there is
//! no separate live event enum (ADR-0058: `stream::Kind` folded into
//! [`AgentEvent`]).

use crate::event::AgentEvent;

/// One live progress event: which run it belongs to, and the neutral event itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub run_id: crate::agent::run::Id,
    pub kind: AgentEvent,
}
