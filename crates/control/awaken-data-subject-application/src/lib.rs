//! The **data-subject application** (ADR-0050): the party a run/request is
//! attributed to, its per-purpose consent grants, and the resolver that answers
//! the consent ceiling and executes erasure.
//!
//! A control/compliance-plane application, Org-scoped: a subject belongs to
//! exactly one Org (the GDPR data controller). Storage is supplied through ports.
//! The neutral `DataSubjectId` / `Purpose` / `ContentCapture` / `ErasureReceipt`
//! vocabulary is reused from `awaken-runtime-contract`; the Anthropic
//! `UserProfile` wire shape is a *projection* over this aggregate (Slice 6).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

pub use awaken_runtime_contract::{ContentCapture, DataSubjectId, ErasureReceipt, Purpose};

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

const CAS_RETRY_LIMIT: usize = 16;

fn advance_revision(subject: &mut DataSubject) -> Result<u64, DataSubjectError> {
    let expected = subject.revision;
    subject.revision = next_revision(expected)
        .ok_or_else(|| DataSubjectError::RevisionExhausted(subject.id.0.clone()))?;
    Ok(expected)
}

#[must_use]
const fn next_revision(current: u64) -> Option<u64> {
    current.checked_add(1)
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

/// Finite consent decision kernel. Keeping the two facts separately makes the
/// withdrawal veto explicit: once observed, a later stale `Granted` duplicate
/// cannot reopen full-content capture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsentDecision {
    granted: bool,
    withdrawn: bool,
}

impl ConsentDecision {
    #[must_use]
    pub const fn observe(self, status: ConsentStatus) -> Self {
        match status {
            ConsentStatus::Granted => Self {
                granted: true,
                withdrawn: self.withdrawn,
            },
            ConsentStatus::Pending => self,
            ConsentStatus::Withdrawn => Self {
                granted: self.granted,
                withdrawn: true,
            },
        }
    }

    #[must_use]
    pub const fn ceiling(self) -> ContentCapture {
        if self.granted && !self.withdrawn {
            ContentCapture::Full
        } else {
            ContentCapture::Structured
        }
    }
}

#[must_use]
const fn retain_consent_for_upsert(existing_matches_incoming_purpose: bool) -> bool {
    !existing_matches_incoming_purpose
}

#[must_use]
const fn erased_consent_status(_current: ConsentStatus) -> ConsentStatus {
    ConsentStatus::Withdrawn
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

/// Protocol-neutral relationship between a subject and the controller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserProfileRelationship {
    #[default]
    External,
    Resold,
    Internal,
}

/// Protocol-neutral lifecycle of an authorization-style trust grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserProfileTrustGrantStatus {
    Active,
    Pending,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfileTrustGrant {
    pub status: UserProfileTrustGrantStatus,
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
    /// Opaque application metadata projected by protocol adapters.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub relationship: UserProfileRelationship,
    /// Authorization-style grants are deliberately separate from GDPR consent.
    #[serde(default)]
    pub trust_grants: BTreeMap<String, UserProfileTrustGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub consents: Vec<ConsentGrant>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Epoch-millis when this subject's content was erased (GDPR Art. 17). The
    /// subject record itself is **retained** as accountability proof (Art. 5(2)/
    /// 7(1)) — this stamps that erasure happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erased_at: Option<i64>,
    /// Optimistic-concurrency fence for every aggregate mutation.
    #[serde(default)]
    pub revision: u64,
}

impl DataSubject {
    /// A fresh subject with no consent grants.
    #[must_use]
    pub fn new(id: DataSubjectId, org: impl Into<String>, at: i64) -> Self {
        Self {
            id,
            org: org.into(),
            external_id: None,
            metadata: BTreeMap::new(),
            relationship: UserProfileRelationship::default(),
            trust_grants: BTreeMap::new(),
            name: None,
            consents: Vec::new(),
            created_at: at,
            updated_at: at,
            erased_at: None,
            revision: 0,
        }
    }

    /// Mark erasure (Art. 17): withdraw every consent grant (kept as audit proof,
    /// not deleted) and stamp `erased_at`. The subject record is retained for
    /// accountability; only the content it pointed to is deleted (by the erasers).
    pub fn mark_erased(&mut self, at: i64) {
        for grant in &mut self.consents {
            grant.status = erased_consent_status(grant.status);
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
        let mut decision = ConsentDecision::default();
        for g in &self.consents {
            if g.purpose != purpose {
                continue;
            }
            decision = decision.observe(g.status);
        }
        decision.ceiling()
    }

    /// Record `grant`, superseding any prior grant for the same purpose.
    pub fn upsert_consent(&mut self, grant: ConsentGrant) {
        self.consents
            .retain(|g| retain_consent_for_upsert(g.purpose == grant.purpose));
        self.consents.push(grant);
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn status(tag: u8) -> ConsentStatus {
        match tag % 3 {
            0 => ConsentStatus::Granted,
            1 => ConsentStatus::Pending,
            _ => ConsentStatus::Withdrawn,
        }
    }

    #[kani::proof]
    fn any_withdrawal_vetoes_full_content_capture() {
        let statuses = [
            status(kani::any()),
            status(kani::any()),
            status(kani::any()),
        ];
        let mut decision = ConsentDecision::default();
        for item in statuses {
            decision = decision.observe(item);
        }
        if statuses.contains(&ConsentStatus::Withdrawn) {
            assert_eq!(decision.ceiling(), ContentCapture::Structured);
        }
    }

    #[kani::proof]
    fn consent_upsert_leaves_exactly_one_row_for_the_incoming_purpose() {
        let existing_match = [kani::any::<bool>(), kani::any(), kani::any()];
        let retained_matches = existing_match
            .iter()
            .filter(|matches| retain_consent_for_upsert(**matches) && **matches)
            .count();
        assert_eq!(retained_matches + 1, 1);
    }

    #[kani::proof]
    fn erasure_withdrawal_is_absorbing_and_idempotent() {
        let first = erased_consent_status(status(kani::any()));
        let second = erased_consent_status(first);
        assert_eq!(first, ConsentStatus::Withdrawn);
        assert_eq!(second, ConsentStatus::Withdrawn);
    }

    #[kani::proof]
    fn revision_advance_is_strict_or_explicitly_exhausted() {
        let current = kani::any::<u64>();
        match next_revision(current) {
            Some(next) => assert!(next > current),
            None => assert_eq!(current, u64::MAX),
        }
    }
}

/// Errors from the data-subject store.
#[derive(Debug, thiserror::Error)]
pub enum DataSubjectError {
    #[error("data_subject not found: {0}")]
    NotFound(String),
    #[error("data_subject already exists: {0}")]
    AlreadyExists(String),
    #[error("data_subject revision conflict: {0}")]
    Conflict(String),
    #[error("data_subject revision exhausted: {0}")]
    RevisionExhausted(String),
    #[error("storage: {0}")]
    Storage(String),
}

/// Durable checkpoint for a multi-store erasure workflow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErasureProgress {
    #[serde(default)]
    pub revision: u64,
    pub completed_targets: Vec<ErasureTarget>,
    pub records_removed: usize,
    pub accountability_stamped: bool,
    pub complete: bool,
}

impl ErasureProgress {
    /// Repository invariant shared by every adapter: an absent checkpoint starts
    /// at zero; an existing checkpoint advances by exactly one.
    #[must_use]
    pub fn follows(&self, expected_revision: Option<u64>) -> bool {
        match expected_revision {
            None => self.revision == 0,
            Some(expected) => expected.checked_add(1) == Some(self.revision),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasureTarget {
    Coordinator,
    Resources,
}

/// Persistence port for the [`DataSubject`] aggregate (repository-per-aggregate).
#[async_trait]
pub trait DataSubjectRepo: Send + Sync {
    /// Insert a fresh aggregate. Existing ids are reported, never overwritten.
    async fn create(&self, subject: DataSubject) -> Result<(), DataSubjectError>;
    /// Replace exactly one expected revision. A stale writer receives Conflict.
    async fn compare_and_swap(
        &self,
        expected_revision: u64,
        subject: DataSubject,
    ) -> Result<(), DataSubjectError>;
    /// Fetch a subject by id.
    async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError>;
    /// List an Org's subjects, in insertion order.
    async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError>;
}

/// Control process-manager persistence, separate from the subject aggregate.
#[async_trait]
pub trait ErasureJobRepo: Send + Sync {
    async fn load(&self, id: &DataSubjectId) -> Result<Option<ErasureProgress>, DataSubjectError>;
    /// Create or replace exactly the checkpoint revision observed by the caller.
    /// `None` means the row must still be absent; stale writers receive Conflict.
    async fn compare_and_swap_progress(
        &self,
        id: &DataSubjectId,
        expected_revision: Option<u64>,
        progress: &ErasureProgress,
    ) -> Result<(), DataSubjectError>;
}

/// Clock port used to make expiry and aggregate timestamps deterministic in tests.
pub trait DataSubjectClock: Send + Sync {
    fn now_millis(&self) -> i64;
}

#[derive(Debug, Default)]
pub struct SystemDataSubjectClock;

impl DataSubjectClock for SystemDataSubjectClock {
    fn now_millis(&self) -> i64 {
        now_millis()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateUserProfileCommand {
    pub org: String,
    pub metadata: BTreeMap<String, String>,
    pub relationship: UserProfileRelationship,
    pub external_id: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateUserProfileCommand {
    pub metadata: Option<BTreeMap<String, String>>,
    pub relationship: Option<UserProfileRelationship>,
    pub trust_grants: Option<BTreeMap<String, UserProfileTrustGrant>>,
    pub external_id: Option<UserProfileFieldUpdate<String>>,
    pub name: Option<UserProfileFieldUpdate<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserProfileFieldUpdate<T> {
    Clear,
    Replace(T),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserProfileRecord {
    pub id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub metadata: BTreeMap<String, String>,
    pub relationship: UserProfileRelationship,
    pub trust_grants: BTreeMap<String, UserProfileTrustGrant>,
    pub external_id: Option<String>,
    pub name: Option<String>,
    pub revision: u64,
}

impl From<&DataSubject> for UserProfileRecord {
    fn from(subject: &DataSubject) -> Self {
        Self {
            id: subject.id.0.clone(),
            created_at: subject.created_at,
            updated_at: subject.updated_at,
            metadata: subject.metadata.clone(),
            relationship: subject.relationship,
            trust_grants: subject.trust_grants.clone(),
            external_id: subject.external_id.clone(),
            name: subject.name.clone(),
            revision: subject.revision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRecord {
    pub id: String,
    pub grants: Vec<ConsentGrant>,
    pub telemetry_content_ceiling: ContentCapture,
}

impl From<&DataSubject> for ConsentRecord {
    fn from(subject: &DataSubject) -> Self {
        Self {
            id: subject.id.0.clone(),
            grants: subject.consents.clone(),
            telemetry_content_ceiling: subject.consent_ceiling(Purpose::TelemetryContent),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureDecisionRecord {
    pub requested: ContentCapture,
    pub ceiling: ContentCapture,
    pub consent: ContentCapture,
    pub effective: ContentCapture,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentTicket {
    pub url: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentRequest {
    pub subject_id: String,
    pub purposes: Vec<Purpose>,
    pub expires_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum DataSubjectApplicationError {
    #[error("user_profile not found")]
    NotFound,
    #[error("data_subject conflict retry budget exhausted")]
    Conflict,
    #[error("invalid enrollment token")]
    InvalidEnrollment,
    #[error("enrollment signing key must be at least 32 bytes")]
    WeakEnrollmentKey,
    #[error("data-subject erasure failed: {0}")]
    Erasure(String),
    #[error(transparent)]
    Repository(#[from] DataSubjectError),
}

type HmacSha256 = Hmac<Sha256>;

/// Domain-separated enrollment key derived from the Control process root key.
/// The root key itself is never retained by this application.
pub fn derive_enrollment_signing_key(root_key: &[u8; 32]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(root_key).expect("HMAC accepts a 32-byte key");
    mac.update(b"awaken:data-subject:enrollment:v1");
    mac.finalize().into_bytes().into()
}

#[derive(Debug, Serialize, Deserialize)]
struct EnrollmentTokenPayload {
    subject_id: String,
    org: String,
    purposes: Vec<Purpose>,
    expires_at: i64,
}

pub struct DataSubjectApplication {
    repo: Arc<dyn DataSubjectRepo>,
    clock: Arc<dyn DataSubjectClock>,
    enrollment_key: Vec<u8>,
}

impl DataSubjectApplication {
    pub fn new(
        repo: Arc<dyn DataSubjectRepo>,
        enrollment_key: Vec<u8>,
    ) -> Result<Self, DataSubjectApplicationError> {
        Self::with_clock(repo, enrollment_key, Arc::new(SystemDataSubjectClock))
    }

    pub fn with_clock(
        repo: Arc<dyn DataSubjectRepo>,
        enrollment_key: Vec<u8>,
        clock: Arc<dyn DataSubjectClock>,
    ) -> Result<Self, DataSubjectApplicationError> {
        if enrollment_key.len() < 32 {
            return Err(DataSubjectApplicationError::WeakEnrollmentKey);
        }
        Ok(Self {
            repo,
            clock,
            enrollment_key,
        })
    }

    pub async fn create_user_profile(
        &self,
        command: CreateUserProfileCommand,
    ) -> Result<UserProfileRecord, DataSubjectApplicationError> {
        let now = self.clock.now_millis();
        for _ in 0..CAS_RETRY_LIMIT {
            let id = DataSubjectId(format!("uprof_{}", uuid::Uuid::new_v4().simple()));
            let mut subject = DataSubject::new(id, command.org.clone(), now);
            subject.metadata = command.metadata.clone();
            subject.relationship = command.relationship;
            subject.external_id = command.external_id.clone();
            subject.name = command.name.clone();
            match self.repo.create(subject.clone()).await {
                Ok(()) => return Ok(UserProfileRecord::from(&subject)),
                Err(DataSubjectError::AlreadyExists(_)) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(DataSubjectApplicationError::Conflict)
    }

    pub async fn get_user_profile(
        &self,
        org: &str,
        id: &str,
    ) -> Result<UserProfileRecord, DataSubjectApplicationError> {
        self.load_scoped(org, id)
            .await
            .map(|subject| UserProfileRecord::from(&subject))
    }

    pub async fn list_user_profiles(
        &self,
        org: &str,
    ) -> Result<Vec<UserProfileRecord>, DataSubjectApplicationError> {
        let mut subjects = self.repo.list(org).await?;
        subjects.sort_by(|left, right| {
            (left.created_at, left.id.0.as_str()).cmp(&(right.created_at, right.id.0.as_str()))
        });
        Ok(subjects.iter().map(UserProfileRecord::from).collect())
    }

    pub async fn update_user_profile(
        &self,
        org: &str,
        id: &str,
        command: UpdateUserProfileCommand,
    ) -> Result<UserProfileRecord, DataSubjectApplicationError> {
        self.mutate_scoped(org, id, move |subject| {
            let mut changed = false;
            if let Some(external_id) = &command.external_id {
                let external_id = match external_id {
                    UserProfileFieldUpdate::Clear => None,
                    UserProfileFieldUpdate::Replace(value) => Some(value.clone()),
                };
                changed |= subject.external_id != external_id;
                subject.external_id = external_id;
            }
            if let Some(name) = &command.name {
                let name = match name {
                    UserProfileFieldUpdate::Clear => None,
                    UserProfileFieldUpdate::Replace(value) => Some(value.clone()),
                };
                changed |= subject.name != name;
                subject.name = name;
            }
            if let Some(relationship) = command.relationship {
                changed |= subject.relationship != relationship;
                subject.relationship = relationship;
            }
            if let Some(patch) = &command.metadata {
                for (key, value) in patch {
                    if value.is_empty() {
                        changed |= subject.metadata.remove(key).is_some();
                    } else if subject.metadata.get(key) != Some(value) {
                        subject.metadata.insert(key.clone(), value.clone());
                        changed = true;
                    }
                }
            }
            if let Some(patch) = &command.trust_grants {
                for (key, value) in patch {
                    if subject.trust_grants.get(key) != Some(value) {
                        subject.trust_grants.insert(key.clone(), value.clone());
                        changed = true;
                    }
                }
            }
            changed
        })
        .await
        .map(|subject| UserProfileRecord::from(&subject))
    }

    pub async fn grant_consent(
        &self,
        org: &str,
        id: &str,
        grant: ConsentGrant,
    ) -> Result<ConsentRecord, DataSubjectApplicationError> {
        let subject = self
            .mutate_or_create(org, id, move |subject| {
                if subject.consents.iter().any(|existing| existing == &grant) {
                    return false;
                }
                subject.upsert_consent(grant.clone());
                true
            })
            .await?;
        Ok(ConsentRecord::from(&subject))
    }

    pub async fn grant_consent_for_purpose(
        &self,
        org: &str,
        id: &str,
        purpose: Purpose,
        version: String,
    ) -> Result<ConsentRecord, DataSubjectApplicationError> {
        self.grant_consent(
            org,
            id,
            ConsentGrant {
                purpose,
                status: ConsentStatus::Granted,
                basis: LawfulBasis::Consent,
                granted_at: self.clock.now_millis(),
                version,
            },
        )
        .await
    }

    pub async fn read_consent(
        &self,
        org: &str,
        id: &str,
    ) -> Result<ConsentRecord, DataSubjectApplicationError> {
        let subject = self.load_scoped(org, id).await?;
        Ok(ConsentRecord::from(&subject))
    }

    pub async fn capture_decision(
        &self,
        org: &str,
        id: &str,
        requested: ContentCapture,
        ceiling: ContentCapture,
    ) -> Result<CaptureDecisionRecord, DataSubjectApplicationError> {
        let consent = match self.load_scoped(org, id).await {
            Ok(subject) => subject.consent_ceiling(Purpose::TelemetryContent),
            Err(DataSubjectApplicationError::NotFound) => ContentCapture::Structured,
            Err(error) => return Err(error),
        };
        let effective = ceiling.meet(requested).meet(consent);
        let reason = if effective == requested {
            "ok"
        } else if effective == consent && consent < ceiling {
            "no_consent"
        } else {
            "clamped_by_ceiling"
        };
        Ok(CaptureDecisionRecord {
            requested,
            ceiling,
            consent,
            effective,
            reason,
        })
    }

    pub async fn erase_user_profile(
        &self,
        org: &str,
        id: &str,
        resolver: &dyn awaken_runtime_contract::DataSubjectResolver,
    ) -> Result<ErasureReceipt, DataSubjectApplicationError> {
        self.load_scoped(org, id).await?;
        resolver
            .erase(&DataSubjectId(id.to_string()))
            .await
            .map_err(|error| DataSubjectApplicationError::Erasure(error.to_string()))
    }

    pub async fn mint_enrollment(
        &self,
        org: &str,
        id: &str,
    ) -> Result<EnrollmentTicket, DataSubjectApplicationError> {
        self.mint_enrollment_for_purposes(
            org,
            id,
            vec![Purpose::TelemetryContent, Purpose::EvalRecording],
        )
        .await
    }

    pub async fn mint_enrollment_for_purposes(
        &self,
        org: &str,
        id: &str,
        purposes: Vec<Purpose>,
    ) -> Result<EnrollmentTicket, DataSubjectApplicationError> {
        if purposes.is_empty() {
            return Err(DataSubjectApplicationError::InvalidEnrollment);
        }
        self.load_scoped(org, id).await?;
        let expires_at = self
            .clock
            .now_millis()
            .checked_add(15 * 60 * 1000)
            .ok_or(DataSubjectApplicationError::InvalidEnrollment)?;
        let payload = EnrollmentTokenPayload {
            subject_id: id.to_string(),
            org: org.to_string(),
            purposes,
            expires_at,
        };
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&payload).map_err(|error| {
                DataSubjectApplicationError::Repository(DataSubjectError::Storage(
                    error.to_string(),
                ))
            })?,
        );
        let signature = self.sign(payload.as_bytes());
        Ok(EnrollmentTicket {
            url: format!("/enroll/{payload}.{signature}"),
            expires_at,
        })
    }

    pub fn inspect_enrollment(
        &self,
        token: &str,
    ) -> Result<EnrollmentRequest, DataSubjectApplicationError> {
        let payload = self.verify_enrollment(token)?;
        Ok(EnrollmentRequest {
            subject_id: payload.subject_id,
            purposes: payload.purposes,
            expires_at: payload.expires_at,
        })
    }

    pub async fn accept_enrollment(
        &self,
        token: &str,
    ) -> Result<ConsentRecord, DataSubjectApplicationError> {
        let payload = self.verify_enrollment(token)?;
        let now = self.clock.now_millis();
        let purposes = payload.purposes;
        let subject = self
            .mutate_scoped(&payload.org, &payload.subject_id, move |subject| {
                let mut changed = false;
                for purpose in &purposes {
                    let grant = ConsentGrant {
                        purpose: *purpose,
                        status: ConsentStatus::Granted,
                        basis: LawfulBasis::Consent,
                        granted_at: now,
                        version: "enrollment".to_string(),
                    };
                    if subject.consents.iter().any(|existing| {
                        existing.purpose == grant.purpose
                            && existing.status == ConsentStatus::Granted
                            && existing.basis == LawfulBasis::Consent
                            && existing.version == "enrollment"
                    }) {
                        continue;
                    }
                    subject.upsert_consent(grant);
                    changed = true;
                }
                changed
            })
            .await?;
        Ok(ConsentRecord::from(&subject))
    }

    async fn load_scoped(
        &self,
        org: &str,
        id: &str,
    ) -> Result<DataSubject, DataSubjectApplicationError> {
        match self.repo.get(&DataSubjectId(id.to_string())).await {
            Ok(subject) if subject.org == org => Ok(subject),
            Ok(_) | Err(DataSubjectError::NotFound(_)) => {
                Err(DataSubjectApplicationError::NotFound)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn mutate_scoped<F>(
        &self,
        org: &str,
        id: &str,
        mut apply: F,
    ) -> Result<DataSubject, DataSubjectApplicationError>
    where
        F: FnMut(&mut DataSubject) -> bool,
    {
        for _ in 0..CAS_RETRY_LIMIT {
            let mut subject = self.load_scoped(org, id).await?;
            if !apply(&mut subject) {
                return Ok(subject);
            }
            subject.updated_at = subject.updated_at.max(self.clock.now_millis());
            let expected = advance_revision(&mut subject)?;
            match self.repo.compare_and_swap(expected, subject.clone()).await {
                Ok(()) => return Ok(subject),
                Err(DataSubjectError::Conflict(_)) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(DataSubjectApplicationError::Conflict)
    }

    async fn mutate_or_create<F>(
        &self,
        org: &str,
        id: &str,
        mut apply: F,
    ) -> Result<DataSubject, DataSubjectApplicationError>
    where
        F: FnMut(&mut DataSubject) -> bool,
    {
        for _ in 0..CAS_RETRY_LIMIT {
            match self.repo.get(&DataSubjectId(id.to_string())).await {
                Ok(mut subject) if subject.org == org => {
                    if !apply(&mut subject) {
                        return Ok(subject);
                    }
                    subject.updated_at = subject.updated_at.max(self.clock.now_millis());
                    let expected = advance_revision(&mut subject)?;
                    match self.repo.compare_and_swap(expected, subject.clone()).await {
                        Ok(()) => return Ok(subject),
                        Err(DataSubjectError::Conflict(_)) => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(_) => return Err(DataSubjectApplicationError::NotFound),
                Err(DataSubjectError::NotFound(_)) => {
                    let now = self.clock.now_millis();
                    let mut subject =
                        DataSubject::new(DataSubjectId(id.to_string()), org.to_string(), now);
                    apply(&mut subject);
                    match self.repo.create(subject.clone()).await {
                        Ok(()) => return Ok(subject),
                        Err(DataSubjectError::AlreadyExists(_)) => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(DataSubjectApplicationError::Conflict)
    }

    fn sign(&self, payload: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.enrollment_key)
            .expect("HMAC accepts every non-empty key length");
        mac.update(payload);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }

    fn verify_enrollment(
        &self,
        token: &str,
    ) -> Result<EnrollmentTokenPayload, DataSubjectApplicationError> {
        let (payload, signature) = token
            .split_once('.')
            .ok_or(DataSubjectApplicationError::InvalidEnrollment)?;
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| DataSubjectApplicationError::InvalidEnrollment)?;
        let mut mac = HmacSha256::new_from_slice(&self.enrollment_key)
            .expect("HMAC accepts every non-empty key length");
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| DataSubjectApplicationError::InvalidEnrollment)?;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| DataSubjectApplicationError::InvalidEnrollment)?;
        let payload: EnrollmentTokenPayload = serde_json::from_slice(&bytes)
            .map_err(|_| DataSubjectApplicationError::InvalidEnrollment)?;
        if payload.expires_at < self.clock.now_millis() || payload.purposes.is_empty() {
            return Err(DataSubjectApplicationError::InvalidEnrollment);
        }
        Ok(payload)
    }
}

/// Organization-level privacy process manager over the one subject repository
/// and the one subject erasure resolver.
pub struct RepoOrganizationPrivacyResolver {
    repo: Arc<dyn DataSubjectRepo>,
    subjects: Arc<dyn awaken_runtime_contract::DataSubjectResolver>,
}

impl RepoOrganizationPrivacyResolver {
    #[must_use]
    pub fn new(
        repo: Arc<dyn DataSubjectRepo>,
        subjects: Arc<dyn awaken_runtime_contract::DataSubjectResolver>,
    ) -> Self {
        Self { repo, subjects }
    }

    async fn selected_subjects(
        &self,
        organization_id: &str,
        subject: Option<&DataSubjectId>,
    ) -> Result<Vec<DataSubject>, awaken_runtime_contract::PrivacyExportError> {
        match subject {
            Some(subject) => match self.repo.get(subject).await {
                Ok(record) if record.org == organization_id => Ok(vec![record]),
                Ok(_) | Err(DataSubjectError::NotFound(_)) => {
                    Err(awaken_runtime_contract::PrivacyExportError(
                        "data subject is not owned by the requested organization".into(),
                    ))
                }
                Err(error) => Err(awaken_runtime_contract::PrivacyExportError(
                    error.to_string(),
                )),
            },
            None => self
                .repo
                .list(organization_id)
                .await
                .map_err(|error| awaken_runtime_contract::PrivacyExportError(error.to_string())),
        }
    }
}

#[async_trait]
impl awaken_runtime_contract::OrganizationPrivacyResolver for RepoOrganizationPrivacyResolver {
    async fn erase_organization(
        &self,
        organization_id: &str,
    ) -> Result<ErasureReceipt, awaken_runtime_contract::ErasureError> {
        let mut subjects = self.repo.list(organization_id).await.map_err(|error| {
            awaken_runtime_contract::ErasureError(format!(
                "organization subject inventory failed: {error}"
            ))
        })?;
        subjects.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        let mut records_removed = 0usize;
        for subject in subjects {
            records_removed = records_removed
                .checked_add(self.subjects.erase(&subject.id).await?.records_removed)
                .ok_or_else(|| {
                    awaken_runtime_contract::ErasureError(
                        "organization erasure receipt overflow".into(),
                    )
                })?;
        }
        Ok(ErasureReceipt { records_removed })
    }

    async fn export_organization(
        &self,
        organization_id: &str,
        subject: Option<&DataSubjectId>,
    ) -> Result<
        awaken_runtime_contract::OrganizationPrivacyExport,
        awaken_runtime_contract::PrivacyExportError,
    > {
        let mut subjects = self.selected_subjects(organization_id, subject).await?;
        subjects.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        let records = subjects
            .into_iter()
            .map(|subject| {
                let id = subject.id.clone();
                serde_json::to_value(subject)
                    .map(
                        |payload| awaken_runtime_contract::OrganizationPrivacyRecord {
                            subject: id,
                            payload,
                        },
                    )
                    .map_err(|error| awaken_runtime_contract::PrivacyExportError(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(awaken_runtime_contract::OrganizationPrivacyExport { records })
    }
}

/// Bridges the neutral [`DataSubjectResolver`](awaken_runtime_contract::DataSubjectResolver)
/// port to a [`DataSubjectRepo`] (ADR-0050 D10a). An unknown subject resolves to
/// `Structured` (no content without a known, consenting subject). Holds an
/// `Arc`-shared repo so a consent-write path and the resolver see one store.
pub struct RepoDataSubjectResolver {
    repo: Arc<dyn DataSubjectRepo>,
    jobs: Arc<dyn ErasureJobRepo>,
    coordinator: Option<Arc<dyn awaken_runtime_contract::ContentEraser>>,
    resources: Option<Arc<dyn awaken_runtime_contract::ContentEraser>>,
}

impl RepoDataSubjectResolver {
    #[must_use]
    pub fn new(repo: Arc<dyn DataSubjectRepo>, jobs: Arc<dyn ErasureJobRepo>) -> Self {
        Self {
            repo,
            jobs,
            coordinator: None,
            resources: None,
        }
    }

    /// Bind the one application eraser for a stable bounded-context target
    /// (GDPR Art. 17). Rebinding a target replaces its adapter; it never creates
    /// an order-dependent parallel checkpoint slot.
    #[must_use]
    pub fn with_target(
        mut self,
        target: ErasureTarget,
        eraser: Arc<dyn awaken_runtime_contract::ContentEraser>,
    ) -> Self {
        match target {
            ErasureTarget::Coordinator => self.coordinator = Some(eraser),
            ErasureTarget::Resources => self.resources = Some(eraser),
        }
        self
    }

    async fn stamp_accountability(
        &self,
        subject: &DataSubjectId,
    ) -> Result<(), awaken_runtime_contract::ErasureError> {
        for _ in 0..CAS_RETRY_LIMIT {
            let mut aggregate = match self.repo.get(subject).await {
                Ok(aggregate) => aggregate,
                Err(DataSubjectError::NotFound(_)) => return Ok(()),
                Err(error) => {
                    return Err(awaken_runtime_contract::ErasureError(format!(
                        "accountability read failed: {error}"
                    )));
                }
            };
            if aggregate.erased_at.is_some()
                && aggregate
                    .consents
                    .iter()
                    .all(|grant| grant.status == ConsentStatus::Withdrawn)
            {
                return Ok(());
            }
            aggregate.mark_erased(now_millis());
            let expected = advance_revision(&mut aggregate).map_err(|error| {
                awaken_runtime_contract::ErasureError(format!(
                    "accountability write failed: {error}"
                ))
            })?;
            match self.repo.compare_and_swap(expected, aggregate).await {
                Ok(()) => return Ok(()),
                Err(DataSubjectError::Conflict(_)) => continue,
                Err(error) => {
                    return Err(awaken_runtime_contract::ErasureError(format!(
                        "accountability write failed: {error}"
                    )));
                }
            }
        }
        Err(awaken_runtime_contract::ErasureError(
            "accountability write failed: conflict retry budget exhausted".into(),
        ))
    }
}

fn advance_erasure_progress(
    progress: &mut ErasureProgress,
    expected_revision: Option<u64>,
) -> Result<(), awaken_runtime_contract::ErasureError> {
    progress.revision = match expected_revision {
        None => 0,
        Some(expected) => expected.checked_add(1).ok_or_else(|| {
            awaken_runtime_contract::ErasureError("erasure checkpoint revision exhausted".into())
        })?,
    };
    Ok(())
}

#[async_trait]
impl awaken_runtime_contract::DataSubjectConsentSource for RepoDataSubjectResolver {
    async fn consent_ceiling(&self, subject: &DataSubjectId, purpose: Purpose) -> ContentCapture {
        match self.repo.get(subject).await {
            Ok(s) => s.consent_ceiling(purpose),
            Err(_) => ContentCapture::Structured,
        }
    }
}

#[async_trait]
impl awaken_runtime_contract::DataSubjectResolver for RepoDataSubjectResolver {
    async fn erase(
        &self,
        subject: &DataSubjectId,
    ) -> Result<ErasureReceipt, awaken_runtime_contract::ErasureError> {
        // Cause ordering is durable and common to every caller: the next target
        // effect, accountability stamp, then terminal receipt. Multiple replicas
        // may replay an idempotent target effect, but only one revision-CAS may
        // count it; losers reload the winning checkpoint and continue.
        for _ in 0..CAS_RETRY_LIMIT {
            let loaded = self
                .jobs
                .load(subject)
                .await
                .map_err(|error| awaken_runtime_contract::ErasureError(error.to_string()))?;
            let expected_revision = loaded.as_ref().map(|progress| progress.revision);
            let mut progress = loaded.unwrap_or_default();
            if progress.complete {
                return Ok(ErasureReceipt {
                    records_removed: progress.records_removed,
                });
            }

            let pending_target = [
                (ErasureTarget::Coordinator, self.coordinator.as_ref()),
                (ErasureTarget::Resources, self.resources.as_ref()),
            ]
            .into_iter()
            .find(|(target, eraser)| {
                eraser.is_some() && !progress.completed_targets.contains(target)
            });
            if let Some((target, Some(eraser))) = pending_target {
                progress.records_removed = progress
                    .records_removed
                    .checked_add(eraser.erase_subject(subject).await?)
                    .ok_or_else(|| {
                        awaken_runtime_contract::ErasureError("erasure receipt overflow".into())
                    })?;
                progress.completed_targets.push(target);
                advance_erasure_progress(&mut progress, expected_revision)?;
                match self
                    .jobs
                    .compare_and_swap_progress(subject, expected_revision, &progress)
                    .await
                {
                    Ok(()) | Err(DataSubjectError::Conflict(_)) => continue,
                    Err(error) => {
                        return Err(awaken_runtime_contract::ErasureError(error.to_string()));
                    }
                }
            }

            if !progress.accountability_stamped {
                self.stamp_accountability(subject).await?;
                progress.accountability_stamped = true;
                advance_erasure_progress(&mut progress, expected_revision)?;
                match self
                    .jobs
                    .compare_and_swap_progress(subject, expected_revision, &progress)
                    .await
                {
                    Ok(()) | Err(DataSubjectError::Conflict(_)) => continue,
                    Err(error) => {
                        return Err(awaken_runtime_contract::ErasureError(error.to_string()));
                    }
                }
            }

            progress.complete = true;
            advance_erasure_progress(&mut progress, expected_revision)?;
            match self
                .jobs
                .compare_and_swap_progress(subject, expected_revision, &progress)
                .await
            {
                Ok(()) => {
                    return Ok(ErasureReceipt {
                        records_removed: progress.records_removed,
                    });
                }
                Err(DataSubjectError::Conflict(_)) => continue,
                Err(error) => {
                    return Err(awaken_runtime_contract::ErasureError(error.to_string()));
                }
            }
        }
        Err(awaken_runtime_contract::ErasureError(
            "erasure checkpoint conflict retry budget exhausted".into(),
        ))
    }
}

#[cfg(test)]
mod tests;
