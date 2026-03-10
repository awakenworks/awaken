use crate::runtime::state::SerializedStateAction;
use crate::thread::Message;
use std::sync::Arc;
use tirea_state::TrackedPatch;

/// Incremental output from a run step — the new messages, patches, and
/// serialized state actions accumulated since the last `take_delta()`.
#[derive(Debug, Clone, Default)]
pub struct RunDelta {
    pub messages: Vec<Arc<Message>>,
    pub patches: Vec<TrackedPatch>,
    pub state_actions: Vec<SerializedStateAction>,
}

impl RunDelta {
    /// Returns true if there are no new messages, patches, or serialized state actions.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty() && self.patches.is_empty() && self.state_actions.is_empty()
    }
}
