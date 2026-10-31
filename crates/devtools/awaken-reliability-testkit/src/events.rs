//! Isolated structured-event capture; attach with `WithSubscriber` to each
//! tested future. No global subscriber or product state is installed.
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use awaken_sync::LeafLock;
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

#[derive(Clone)]
pub struct CapturedEvents {
    events: Arc<LeafLock<Vec<BTreeMap<String, String>>>>,
    next_span: Arc<AtomicU64>,
    // Keep a second registered Dispatch alive, but never install it. With only
    // one Dispatch, tracing-core 0.1.36 computes new callsite interest from the
    // current thread, which may have NoSubscriber in a concurrent test. That
    // thread can otherwise cache Never for an event this capture needs later.
    _interest_guard: tracing::Dispatch,
}

impl Default for CapturedEvents {
    fn default() -> Self {
        Self {
            events: Arc::default(),
            next_span: Arc::default(),
            _interest_guard: tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()),
        }
    }
}

impl CapturedEvents {
    pub fn snapshot(&self) -> Vec<BTreeMap<String, String>> {
        self.events.lock().clone()
    }
}

struct Fields(BTreeMap<String, String>);
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
}
impl Subscriber for CapturedEvents {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed) + 1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        fields
            .0
            .insert("target".into(), event.metadata().target().into());
        self.events.lock().push(fields.0);
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstrumented_first_use_cannot_disable_scoped_capture() {
        // Cause/effect table: C1 a scoped capture Dispatch exists; C2 another
        // thread first visits the shared callsite with NoSubscriber; C3 the
        // capture visits it; C4 an unrelated thread visits it again. R1 C1+C2
        // must not cache a global rejection; R2 C3 records exactly its fields;
        // R3 C4 must not leak into the scoped capture. An explicit join fixes
        // the order without sleeps or probabilistic scheduler contention.
        // Run this test alone as well as in the suite: other live Dispatches
        // can mask tracing-core's single-Dispatch optimization.
        fn shared_callsite(value: u64) {
            tracing::warn!(probe_elapsed_ms = value, "scoped capture regression");
        }
        let captured = CapturedEvents::default();
        let dispatch = tracing::Dispatch::new(captured.clone());
        std::thread::spawn(|| shared_callsite(1)).join().unwrap();
        tracing::dispatcher::with_default(&dispatch, || shared_callsite(2));
        std::thread::spawn(|| shared_callsite(3)).join().unwrap();
        let events = captured.snapshot();
        assert_eq!(events.len(), 1, "R1-R3: {events:?}");
        assert_eq!(events[0]["probe_elapsed_ms"], "2", "R2");
    }
}
