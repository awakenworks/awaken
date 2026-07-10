//! Data-subject attribution and the consent/erasure resolver port (ADR-0050).
//!
//! The party a run/request is attributed to is a neutral-core concept. The
//! runtime carries only an opaque [`DataSubjectId`] (never the subject's
//! attributes or PII); the [`DataSubjectResolver`] port — the one customization
//! seam — answers the consent ceiling and executes erasure, consulted at the
//! run/turn boundary and never on the inference hot path.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capture::ContentCapture;

/// Opaque identifier of the data subject a run/request is attributed to. Minted
/// `dsub_…` by the subject store; an opaque string everywhere else (data
/// minimisation — the hot path never holds subject attributes).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DataSubjectId(pub String);

impl DataSubjectId {
    /// Borrow the underlying opaque id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The purpose a capture is attributed to; consent is per-purpose (purpose
/// limitation, GDPR Art. 5). Closed set. Wire values are snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// Recording prompt/completion/tool content into telemetry (traces).
    TelemetryContent,
    /// Recording a real run into an eval fixture/dataset.
    EvalRecording,
}

/// Receipt of a right-to-erasure (GDPR Art. 17) fan-out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErasureReceipt {
    /// Number of content records removed across all stores.
    pub records_removed: usize,
}

/// Resolves the consent ceiling and executes erasure for a data subject (D10a).
/// The **one** customization seam: swap the impl to change where subject facts
/// come from. Consulted at the run/turn boundary, never on the inference hot
/// path.
#[async_trait]
pub trait DataSubjectResolver: Send + Sync {
    /// The capture ceiling this subject's consent permits for `purpose`. A real
    /// resolver returns `Full` only when an active grant exists, else clamps to
    /// `Structured`.
    async fn consent_ceiling(&self, subject: &DataSubjectId, purpose: Purpose) -> ContentCapture;

    /// Erase all content attributed to `subject`; returns a receipt.
    async fn erase(&self, subject: &DataSubjectId) -> ErasureReceipt;
}

/// Where the runtime writes captured prompt/completion/tool content when the
/// [`CaptureDecision`](crate::CaptureDecision) permits it (ADR-0050). The sink
/// is subject-tagged so the same store can later erase by subject. Best-effort,
/// off the committed path — it never blocks or fails a run.
#[async_trait]
pub trait CaptureSink: Send + Sync {
    /// Record one captured-content item, attributed to `subject` for `purpose`.
    async fn record(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        kind: crate::capture::ContentKind,
        content: &str,
    );
}

/// A content store that can erase all records attributed to a data subject
/// (GDPR Art. 17, ADR-0050 D7). A resolver fans an erasure out across every
/// registered eraser; each returns the number of records it removed.
#[async_trait]
pub trait ContentEraser: Send + Sync {
    /// Erase content attributed to `subject`; return the number of records removed.
    async fn erase_subject(&self, subject: &DataSubjectId) -> usize;
}

/// The standalone/open null object: no consent subsystem, so it never clamps
/// (the env default + config ceiling decide); erasure is a no-op receipt.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullResolver;

#[async_trait]
impl DataSubjectResolver for NullResolver {
    async fn consent_ceiling(&self, _subject: &DataSubjectId, _purpose: Purpose) -> ContentCapture {
        // No consent tracking ⇒ consent does not restrict; the ceiling/env decide.
        ContentCapture::Full
    }

    async fn erase(&self, _subject: &DataSubjectId) -> ErasureReceipt {
        ErasureReceipt::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_resolver_never_clamps() {
        let r = NullResolver;
        let s = DataSubjectId("dsub_1".into());
        assert_eq!(
            r.consent_ceiling(&s, Purpose::TelemetryContent).await,
            ContentCapture::Full
        );
        assert_eq!(r.erase(&s).await, ErasureReceipt::default());
    }

    #[test]
    fn purpose_wire_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&Purpose::TelemetryContent).unwrap(),
            "\"telemetry_content\""
        );
        assert_eq!(
            serde_json::from_str::<Purpose>("\"eval_recording\"").unwrap(),
            Purpose::EvalRecording
        );
    }

    #[test]
    fn subject_id_round_trips() {
        let id = DataSubjectId("dsub_abc".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"dsub_abc\"");
        assert_eq!(serde_json::from_str::<DataSubjectId>(&json).unwrap(), id);
    }

    #[test]
    fn subject_id_as_str_and_receipt_default() {
        assert_eq!(DataSubjectId("dsub_x".into()).as_str(), "dsub_x");
        assert_eq!(ErasureReceipt::default().records_removed, 0);
        // Purpose hashes/eq for map keys.
        assert_eq!(Purpose::EvalRecording, Purpose::EvalRecording);
    }
}
