//! Data governance ports over the Runtime SDK's capture vocabulary (ADR-0050).
//!
//! The Runtime SDK keeps what the engine reads when it captures content: the
//! opaque [`DataSubjectId`], the [`Purpose`] and the capture sink. Everything
//! that gives a subject meaning — the consent ceiling, erasure across content
//! stores and organization-wide privacy export — is Platform governance and
//! lives here (ADR-0086 §2). The consent port is consulted at the Run boundary,
//! never on the inference hot path.

#![warn(unreachable_pub)]

use async_trait::async_trait;
use awaken_runtime_contract::ContentCapture;
use awaken_runtime_contract::data_subject::{DataSubjectId, Purpose};
use serde::{Deserialize, Serialize};

/// Receipt of a right-to-erasure (GDPR Art. 17) fan-out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErasureReceipt {
    /// Number of content records removed across all stores.
    pub records_removed: usize,
}

/// One product-owned record returned by an organization privacy export.
///
/// The envelope keeps cross-process routing neutral while the product remains
/// the sole owner of the payload schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrganizationPrivacyRecord {
    pub subject: DataSubjectId,
    pub payload: serde_json::Value,
}

/// A product-level privacy export over one exact organization, optionally
/// narrowed to a known data subject.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OrganizationPrivacyExport {
    pub records: Vec<OrganizationPrivacyRecord>,
}

/// A product-owned privacy export failed or the requested scope was invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("privacy export failed: {0}")]
pub struct PrivacyExportError(pub String);

/// A right-to-erasure (GDPR Art. 17) failure. A content-store DELETE or the
/// accountability write did not complete, so the erasure did **not** happen — the
/// caller must learn this instead of receiving a clean success receipt. Erasure is
/// fail-closed: an unsurfaced backend error is a compliance breach (the subject is
/// told their data was erased when it was not), so every backend failure propagates
/// as this error rather than being swallowed into a `records_removed: 0` receipt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("erasure failed: {0}")]
pub struct ErasureError(pub String);

/// Consent-query port consumed at the Coordinator Run boundary.
///
/// Control owns the subject aggregate. A split Coordinator reaches this port
/// through an authenticated adapter; AllInOne injects the same local source.
/// Neither form gives Coordinator access to Control persistence.
#[async_trait]
pub trait DataSubjectConsentSource: Send + Sync {
    /// The capture ceiling this subject's consent permits for `purpose`. A real
    /// resolver returns `Full` only when an active grant exists, else clamps to
    /// `Structured`.
    async fn consent_ceiling(&self, subject: &DataSubjectId, purpose: Purpose) -> ContentCapture;
}

/// Executes the Control-owned erasure process for a data subject (D10a).
/// Consent reads reuse the one [`DataSubjectConsentSource`] implementation;
/// erasure additionally coordinates the content-owning service ports.
#[async_trait]
pub trait DataSubjectResolver: DataSubjectConsentSource {
    /// Erase all content attributed to `subject`; returns a receipt on success.
    /// Fail-closed: if any content-store DELETE or the accountability write fails,
    /// this surfaces an [`ErasureError`] — a partial/failed erasure must never be
    /// reported as a clean success receipt.
    async fn erase(&self, subject: &DataSubjectId) -> Result<ErasureReceipt, ErasureError>;
}

/// Product-level organization privacy process manager.
///
/// This is distinct from [`DataSubjectResolver`]: it resolves the organization
/// membership set and delegates each subject effect to that one canonical
/// resolver instead of introducing another content-store fan-out.
#[async_trait]
pub trait OrganizationPrivacyResolver: Send + Sync {
    async fn erase_organization(
        &self,
        organization_id: &str,
    ) -> Result<ErasureReceipt, ErasureError>;

    async fn export_organization(
        &self,
        organization_id: &str,
        subject: Option<&DataSubjectId>,
    ) -> Result<OrganizationPrivacyExport, PrivacyExportError>;
}

/// A content store that can erase all records attributed to a data subject
/// (GDPR Art. 17, ADR-0050 D7). A resolver fans an erasure out across every
/// registered eraser; each returns the number of records it removed. Since a
/// process may fail after the effect but before its saga checkpoint commits,
/// implementations must be idempotent by subject and replay the same durable
/// receipt on retry.
#[async_trait]
pub trait ContentEraser: Send + Sync {
    /// Erase content attributed to `subject`; return the stable number of records
    /// removed. Repeating the same subject must replay that receipt rather than
    /// perform or count a second logical erasure.
    /// Fail-closed: a backend DELETE failure surfaces as an [`ErasureError`] and is
    /// never collapsed into a `0` count (which a clean "nothing matched" also
    /// reports) — the resolver must be able to tell a real erasure from a no-op.
    async fn erase_subject(&self, subject: &DataSubjectId) -> Result<usize, ErasureError>;
}

/// The standalone/open null object: no consent subsystem, so it never clamps
/// (the env default + config ceiling decide); erasure is a no-op receipt.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullResolver;

#[async_trait]
impl DataSubjectConsentSource for NullResolver {
    async fn consent_ceiling(&self, _subject: &DataSubjectId, _purpose: Purpose) -> ContentCapture {
        // No consent tracking ⇒ consent does not restrict; the ceiling/env decide.
        ContentCapture::Full
    }
}

#[async_trait]
impl DataSubjectResolver for NullResolver {
    async fn erase(&self, _subject: &DataSubjectId) -> Result<ErasureReceipt, ErasureError> {
        Ok(ErasureReceipt::default())
    }
}

#[async_trait]
impl OrganizationPrivacyResolver for NullResolver {
    async fn erase_organization(
        &self,
        _organization_id: &str,
    ) -> Result<ErasureReceipt, ErasureError> {
        Ok(ErasureReceipt::default())
    }

    async fn export_organization(
        &self,
        _organization_id: &str,
        _subject: Option<&DataSubjectId>,
    ) -> Result<OrganizationPrivacyExport, PrivacyExportError> {
        Ok(OrganizationPrivacyExport::default())
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
        assert_eq!(r.erase(&s).await.unwrap(), ErasureReceipt::default());
    }

    #[test]
    fn subject_id_as_str_and_receipt_default() {
        assert_eq!(DataSubjectId("dsub_x".into()).as_str(), "dsub_x");
        assert_eq!(ErasureReceipt::default().records_removed, 0);
        // Purpose hashes/eq for map keys.
        assert_eq!(Purpose::EvalRecording, Purpose::EvalRecording);
    }

    #[test]
    fn erasure_receipt_wire_shape() {
        // The receipt keeps the shape it had in the Runtime SDK contract, so
        // persisted saga checkpoints and the erasure HTTP body still decode.
        let receipt = ErasureReceipt { records_removed: 3 };
        let wire = serde_json::to_value(&receipt).unwrap();
        assert_eq!(wire, serde_json::json!({"records_removed": 3}));
        assert_eq!(
            serde_json::from_value::<ErasureReceipt>(wire).unwrap(),
            receipt
        );
    }

    #[test]
    fn organization_privacy_export_keeps_its_wire_shape() {
        let export = OrganizationPrivacyExport {
            records: vec![OrganizationPrivacyRecord {
                subject: DataSubjectId("dsub_a".into()),
                payload: serde_json::json!({"k": 1}),
            }],
        };
        let wire = serde_json::to_value(&export).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"records": [{"subject": "dsub_a", "payload": {"k": 1}}]})
        );
        assert_eq!(
            serde_json::from_value::<OrganizationPrivacyExport>(wire).unwrap(),
            export
        );
    }
}
