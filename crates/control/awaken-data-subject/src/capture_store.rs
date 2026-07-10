//! Captured-content store (ADR-0050 D7): the erasable, TTL'd sink that captured
//! prompt/completion/tool content is written to, tagged by data subject so GDPR
//! Art. 17 erasure removes exactly that subject's content and a TTL sweep
//! enforces storage limitation (Art. 5(e)). Implements
//! [`ContentEraser`](awaken_runtime_contract::ContentEraser) so a resolver fans
//! an erasure out to it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_runtime_contract::{CaptureSink, ContentEraser, ContentKind, DataSubjectId, Purpose};

/// One captured-content record, tagged by subject + purpose + record time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedRecord {
    pub id: String,
    pub subject: DataSubjectId,
    pub purpose: Purpose,
    /// Epoch milliseconds the content was recorded (for TTL).
    pub recorded_at: i64,
    pub content: String,
    /// Art. 18 restriction: a restricted record is frozen — kept in storage but
    /// excluded from erasure, TTL sweep, and every read path.
    #[doc(hidden)]
    pub restricted: bool,
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

    /// Insert a captured-content item at an explicit time; returns its `cap_…`
    /// id. The [`CaptureSink`] impl calls this with the wall clock.
    pub fn insert(
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
            restricted: false,
        });
        id
    }

    /// Restrict a subject's records (GDPR Art. 18): freeze them so they survive
    /// erasure + TTL and are hidden from reads. Returns the number restricted.
    pub fn restrict(&self, subject: &DataSubjectId) -> usize {
        let mut v = self.inner.lock().unwrap();
        let mut n = 0;
        for r in v.iter_mut() {
            if &r.subject == subject && !r.restricted {
                r.restricted = true;
                n += 1;
            }
        }
        n
    }

    /// Lift the restriction on a subject's records (Art. 18(3): inform the
    /// subject before doing so). Returns the number released.
    pub fn release(&self, subject: &DataSubjectId) -> usize {
        let mut v = self.inner.lock().unwrap();
        let mut n = 0;
        for r in v.iter_mut() {
            if &r.subject == subject && r.restricted {
                r.restricted = false;
                n += 1;
            }
        }
        n
    }

    /// Remove records older than `ttl_millis` as of `now` (Art. 5(e) storage
    /// limitation); returns the number swept. Restricted records are exempt.
    pub fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize {
        let mut v = self.inner.lock().unwrap();
        let before = v.len();
        v.retain(|r| r.restricted || now - r.recorded_at < ttl_millis);
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

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl CaptureSink for InMemoryCapturedContentStore {
    async fn record(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        _kind: ContentKind,
        content: &str,
    ) {
        self.insert(subject.clone(), purpose, content, now_millis());
    }
}

#[async_trait]
impl ContentEraser for InMemoryCapturedContentStore {
    async fn erase_subject(&self, subject: &DataSubjectId) -> usize {
        let mut v = self.inner.lock().unwrap();
        let before = v.len();
        // Restricted (Art. 18) records survive erasure — kept for the legal
        // purpose until released.
        v.retain(|r| &r.subject != subject || r.restricted);
        before - v.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> InMemoryCapturedContentStore {
        let s = InMemoryCapturedContentStore::new();
        s.insert(
            DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x1",
            100,
        );
        s.insert(
            DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x2",
            100,
        );
        s.insert(DataSubjectId("b".into()), Purpose::EvalRecording, "y1", 100);
        s
    }

    #[tokio::test]
    async fn capture_sink_records_then_erases_by_subject() {
        let s = InMemoryCapturedContentStore::new();
        s.record(
            &DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            ContentKind::InputMessages,
            "hello a@b.com",
        )
        .await;
        assert_eq!(s.len(), 1, "sink wrote one record");
        assert_eq!(s.erase_subject(&DataSubjectId("a".into())).await, 1);
        assert!(s.is_empty());
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

    #[tokio::test]
    async fn restricted_records_survive_erasure_and_ttl() {
        let s = InMemoryCapturedContentStore::new();
        s.insert(
            DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "x",
            100,
        );
        s.insert(
            DataSubjectId("a".into()),
            Purpose::TelemetryContent,
            "y",
            100,
        );
        assert_eq!(s.restrict(&DataSubjectId("a".into())), 2);

        // Erasure and TTL both skip restricted records.
        assert_eq!(s.erase_subject(&DataSubjectId("a".into())).await, 0);
        assert_eq!(s.sweep_expired(1, 10_000), 0);
        assert_eq!(s.len(), 2, "restricted records survive both");

        // Once released, erasure removes them.
        assert_eq!(s.release(&DataSubjectId("a".into())), 2);
        assert_eq!(s.erase_subject(&DataSubjectId("a".into())).await, 2);
        assert!(s.is_empty());
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
