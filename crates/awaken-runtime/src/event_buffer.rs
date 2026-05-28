//! Runtime-owned staging buffer for canonical event drafts.

use awaken_runtime_contract::contract::commit_coordinator::{
    CanonicalEventStager, StagedCanonicalEvent,
};
use awaken_runtime_contract::contract::event_store::CanonicalEventDraft;
use parking_lot::Mutex;

/// In-process staging buffer for canonical event drafts produced during one
/// run activation.
#[derive(Debug, Default)]
pub struct EventBuffer {
    drafts: Mutex<Vec<StagedCanonicalEvent>>,
}

impl EventBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn drain(&self) -> Vec<StagedCanonicalEvent> {
        let mut guard = self.drafts.lock();
        std::mem::take(&mut *guard)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.drafts.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.drafts.lock().is_empty()
    }
}

impl CanonicalEventStager for EventBuffer {
    fn stage(&self, draft: CanonicalEventDraft) {
        self.drafts.lock().push(StagedCanonicalEvent::new(draft));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::contract::event_store::{
        CanonicalEventDraft, CanonicalEventKind, EventScope, EventVisibility,
    };
    use serde_json::json;

    fn sample_draft(kind: &str) -> CanonicalEventDraft {
        let mut draft = CanonicalEventDraft::new(
            vec![EventScope::thread("t-1"), EventScope::run("r-1")],
            CanonicalEventKind::new(kind).unwrap(),
            json!({"kind": kind}),
            "test",
        )
        .unwrap();
        draft.visibility = EventVisibility::Public;
        draft
    }

    #[test]
    fn stage_and_drain_preserve_order() {
        let buffer = EventBuffer::new();
        assert!(buffer.is_empty());

        buffer.stage(sample_draft("RunStarted"));
        buffer.stage(sample_draft("ToolCallReady"));
        buffer.stage(sample_draft("RunCompleted"));

        assert_eq!(buffer.len(), 3);
        let drained = buffer.drain();
        let kinds: Vec<&str> = drained
            .iter()
            .map(|s| s.draft.event_kind.as_str())
            .collect();
        assert_eq!(kinds, vec!["RunStarted", "ToolCallReady", "RunCompleted"]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn drain_empties_buffer_even_on_repeat() {
        let buffer = EventBuffer::new();
        buffer.stage(sample_draft("RunStarted"));
        let _ = buffer.drain();
        assert!(buffer.is_empty());
        assert!(buffer.drain().is_empty());
    }
}
