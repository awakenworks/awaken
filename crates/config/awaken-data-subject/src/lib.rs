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
    /// when an active (`Granted`) grant exists, else `Structured`.
    #[must_use]
    pub fn consent_ceiling(&self, purpose: Purpose) -> ContentCapture {
        let granted = self
            .consents
            .iter()
            .any(|g| g.purpose == purpose && g.status == ConsentStatus::Granted);
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
}
