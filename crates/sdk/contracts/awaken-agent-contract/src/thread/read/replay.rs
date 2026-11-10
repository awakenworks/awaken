use crate::agent::{
    message::Message,
    run::{Id as RunId, RunState},
    state::Command,
    thread::Id as ThreadId,
};
use crate::audit::record::Record;

/// Immutable facts through a caller-selected committed store position.
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedThreadPrefix {
    pub thread_id: ThreadId,
    pub runs: Vec<(u64, RunId, RunState)>,
    pub messages: Vec<(u64, Message)>,
    pub state: Vec<(u64, Command)>,
    pub events: Vec<Record>,
}
