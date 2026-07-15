//! The **data-subject** aggregate (ADR-0050): the party a run/request is
//! attributed to, its per-purpose consent grants, and the resolver that answers
//! the consent ceiling and executes erasure.
//!
//! A control/compliance-plane store (peer of `awaken-credential-vault`),
//! Org-scoped: a subject belongs to exactly one Org (the GDPR data controller).
//! The neutral `DataSubjectId` / `Purpose` / `ContentCapture` / `ErasureReceipt`
//! vocabulary is reused from `awaken-runtime-contract`; the Anthropic
//! `UserProfile` wire shape is a *projection* over this aggregate (Slice 6).

mod capture_store;
#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "postgres")]
mod postgres_capture;
mod schema;
mod sqlite;
mod sqlite_capture;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use awaken_runtime_contract::{ContentCapture, DataSubjectId, ErasureReceipt, Purpose};
pub use capture_store::{CapturedRecord, InMemoryCapturedContentStore};
#[cfg(feature = "postgres")]
pub use postgres::{PgDataSubjectRepo, PgStoreError};
#[cfg(feature = "postgres")]
pub use postgres_capture::PgCapturedContentStore;
pub use schema::{BUNDLE_ID, data_subject_bundle};
pub use sqlite::{SqliteDataSubjectRepo, StoreError};
pub use sqlite_capture::SqliteCapturedContentStore;

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Status of a consent grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentStatus {
    /// Active grant — content capture is permitted for the purpose.
    Granted,
    /// Enrollment in flight; not yet a permit.
    Pending,
    /// Withdrawn — the record is kept for audit but permits nothing.
    Withdrawn,
}

/// The GDPR lawful basis a capture relies on. `Consent` is the default; the
/// others record that capture is justified without explicit end-user consent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LawfulBasis {
    /// End-user consent (the default and most common basis).
    #[default]
    Consent,
    /// Performance of a contract.
    Contract,
    /// Legitimate interest.
    LegitimateInterest,
}

/// One per-purpose consent grant on a [`DataSubject`]. Withdrawal flips
/// [`status`](Self::status) to `Withdrawn` (never deleted — audit proof).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentGrant {
    pub purpose: Purpose,
    pub status: ConsentStatus,
    #[serde(default)]
    pub basis: LawfulBasis,
    /// Epoch milliseconds when the grant was recorded.
    pub granted_at: i64,
    /// Consent-text version; a change invalidates the grant (re-consent).
    pub version: String,
}

/// The data-subject aggregate root. Consent grants are part of this aggregate
/// (no independent lifecycle); at most one active grant per purpose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSubject {
    pub id: DataSubjectId,
    /// Owning Org (the data controller); a subject belongs to exactly one.
    pub org: String,
    /// The developer's own id for this subject (join key, not enforced unique).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(default)]
    pub consents: Vec<ConsentGrant>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Epoch-millis when this subject's content was erased (GDPR Art. 17). The
    /// subject record itself is **retained** as accountability proof (Art. 5(2)/
    /// 7(1)) — this stamps that erasure happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erased_at: Option<i64>,
}

impl DataSubject {
    /// A fresh subject with no consent grants.
    #[must_use]
    pub fn new(id: DataSubjectId, org: impl Into<String>, at: i64) -> Self {
        Self {
            id,
            org: org.into(),
            external_id: None,
            consents: Vec::new(),
            created_at: at,
            updated_at: at,
            erased_at: None,
        }
    }

    /// Mark erasure (Art. 17): withdraw every consent grant (kept as audit proof,
    /// not deleted) and stamp `erased_at`. The subject record is retained for
    /// accountability; only the content it pointed to is deleted (by the erasers).
    pub fn mark_erased(&mut self, at: i64) {
        for grant in &mut self.consents {
            grant.status = ConsentStatus::Withdrawn;
        }
        self.erased_at = Some(at);
        self.updated_at = at;
    }

    /// The capture ceiling this subject's consent permits for `purpose`: `Full`
    /// only when an active (`Granted`) grant exists **and** no `Withdrawn` grant
    /// for the same purpose vetoes it; otherwise `Structured`.
    ///
    /// The write path (`upsert_consent`) keeps at most one grant per purpose, so a
    /// `Granted`/`Withdrawn` pair for one purpose should never arise. But this is a
    /// GDPR boundary and the aggregate can be *deserialized* from a legacy or
    /// tampered blob that bypasses that invariant — so the read fails **closed**: a
    /// withdrawal is honoured even if a stale `Granted` duplicate survives beside
    /// it. (Fixing it here, not only in the writer, means no reader can leak.)
    #[must_use]
    pub fn consent_ceiling(&self, purpose: Purpose) -> ContentCapture {
        let mut granted = false;
        for g in &self.consents {
            if g.purpose != purpose {
                continue;
            }
            match g.status {
                // A withdrawal for this purpose vetoes capture outright.
                ConsentStatus::Withdrawn => return ContentCapture::Structured,
                ConsentStatus::Granted => granted = true,
                ConsentStatus::Pending => {}
            }
        }
        if granted {
            ContentCapture::Full
        } else {
            ContentCapture::Structured
        }
    }

    /// Record `grant`, superseding any prior grant for the same purpose.
    pub fn upsert_consent(&mut self, grant: ConsentGrant) {
        self.consents.retain(|g| g.purpose != grant.purpose);
        self.consents.push(grant);
    }
}

/// Errors from the data-subject store.
#[derive(Debug, thiserror::Error)]
pub enum DataSubjectError {
    #[error("data_subject not found: {0}")]
    NotFound(String),
    #[error("storage: {0}")]
    Storage(String),
}

/// Persistence port for the [`DataSubject`] aggregate (repository-per-aggregate).
#[async_trait]
pub trait DataSubjectRepo: Send + Sync {
    /// Upsert a subject.
    async fn put(&self, subject: DataSubject) -> Result<(), DataSubjectError>;
    /// Fetch a subject by id.
    async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError>;
    /// List an Org's subjects, in insertion order.
    async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError>;
    /// Delete a subject record (its consent history); returns Ok even if absent.
    async fn delete(&self, id: &DataSubjectId) -> Result<(), DataSubjectError>;
}

/// In-memory [`DataSubjectRepo`] (tests / ephemeral single-process).
#[derive(Default)]
pub struct InMemoryDataSubjectRepo {
    inner: Mutex<BTreeMap<String, DataSubject>>,
    order: Mutex<Vec<String>>,
}

impl InMemoryDataSubjectRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl DataSubjectRepo for InMemoryDataSubjectRepo {
    async fn put(&self, subject: DataSubject) -> Result<(), DataSubjectError> {
        let key = subject.id.0.clone();
        let mut map = self.inner.lock().unwrap();
        if !map.contains_key(&key) {
            self.order.lock().unwrap().push(key.clone());
        }
        map.insert(key, subject);
        Ok(())
    }

    async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
        self.inner
            .lock()
            .unwrap()
            .get(&id.0)
            .cloned()
            .ok_or_else(|| DataSubjectError::NotFound(id.0.clone()))
    }

    async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
        let map = self.inner.lock().unwrap();
        Ok(self
            .order
            .lock()
            .unwrap()
            .iter()
            .filter_map(|k| map.get(k))
            .filter(|s| s.org == org)
            .cloned()
            .collect())
    }

    async fn delete(&self, id: &DataSubjectId) -> Result<(), DataSubjectError> {
        self.inner.lock().unwrap().remove(&id.0);
        self.order.lock().unwrap().retain(|k| k != &id.0);
        Ok(())
    }
}

/// Bridges the neutral [`DataSubjectResolver`](awaken_runtime_contract::DataSubjectResolver)
/// port to a [`DataSubjectRepo`] (ADR-0050 D10a). An unknown subject resolves to
/// `Structured` (no content without a known, consenting subject). Holds an
/// `Arc`-shared repo so a consent-write path and the resolver see one store.
pub struct RepoDataSubjectResolver {
    repo: std::sync::Arc<dyn DataSubjectRepo>,
    erasers: Vec<std::sync::Arc<dyn awaken_runtime_contract::ContentEraser>>,
}

impl RepoDataSubjectResolver {
    #[must_use]
    pub fn new(repo: std::sync::Arc<dyn DataSubjectRepo>) -> Self {
        Self {
            repo,
            erasers: Vec::new(),
        }
    }

    /// Register a content store to fan an erasure out to (GDPR Art. 17). Each
    /// registered eraser's removed-count sums into the [`ErasureReceipt`].
    #[must_use]
    pub fn with_eraser(
        mut self,
        eraser: std::sync::Arc<dyn awaken_runtime_contract::ContentEraser>,
    ) -> Self {
        self.erasers.push(eraser);
        self
    }
}

#[async_trait]
impl awaken_runtime_contract::DataSubjectResolver for RepoDataSubjectResolver {
    async fn consent_ceiling(&self, subject: &DataSubjectId, purpose: Purpose) -> ContentCapture {
        match self.repo.get(subject).await {
            Ok(s) => s.consent_ceiling(purpose),
            Err(_) => ContentCapture::Structured,
        }
    }

    async fn erase(&self, subject: &DataSubjectId) -> ErasureReceipt {
        // Fan the erasure out across every registered content store, summing the
        // records removed.
        let mut records_removed = 0;
        for eraser in &self.erasers {
            records_removed += eraser.erase_subject(subject).await;
        }
        // Retain the subject record as accountability proof (Art. 5(2)/7(1)):
        // withdraw its consents + stamp `erased_at`, rather than deleting it.
        if let Ok(mut s) = self.repo.get(subject).await {
            s.mark_erased(now_millis());
            let _ = self.repo.put(s).await;
        }
        ErasureReceipt { records_removed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::DataSubjectResolver;

    fn granted(purpose: Purpose) -> ConsentGrant {
        ConsentGrant {
            purpose,
            status: ConsentStatus::Granted,
            basis: LawfulBasis::Consent,
            granted_at: 1,
            version: "v1".into(),
        }
    }

    // DS3: a Pending (in-flight, not-yet-a-permit) grant ceils to Structured, the
    // same as no grant at all — only an active `Granted` grant lifts to Full.
    #[test]
    fn consent_ceiling_pending_grant_is_structured() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(ConsentGrant {
            status: ConsentStatus::Pending,
            ..granted(Purpose::TelemetryContent)
        });
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Structured
        );
    }

    // FAIL-CLOSED: if the aggregate is deserialized from a legacy/tampered blob
    // that holds BOTH a Withdrawn and a Granted grant for one purpose (a state the
    // writer never produces), the read must honour the withdrawal — otherwise a
    // withdrawn subject leaks as `Full` (a GDPR breach). `consents` is a pub field,
    // so we can construct that broken state directly, as a bad blob would.
    #[test]
    fn consent_ceiling_withdrawal_vetoes_a_stale_granted_duplicate() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.consents.push(granted(Purpose::TelemetryContent));
        s.consents.push(ConsentGrant {
            status: ConsentStatus::Withdrawn,
            ..granted(Purpose::TelemetryContent)
        });
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Structured,
            "a Withdrawn grant must veto a coexisting stale Granted grant"
        );
        // order-independent: withdrawal first, grant second, same verdict.
        let mut s2 = DataSubject::new(DataSubjectId("dsub_2".into()), "org_1", 0);
        s2.consents.push(ConsentGrant {
            status: ConsentStatus::Withdrawn,
            ..granted(Purpose::TelemetryContent)
        });
        s2.consents.push(granted(Purpose::TelemetryContent));
        assert_eq!(
            s2.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Structured
        );
    }

    // upsert (b): at most one grant per purpose, even across several purposes —
    // re-upserting one purpose supersedes only that purpose's grant.
    #[test]
    fn upsert_keeps_at_most_one_grant_per_purpose() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(granted(Purpose::TelemetryContent));
        s.upsert_consent(granted(Purpose::EvalRecording));
        assert_eq!(s.consents.len(), 2, "two distinct purposes coexist");
        // Re-upsert TelemetryContent as Withdrawn: supersedes only that purpose.
        s.upsert_consent(ConsentGrant {
            status: ConsentStatus::Withdrawn,
            ..granted(Purpose::TelemetryContent)
        });
        assert_eq!(s.consents.len(), 2, "still at most one per purpose");
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Structured,
            "superseded to Withdrawn"
        );
        assert_eq!(
            s.consent_ceiling(Purpose::EvalRecording),
            ContentCapture::Full,
            "the other purpose is untouched"
        );
    }

    #[test]
    fn consent_ceiling_is_full_only_with_an_active_grant() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Structured
        );
        s.upsert_consent(granted(Purpose::TelemetryContent));
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Full
        );
        // A different purpose is unaffected.
        assert_eq!(
            s.consent_ceiling(Purpose::EvalRecording),
            ContentCapture::Structured
        );
    }

    #[test]
    fn mark_erased_withdraws_grants_but_keeps_them_as_audit() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(granted(Purpose::TelemetryContent));
        s.mark_erased(999);
        // The grant is RETAINED (accountability proof), just withdrawn.
        assert_eq!(s.consents.len(), 1);
        assert_eq!(s.consents[0].status, ConsentStatus::Withdrawn);
        assert_eq!(s.erased_at, Some(999));
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Structured
        );
    }

    #[test]
    fn upsert_supersedes_same_purpose_and_keeps_withdrawal() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(granted(Purpose::TelemetryContent));
        s.upsert_consent(ConsentGrant {
            status: ConsentStatus::Withdrawn,
            ..granted(Purpose::TelemetryContent)
        });
        assert_eq!(s.consents.len(), 1);
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Structured
        );
    }

    // upsert transition (Pending → Granted): completing an in-flight enrollment
    // supersedes the Pending grant and lifts the ceiling to Full.
    #[test]
    fn upsert_pending_then_granted_completes_enrollment() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(ConsentGrant {
            status: ConsentStatus::Pending,
            ..granted(Purpose::TelemetryContent)
        });
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Structured,
            "Pending does not permit"
        );
        s.upsert_consent(granted(Purpose::TelemetryContent));
        assert_eq!(s.consents.len(), 1, "still at most one per purpose");
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Full,
            "enrollment completed → Full"
        );
    }

    // upsert transition (Withdrawn → Granted): a re-consent after withdrawal
    // supersedes the withdrawal and restores the Full ceiling.
    #[test]
    fn upsert_reconsent_after_withdrawal_restores_full() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(ConsentGrant {
            status: ConsentStatus::Withdrawn,
            ..granted(Purpose::TelemetryContent)
        });
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Structured
        );
        // Re-consent with a fresh (newer) version.
        s.upsert_consent(ConsentGrant {
            version: "v2".into(),
            granted_at: 2,
            ..granted(Purpose::TelemetryContent)
        });
        assert_eq!(
            s.consents.len(),
            1,
            "withdrawal was superseded, not appended"
        );
        assert_eq!(s.consents[0].version, "v2");
        assert_eq!(
            s.consent_ceiling(Purpose::TelemetryContent),
            ContentCapture::Full
        );
    }

    // mark_erased on a subject with NO grants: stamps erased_at + updated_at and
    // leaves the (empty) consent set as-is — no residue, and safe to call.
    #[test]
    fn mark_erased_without_consents_stamps_and_leaves_no_residue() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.mark_erased(500);
        assert!(s.consents.is_empty(), "no grants to withdraw");
        assert_eq!(s.erased_at, Some(500));
        assert_eq!(s.updated_at, 500, "updated_at advances to erasure time");
    }

    // mark_erased withdraws grants of EVERY prior status (Granted and Pending
    // alike) — erasure leaves no live-or-pending consent behind.
    #[test]
    fn mark_erased_withdraws_every_status_including_pending() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(granted(Purpose::TelemetryContent));
        s.upsert_consent(ConsentGrant {
            status: ConsentStatus::Pending,
            ..granted(Purpose::EvalRecording)
        });
        s.mark_erased(700);
        assert_eq!(s.consents.len(), 2, "both grants retained as audit proof");
        assert!(
            s.consents
                .iter()
                .all(|g| g.status == ConsentStatus::Withdrawn),
            "Granted AND Pending both flip to Withdrawn"
        );
        assert_eq!(s.updated_at, 700);
    }

    // mark_erased is idempotent: a second erasure keeps grants Withdrawn and just
    // re-stamps the timestamps (no residue accumulates, no state flips back).
    #[test]
    fn mark_erased_is_idempotent() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(granted(Purpose::TelemetryContent));
        s.mark_erased(100);
        s.mark_erased(200);
        assert_eq!(s.consents.len(), 1);
        assert_eq!(s.consents[0].status, ConsentStatus::Withdrawn);
        assert_eq!(s.erased_at, Some(200), "re-stamped to the latest erasure");
        assert_eq!(s.updated_at, 200);
    }

    // DataSubject::new default state: no grants, created_at == updated_at, and
    // neither the external id nor the erasure stamp is set.
    #[test]
    fn new_subject_defaults() {
        let s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 42);
        assert!(s.consents.is_empty());
        assert_eq!(s.created_at, 42);
        assert_eq!(s.updated_at, 42, "created == updated on a fresh subject");
        assert_eq!(s.erased_at, None);
        assert_eq!(s.external_id, None);
    }

    // Wire shapes: ConsentStatus / LawfulBasis are snake_case; LawfulBasis has a
    // Consent default and legitimate_interest is the multi-word case.
    #[test]
    fn consent_status_and_lawful_basis_wire_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&ConsentStatus::Withdrawn).unwrap(),
            "\"withdrawn\""
        );
        assert_eq!(
            serde_json::from_str::<ConsentStatus>("\"pending\"").unwrap(),
            ConsentStatus::Pending
        );
        assert_eq!(
            serde_json::to_string(&LawfulBasis::LegitimateInterest).unwrap(),
            "\"legitimate_interest\""
        );
        assert_eq!(LawfulBasis::default(), LawfulBasis::Consent);
    }

    // DataSubject serde: None external_id/erased_at are omitted from the wire, a
    // grant round-trips, and a grant JSON lacking `basis` defaults to Consent.
    #[test]
    fn data_subject_serde_omits_absent_optionals_and_defaults_basis() {
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(granted(Purpose::TelemetryContent));
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("external_id"), "None external_id omitted");
        assert!(!json.contains("erased_at"), "None erased_at omitted");
        // Full round-trip preserves the grant.
        let back: DataSubject = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);

        // A grant lacking `basis` deserializes to the Consent default.
        let grant: ConsentGrant = serde_json::from_str(
            r#"{"purpose":"telemetry_content","status":"granted","granted_at":1,"version":"v1"}"#,
        )
        .unwrap();
        assert_eq!(grant.basis, LawfulBasis::Consent);
    }

    // Error Display strings are the operator-facing wire text.
    #[test]
    fn data_subject_error_display_messages() {
        assert_eq!(
            DataSubjectError::NotFound("dsub_1".into()).to_string(),
            "data_subject not found: dsub_1"
        );
        assert_eq!(
            DataSubjectError::Storage("disk full".into()).to_string(),
            "storage: disk full"
        );
    }

    // Resolver erase with NO content erasers registered: still marks the subject
    // erased (retained + stamped) and returns a zero-count receipt.
    #[tokio::test]
    async fn resolver_erase_without_erasers_stamps_subject_and_returns_zero() {
        let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(granted(Purpose::TelemetryContent));
        repo.put(s).await.unwrap();

        let resolver = RepoDataSubjectResolver::new(repo.clone());
        let id = DataSubjectId("dsub_1".into());
        let receipt = resolver.erase(&id).await;
        assert_eq!(receipt.records_removed, 0, "no erasers → nothing removed");

        let after = repo.get(&id).await.expect("subject retained");
        assert!(after.erased_at.is_some(), "still stamped without erasers");
        assert_eq!(after.consents[0].status, ConsentStatus::Withdrawn);
    }

    // Resolver erase of a subject UNKNOWN to the repo still fans content removal
    // out to the erasers (fail-open on content) and does not error on the absent
    // subject record.
    #[tokio::test]
    async fn resolver_erase_unknown_subject_counts_eraser_removals() {
        use awaken_runtime_contract::ContentEraser;

        struct FakeEraser(usize);
        #[async_trait]
        impl ContentEraser for FakeEraser {
            async fn erase_subject(&self, _s: &DataSubjectId) -> usize {
                self.0
            }
        }

        let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
        let resolver =
            RepoDataSubjectResolver::new(repo).with_eraser(std::sync::Arc::new(FakeEraser(6)));
        // Subject was never put(); erasers still remove its orphaned content.
        let receipt = resolver.erase(&DataSubjectId("orphan".into())).await;
        assert_eq!(
            receipt.records_removed, 6,
            "content erased even with no record"
        );
    }

    #[tokio::test]
    async fn resolver_reads_grant_and_erases() {
        let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(granted(Purpose::TelemetryContent));
        repo.put(s).await.unwrap();

        let resolver = RepoDataSubjectResolver::new(repo);
        let id = DataSubjectId("dsub_1".into());
        assert_eq!(
            resolver
                .consent_ceiling(&id, Purpose::TelemetryContent)
                .await,
            ContentCapture::Full
        );
        // Unknown subject → Structured.
        assert_eq!(
            resolver
                .consent_ceiling(&DataSubjectId("nope".into()), Purpose::TelemetryContent)
                .await,
            ContentCapture::Structured
        );
        // Erase removes the record → resolves back to Structured.
        resolver.erase(&id).await;
        assert_eq!(
            resolver
                .consent_ceiling(&id, Purpose::TelemetryContent)
                .await,
            ContentCapture::Structured
        );
    }

    #[tokio::test]
    async fn erase_fans_out_and_sums_removed_counts() {
        use awaken_runtime_contract::ContentEraser;

        struct FakeEraser(usize);
        #[async_trait]
        impl ContentEraser for FakeEraser {
            async fn erase_subject(&self, _s: &DataSubjectId) -> usize {
                self.0
            }
        }

        let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
        repo.put(DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0))
            .await
            .unwrap();
        let resolver = RepoDataSubjectResolver::new(repo)
            .with_eraser(std::sync::Arc::new(FakeEraser(2)))
            .with_eraser(std::sync::Arc::new(FakeEraser(3)));

        let receipt = resolver.erase(&DataSubjectId("dsub_1".into())).await;
        assert_eq!(receipt.records_removed, 5, "sums across content stores");
    }

    // E4: the resolver's erase fans the removal out across every eraser (sum),
    // then RETAINS the subject record as accountability proof — stamping
    // `erased_at`, withdrawing its grants, and so ceiling any purpose to Structured.
    #[tokio::test]
    async fn resolver_erase_fans_out_and_stamps_retained_subject() {
        use awaken_runtime_contract::ContentEraser;

        struct FakeEraser(usize);
        #[async_trait]
        impl ContentEraser for FakeEraser {
            async fn erase_subject(&self, _s: &DataSubjectId) -> usize {
                self.0
            }
        }

        let repo = std::sync::Arc::new(InMemoryDataSubjectRepo::new());
        let mut s = DataSubject::new(DataSubjectId("dsub_1".into()), "org_1", 0);
        s.upsert_consent(granted(Purpose::TelemetryContent));
        repo.put(s).await.unwrap();

        let resolver = RepoDataSubjectResolver::new(repo.clone())
            .with_eraser(std::sync::Arc::new(FakeEraser(4)))
            .with_eraser(std::sync::Arc::new(FakeEraser(3)));
        let id = DataSubjectId("dsub_1".into());

        let receipt = resolver.erase(&id).await;
        assert_eq!(receipt.records_removed, 7, "fan-out sum across erasers");

        // Subject record is retained (Art. 5(2)/7(1)) with an erased_at stamp and
        // its grants withdrawn — not deleted.
        let after = repo.get(&id).await.expect("subject retained after erase");
        assert!(after.erased_at.is_some(), "erased_at stamped");
        assert_eq!(after.consents.len(), 1, "grant kept as audit proof");
        assert_eq!(after.consents[0].status, ConsentStatus::Withdrawn);

        // Any purpose now ceils to Structured (no live consent survives erasure).
        assert_eq!(
            resolver
                .consent_ceiling(&id, Purpose::TelemetryContent)
                .await,
            ContentCapture::Structured
        );
        assert_eq!(
            resolver.consent_ceiling(&id, Purpose::EvalRecording).await,
            ContentCapture::Structured
        );
    }
}
