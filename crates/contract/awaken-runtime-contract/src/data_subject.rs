//! Data-subject attribution for captured content (ADR-0050).
//!
//! The party a run/request is attributed to is a neutral-core concept. The
//! runtime carries only an opaque [`DataSubjectId`] (never the subject's
//! attributes or PII), the [`Purpose`] a capture serves, and the [`CaptureSink`]
//! that receives it. Consent, subject resolution, erasure and organization
//! privacy give the subject meaning; they are Platform governance and live in
//! `awaken-data-governance-contract` (ADR-0086 §2).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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
    ) -> Result<(), CaptureError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaptureError {
    #[error("capture is fenced for erased data subject")]
    SubjectErased,
    #[error("capture store failed: {0}")]
    Store(String),
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
