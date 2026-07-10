//! Captured-content store (ADR-0050 D7): the erasable, TTL'd sink that captured
//! prompt/completion/tool content is written to, tagged by data subject so GDPR
//! Art. 17 erasure removes exactly that subject's content and a TTL sweep
//! enforces storage limitation (Art. 5(e)). Implements
//! [`ContentEraser`](awaken_runtime_contract::ContentEraser) so a resolver fans
//! an erasure out to it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_runtime_contract::{ContentEraser, DataSubjectId, Purpose};

/// One captured-content record, tagged by subject + purpose + record time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedRecord {
    pub id: String,
    pub subject: DataSubjectId,
    pub purpose: Purpose,
    /// Epoch milliseconds the content was recorded (for TTL).
    pub recorded_at: i64,
    pub content: String,
}

/// In-memory captured-content store. A real backend would persist rows; the
/// erasure/TTL contract is identical.
#[derive(Default)]
pub struct InMemoryCapturedContentStore {
    inner: Mutex<Vec<CapturedRecord>>,
    seq: AtomicU64,
}

impl InMemoryCapturedContentStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a captured-content item; returns its `cap_…` id.
    pub fn record(
        &self,
        subject: DataSubjectId,
        purpose: Purpose,
        content: impl Into<String>,
        now: i64,
    ) -> String {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let id = format!("cap_{n:016}");
        self.inner.lock().unwrap().push(CapturedRecord {
            id: id.clone(),
            subject,
            purpose,
            recorded_at: now,
            content: content.into(),
        });
        id
    }

    /// Remove records older than `ttl_millis` as of `now` (Art. 5(e) storage
    /// limitation); returns the number swept.
    pub fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize {
        let mut v = self.inner.lock().unwrap();
        let before = v.len();
        v.retain(|r| now - r.recorded_at < ttl_millis);
        before - v.len()
    }

    /// Current record count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl ContentEraser for InMemoryCapturedContentStore {
    async fn erase_subject(&self, subject: &DataSubjectId) -> usize {
        let mut v = self.inner.lock().unwrap();
        let before = v.len();
        v.retain(|r| &r.subject != subject);
        before - v.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> InMemoryCapturedContentStore {
        let s = InMemoryCapturedContentStore::new();
        s.record(
            DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x1",
            100,
        );
        s.record(
            DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x2",
            100,
        );
        s.record(DataSubjectId("b".into()), Purpose::EvalRecording, "y1", 100);
        s
    }

    #[tokio::test]
    async fn erase_removes_exactly_the_subjects_records() {
        let s = store();
        assert_eq!(s.len(), 3);
        let removed = s.erase_subject(&DataSubjectId("a".into())).await;
        assert_eq!(removed, 2, "both of subject a's records removed");
        assert_eq!(s.len(), 1, "subject b's record remains");
        // Erasing an unknown subject removes nothing.
        assert_eq!(s.erase_subject(&DataSubjectId("ghost".into())).await, 0);
    }

    #[test]
    fn ttl_sweep_removes_expired_records() {
        let s = store(); // all recorded at t=100
        // At now=150 with ttl=100, nothing is older than the ttl.
        assert_eq!(s.sweep_expired(100, 150), 0);
        assert_eq!(s.len(), 3);
        // At now=250 with ttl=100, all 3 (age 150) are expired.
        assert_eq!(s.sweep_expired(100, 250), 3);
        assert!(s.is_empty());
    }
}
