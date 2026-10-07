// @formal entrypoint consent.any_withdrawal_vetoes_full_capture
// @formal entrypoint consent.erasure_withdrawal_is_absorbing
// @formal entrypoint consent.upsert_retains_one_purpose_row
// @formal entrypoint data_subject.revision_is_monotonic
// @formal entrypoint data_subject.revision_is_strict_or_exhausted
// @formal entrypoint data_subject.stale_writer_never_applies
// @formal entrypoint erasure.accountability_stamp_follows_all_targets
// @formal entrypoint erasure.checkpoint_never_forgets_completed_targets
// @formal entrypoint erasure.completion_requires_accountability_stamp
// @formal entrypoint erasure.concurrent_replicas_have_one_checkpoint_winner
// @formal entrypoint erasure.each_target_is_counted_once
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

use async_trait::async_trait;
use awaken_time::WallClock;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

pub use awaken_data_governance_contract::ErasureReceipt;
pub use awaken_runtime_contract::{ContentCapture, DataSubjectId, Purpose};

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

/// Protocol-neutral access model introduced by the current User Profiles wire.
/// It remains optional so legacy relationship-only profiles retain their exact
/// authored provenance instead of being silently rewritten on read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserProfileAccessType {
    Application,
    Passthrough,
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
    /// When the platform reports that the represented user completed onboarding.
    /// This belongs to the existing DataSubject aggregate so protocol projections
    /// cannot create a second profile-shaped source of truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_user_onboarded_at: Option<i64>,
    /// Platform-stated facts about the entity (`user-profiles-2026-09-04`),
    /// recorded as given and never verified here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_user_details: Option<ExternalUserDetails>,
    /// Opaque application metadata projected by protocol adapters.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub relationship: UserProfileRelationship,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_type: Option<UserProfileAccessType>,
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
            external_user_onboarded_at: None,
            external_user_details: None,
            metadata: BTreeMap::new(),
            relationship: UserProfileRelationship::default(),
            access_type: None,
            trust_grants: BTreeMap::new(),
            name: None,
            consents: Vec::new(),
            created_at: at,
            updated_at: at,
            erased_at: None,
            revision: 0,
        }
    }

    /// Whether this aggregate is the exact non-wrapping successor of the
    /// revision observed by a compare-and-swap caller.
    #[must_use]
    pub fn is_successor_of(&self, expected_revision: u64) -> bool {
        expected_revision.checked_add(1) == Some(self.revision)
    }

    /// The sole revision admission rule for replacing an existing subject row.
    /// Adapters supply the revision read under their lock or transaction; the
    /// row key is always this aggregate's `id`.
    #[must_use]
    pub fn follows_revision(&self, expected_revision: u64, current_revision: u64) -> bool {
        current_revision == expected_revision && self.is_successor_of(expected_revision)
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

    /// @obligation consent.any_withdrawal_vetoes_full_capture
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

    /// @obligation consent.upsert_retains_one_purpose_row
    #[kani::proof]
    fn consent_upsert_leaves_exactly_one_row_for_the_incoming_purpose() {
        let existing_match = [kani::any::<bool>(), kani::any(), kani::any()];
        let retained_matches = existing_match
            .iter()
            .filter(|matches| retain_consent_for_upsert(**matches) && **matches)
            .count();
        assert_eq!(retained_matches + 1, 1);
    }

    /// @obligation consent.erasure_withdrawal_is_absorbing
    #[kani::proof]
    fn erasure_withdrawal_is_absorbing_and_idempotent() {
        let first = erased_consent_status(status(kani::any()));
        let second = erased_consent_status(first);
        assert_eq!(first, ConsentStatus::Withdrawn);
        assert_eq!(second, ConsentStatus::Withdrawn);
    }

    /// @obligation data_subject.revision_is_monotonic
    /// @obligation data_subject.revision_is_strict_or_exhausted
    #[kani::proof]
    fn revision_advance_is_strict_or_explicitly_exhausted() {
        // Cause/effect decision table: C1 current={0..u64::MAX-1,u64::MAX};
        // E1 a representable successor is exactly current+1 and strictly newer;
        // E2 exhaustion returns None and cannot create a wrapped or saturated
        // write candidate. R1=C1(non-max)->E1; R2=C1(max)->E2. Arbitrary u64
        // selection covers both rules and the complete production scalar domain.
        let current = kani::any::<u64>();
        match next_revision(current) {
            Some(next) => assert!(next > current),
            None => assert_eq!(current, u64::MAX),
        }
    }

    /// @obligation data_subject.stale_writer_never_applies
    #[kani::proof]
    fn stale_data_subject_writer_is_never_admitted() {
        // Cause/effect decision table: C1 stored revision equals/differs from
        // the caller expectation; C2 candidate is exact successor/equal/jump/
        // wrapped. E1 admission occurs exactly for C1=equal and C2=successor;
        // E2 every stale, future, equal, skipped or exhausted candidate is
        // rejected. Arbitrary u64 values exhaust both causes without assumes.
        let expected_revision = kani::any::<u64>();
        let current_revision = kani::any::<u64>();
        let candidate_revision = kani::any::<u64>();
        let mut candidate = DataSubject::new(DataSubjectId(String::new()), String::new(), 0);
        candidate.revision = candidate_revision;

        let admitted = candidate.follows_revision(expected_revision, current_revision);
        let exact_successor = expected_revision
            .checked_add(1)
            .is_some_and(|next| next == candidate_revision);
        assert_eq!(
            admitted,
            current_revision == expected_revision && exact_successor
        );
        if current_revision != expected_revision {
            assert!(!admitted);
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

// A transient projection of the one checkpoint, never a second stored state.
#[derive(Debug, PartialEq, Eq)]
enum ErasureAction<T> {
    InvalidCheckpoint,
    Replay,
    Erase(ErasureTarget, T),
    Stamp,
    Complete,
}

impl ErasureProgress {
    // Validate the loaded shape before replay or any effect. The checkpoint has
    // no historical adapter inventory: adding a bound target can therefore make
    // a prior terminal checkpoint inconsistent. Reject it without reopening or
    // repairing it, rather than returning a receipt that omits the new target.
    fn next_action<T>(&self, targets: [(ErasureTarget, Option<T>); 2]) -> ErasureAction<T> {
        let pending = self.pending_target(targets);
        if (self.complete && !self.accountability_stamped)
            || (self.accountability_stamped && pending.is_some())
        {
            return ErasureAction::InvalidCheckpoint;
        }
        if self.complete {
            return ErasureAction::Replay;
        }
        if let Some((target, Some(eraser))) = pending {
            return ErasureAction::Erase(target, eraser);
        }
        if self.accountability_stamped {
            ErasureAction::Complete
        } else {
            ErasureAction::Stamp
        }
    }

    // The resolver supplies its two stable target/adapter slots in order. Keep
    // selection here with the checkpoint it observes; adapter calls remain in
    // the resolver and an absent adapter never becomes a pending effect.
    fn pending_target<T>(
        &self,
        targets: [(ErasureTarget, Option<T>); 2],
    ) -> Option<(ErasureTarget, Option<T>)> {
        targets
            .into_iter()
            .find(|(target, eraser)| eraser.is_some() && !self.completed_targets.contains(target))
    }

    // Called only after the selected target's effect succeeds. Overflow leaves
    // the complete checkpoint unchanged; publication still uses the original CAS.
    fn record_target(
        &mut self,
        target: ErasureTarget,
        removed: usize,
    ) -> Result<(), awaken_data_governance_contract::ErasureError> {
        self.records_removed = self.records_removed.checked_add(removed).ok_or_else(|| {
            awaken_data_governance_contract::ErasureError("erasure receipt overflow".into())
        })?;
        self.completed_targets.push(target);
        Ok(())
    }

    /// The sole checkpoint successor relation, evaluated against the row read
    /// under the adapter's lock/transaction. Caller revision and stored revision
    /// must agree; creation starts at zero and updates advance exactly once.
    /// Completed targets are a set: ordering and duplicate occurrences do not
    /// matter, but no old target may disappear. Counts and terminal flags are
    /// deliberately outside this relation and retain their existing owners.
    #[must_use]
    pub fn follows(&self, expected_revision: Option<u64>, current: Option<&Self>) -> bool {
        match (expected_revision, current) {
            (None, None) => self.revision == 0,
            (Some(expected), Some(current)) => {
                current.revision == expected
                    && expected.checked_add(1) == Some(self.revision)
                    && current
                        .completed_targets
                        .iter()
                        .all(|target| self.completed_targets.contains(target))
            }
            _ => false,
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
    /// `None` means the row must still be absent. Every adapter evaluates
    /// [`ErasureProgress::follows`] against its actual row under one lock or
    /// transaction, then atomically fences publication at that same revision.
    /// Stale or target-regressive writers receive Conflict without changing it.
    async fn compare_and_swap_progress(
        &self,
        id: &DataSubjectId,
        expected_revision: Option<u64>,
        progress: &ErasureProgress,
    ) -> Result<(), DataSubjectError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateUserProfileCommand {
    pub org: String,
    pub metadata: BTreeMap<String, String>,
    pub relationship: UserProfileRelationship,
    pub access_type: Option<UserProfileAccessType>,
    pub external_id: Option<String>,
    pub external_user_onboarded_at: Option<i64>,
    pub external_user_details: Option<ExternalUserDetailsUpdate>,
    pub name: Option<String>,
}

/// Platform-stated facts about the entity behind a profile. Every field is
/// `None` until the platform supplies it; the platform's statements are
/// recorded as given, never verified here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalUserDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_status: Option<ExternalAccountStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<ExternalEntityType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarded_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAccountStatus {
    Active,
    Suspended,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEntityType {
    Individual,
    Business,
    NonProfit,
    Government,
}

/// A per-field patch of [`ExternalUserDetails`]: `None` keeps the recorded
/// value, `Some(Clear)` erases it, `Some(Replace)` overwrites it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalUserDetailsUpdate {
    pub account_status: Option<UserProfileFieldUpdate<ExternalAccountStatus>>,
    pub country: Option<UserProfileFieldUpdate<String>>,
    pub email_hash: Option<UserProfileFieldUpdate<String>>,
    pub entity_type: Option<UserProfileFieldUpdate<ExternalEntityType>>,
    pub name_hash: Option<UserProfileFieldUpdate<String>>,
    pub onboarded_at: Option<i64>,
    pub reference_id: Option<UserProfileFieldUpdate<String>>,
}

impl ExternalUserDetailsUpdate {
    /// Apply the patch; returns whether anything recorded changed.
    fn apply(&self, details: &mut ExternalUserDetails) -> bool {
        fn field<T: Clone + PartialEq>(
            slot: &mut Option<T>,
            update: &Option<UserProfileFieldUpdate<T>>,
        ) -> bool {
            let next = match update {
                None => return false,
                Some(UserProfileFieldUpdate::Clear) => None,
                Some(UserProfileFieldUpdate::Replace(value)) => Some(value.clone()),
            };
            let changed = *slot != next;
            *slot = next;
            changed
        }
        let mut changed = false;
        changed |= field(&mut details.account_status, &self.account_status);
        changed |= field(&mut details.country, &self.country);
        changed |= field(&mut details.email_hash, &self.email_hash);
        changed |= field(&mut details.entity_type, &self.entity_type);
        changed |= field(&mut details.name_hash, &self.name_hash);
        if let Some(onboarded_at) = self.onboarded_at {
            changed |= details.onboarded_at != Some(onboarded_at);
            details.onboarded_at = Some(onboarded_at);
        }
        changed |= field(&mut details.reference_id, &self.reference_id);
        changed
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateUserProfileCommand {
    pub metadata: Option<BTreeMap<String, String>>,
    pub relationship: Option<UserProfileRelationship>,
    pub access_type: Option<Option<UserProfileAccessType>>,
    pub trust_grants: Option<BTreeMap<String, UserProfileTrustGrant>>,
    pub external_id: Option<UserProfileFieldUpdate<String>>,
    pub external_user_onboarded_at: Option<i64>,
    pub external_user_details: Option<ExternalUserDetailsUpdate>,
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
    pub access_type: Option<UserProfileAccessType>,
    pub trust_grants: BTreeMap<String, UserProfileTrustGrant>,
    pub external_id: Option<String>,
    pub external_user_onboarded_at: Option<i64>,
    pub external_user_details: Option<ExternalUserDetails>,
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
            access_type: subject.access_type,
            trust_grants: subject.trust_grants.clone(),
            external_id: subject.external_id.clone(),
            external_user_onboarded_at: subject.external_user_onboarded_at,
            external_user_details: subject.external_user_details.clone(),
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
    /// The one clock every subject timestamp and enrollment expiry is judged
    /// by. The erasure resolver over the same repository must hold the same
    /// handle, or `erased_at` and the `updated_at` beside it disagree.
    clock: Arc<dyn WallClock>,
    enrollment_key: Vec<u8>,
}

impl DataSubjectApplication {
    pub fn new(
        repo: Arc<dyn DataSubjectRepo>,
        enrollment_key: Vec<u8>,
        clock: Arc<dyn WallClock>,
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
        let now = self.clock.now().as_epoch_ms_i64();
        for _ in 0..CAS_RETRY_LIMIT {
            let id = DataSubjectId(format!("uprof_{}", uuid::Uuid::new_v4().simple()));
            let mut subject = DataSubject::new(id, command.org.clone(), now);
            subject.metadata = command.metadata.clone();
            subject.relationship = command.relationship;
            subject.access_type = command.access_type;
            subject.external_id = command.external_id.clone();
            subject.external_user_onboarded_at = command.external_user_onboarded_at;
            if let Some(details) = &command.external_user_details {
                let mut recorded = ExternalUserDetails::default();
                details.apply(&mut recorded);
                subject.external_user_details = Some(recorded);
            }
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
            if let Some(external_user_onboarded_at) = command.external_user_onboarded_at {
                changed |= subject.external_user_onboarded_at != Some(external_user_onboarded_at);
                subject.external_user_onboarded_at = Some(external_user_onboarded_at);
            }
            if let Some(details) = &command.external_user_details {
                // A first patch materializes the record so later reads see
                // every field, `null` where the platform said nothing.
                let recorded = subject
                    .external_user_details
                    .get_or_insert_with(Default::default);
                changed |= details.apply(recorded);
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
            if let Some(access_type) = command.access_type {
                changed |= subject.access_type != access_type;
                subject.access_type = access_type;
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
                granted_at: self.clock.now().as_epoch_ms_i64(),
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
        resolver: &dyn awaken_data_governance_contract::DataSubjectResolver,
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
            .now()
            .as_epoch_ms_i64()
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
        let now = self.clock.now().as_epoch_ms_i64();
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
            subject.updated_at = subject.updated_at.max(self.clock.now().as_epoch_ms_i64());
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
                    subject.updated_at = subject.updated_at.max(self.clock.now().as_epoch_ms_i64());
                    let expected = advance_revision(&mut subject)?;
                    match self.repo.compare_and_swap(expected, subject.clone()).await {
                        Ok(()) => return Ok(subject),
                        Err(DataSubjectError::Conflict(_)) => continue,
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(_) => return Err(DataSubjectApplicationError::NotFound),
                Err(DataSubjectError::NotFound(_)) => {
                    let now = self.clock.now().as_epoch_ms_i64();
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
        // The enrollment token expires at `expires_at` (the one Expiry law).
        if awaken_time::Expiry::from_persisted_ms(payload.expires_at)
            .is_expired_at(self.clock.now())
            || payload.purposes.is_empty()
        {
            return Err(DataSubjectApplicationError::InvalidEnrollment);
        }
        Ok(payload)
    }
}

/// Organization-level privacy process manager over the one subject repository
/// and the one subject erasure resolver.
pub struct RepoOrganizationPrivacyResolver {
    repo: Arc<dyn DataSubjectRepo>,
    subjects: Arc<dyn awaken_data_governance_contract::DataSubjectResolver>,
}

impl RepoOrganizationPrivacyResolver {
    #[must_use]
    pub fn new(
        repo: Arc<dyn DataSubjectRepo>,
        subjects: Arc<dyn awaken_data_governance_contract::DataSubjectResolver>,
    ) -> Self {
        Self { repo, subjects }
    }

    async fn selected_subjects(
        &self,
        organization_id: &str,
        subject: Option<&DataSubjectId>,
    ) -> Result<Vec<DataSubject>, awaken_data_governance_contract::PrivacyExportError> {
        match subject {
            Some(subject) => match self.repo.get(subject).await {
                Ok(record) if record.org == organization_id => Ok(vec![record]),
                Ok(_) | Err(DataSubjectError::NotFound(_)) => {
                    Err(awaken_data_governance_contract::PrivacyExportError(
                        "data subject is not owned by the requested organization".into(),
                    ))
                }
                Err(error) => Err(awaken_data_governance_contract::PrivacyExportError(
                    error.to_string(),
                )),
            },
            None => self.repo.list(organization_id).await.map_err(|error| {
                awaken_data_governance_contract::PrivacyExportError(error.to_string())
            }),
        }
    }
}

#[async_trait]
impl awaken_data_governance_contract::OrganizationPrivacyResolver
    for RepoOrganizationPrivacyResolver
{
    async fn erase_organization(
        &self,
        organization_id: &str,
    ) -> Result<ErasureReceipt, awaken_data_governance_contract::ErasureError> {
        let mut subjects = self.repo.list(organization_id).await.map_err(|error| {
            awaken_data_governance_contract::ErasureError(format!(
                "organization subject inventory failed: {error}"
            ))
        })?;
        subjects.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        let mut records_removed = 0usize;
        for subject in subjects {
            records_removed = records_removed
                .checked_add(self.subjects.erase(&subject.id).await?.records_removed)
                .ok_or_else(|| {
                    awaken_data_governance_contract::ErasureError(
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
        awaken_data_governance_contract::OrganizationPrivacyExport,
        awaken_data_governance_contract::PrivacyExportError,
    > {
        let mut subjects = self.selected_subjects(organization_id, subject).await?;
        subjects.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        let records = subjects
            .into_iter()
            .map(|subject| {
                let id = subject.id.clone();
                serde_json::to_value(subject)
                    .map(
                        |payload| awaken_data_governance_contract::OrganizationPrivacyRecord {
                            subject: id,
                            payload,
                        },
                    )
                    .map_err(|error| {
                        awaken_data_governance_contract::PrivacyExportError(error.to_string())
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(awaken_data_governance_contract::OrganizationPrivacyExport { records })
    }
}

/// Bridges the neutral [`DataSubjectResolver`](awaken_data_governance_contract::DataSubjectResolver)
/// port to a [`DataSubjectRepo`] (ADR-0050 D10a). An unknown subject resolves to
/// `Structured` (no content without a known, consenting subject). Holds an
/// `Arc`-shared repo so a consent-write path and the resolver see one store.
pub struct RepoDataSubjectResolver {
    repo: Arc<dyn DataSubjectRepo>,
    jobs: Arc<dyn ErasureJobRepo>,
    /// Stamps `erased_at`. It must be the clock the [`DataSubjectApplication`]
    /// over the same repository holds: the erasure stamp also moves the
    /// aggregate's `updated_at`, which that application stamped before.
    clock: Arc<dyn WallClock>,
    coordinator: Option<Arc<dyn awaken_data_governance_contract::ContentEraser>>,
    resources: Option<Arc<dyn awaken_data_governance_contract::ContentEraser>>,
}

impl RepoDataSubjectResolver {
    #[must_use]
    pub fn new(
        repo: Arc<dyn DataSubjectRepo>,
        jobs: Arc<dyn ErasureJobRepo>,
        clock: Arc<dyn WallClock>,
    ) -> Self {
        Self {
            repo,
            jobs,
            clock,
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
        eraser: Arc<dyn awaken_data_governance_contract::ContentEraser>,
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
    ) -> Result<(), awaken_data_governance_contract::ErasureError> {
        for _ in 0..CAS_RETRY_LIMIT {
            let mut aggregate = match self.repo.get(subject).await {
                Ok(aggregate) => aggregate,
                Err(DataSubjectError::NotFound(_)) => return Ok(()),
                Err(error) => {
                    return Err(awaken_data_governance_contract::ErasureError(format!(
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
            aggregate.mark_erased(self.clock.now().as_epoch_ms_i64());
            let expected = advance_revision(&mut aggregate).map_err(|error| {
                awaken_data_governance_contract::ErasureError(format!(
                    "accountability write failed: {error}"
                ))
            })?;
            match self.repo.compare_and_swap(expected, aggregate).await {
                Ok(()) => return Ok(()),
                Err(DataSubjectError::Conflict(_)) => continue,
                Err(error) => {
                    return Err(awaken_data_governance_contract::ErasureError(format!(
                        "accountability write failed: {error}"
                    )));
                }
            }
        }
        Err(awaken_data_governance_contract::ErasureError(
            "accountability write failed: conflict retry budget exhausted".into(),
        ))
    }
}

fn advance_erasure_progress(
    progress: &mut ErasureProgress,
    expected_revision: Option<u64>,
) -> Result<(), awaken_data_governance_contract::ErasureError> {
    progress.revision = match expected_revision {
        None => 0,
        Some(expected) => expected.checked_add(1).ok_or_else(|| {
            awaken_data_governance_contract::ErasureError(
                "erasure checkpoint revision exhausted".into(),
            )
        })?,
    };
    Ok(())
}

#[async_trait]
impl awaken_data_governance_contract::DataSubjectConsentSource for RepoDataSubjectResolver {
    async fn consent_ceiling(&self, subject: &DataSubjectId, purpose: Purpose) -> ContentCapture {
        match self.repo.get(subject).await {
            Ok(s) => s.consent_ceiling(purpose),
            Err(_) => ContentCapture::Structured,
        }
    }
}

#[async_trait]
impl awaken_data_governance_contract::DataSubjectResolver for RepoDataSubjectResolver {
    async fn erase(
        &self,
        subject: &DataSubjectId,
    ) -> Result<ErasureReceipt, awaken_data_governance_contract::ErasureError> {
        // Cause ordering is durable and common to every caller: the next target
        // effect, accountability stamp, then terminal receipt. Multiple replicas
        // may replay an idempotent target effect, but only one revision-CAS may
        // count it; losers reload the winning checkpoint and continue.
        for _ in 0..CAS_RETRY_LIMIT {
            let loaded = self.jobs.load(subject).await.map_err(|error| {
                awaken_data_governance_contract::ErasureError(error.to_string())
            })?;
            let expected_revision = loaded.as_ref().map(|progress| progress.revision);
            let mut progress = loaded.unwrap_or_default();
            let completing = match progress.next_action([
                (ErasureTarget::Coordinator, self.coordinator.as_ref()),
                (ErasureTarget::Resources, self.resources.as_ref()),
            ]) {
                ErasureAction::InvalidCheckpoint => {
                    return Err(awaken_data_governance_contract::ErasureError(
                        "invalid erasure checkpoint".into(),
                    ));
                }
                ErasureAction::Replay => {
                    return Ok(ErasureReceipt {
                        records_removed: progress.records_removed,
                    });
                }
                ErasureAction::Erase(target, eraser) => {
                    progress.record_target(target, eraser.erase_subject(subject).await?)?;
                    false
                }
                ErasureAction::Stamp => {
                    self.stamp_accountability(subject).await?;
                    progress.accountability_stamped = true;
                    false
                }
                ErasureAction::Complete => {
                    progress.complete = true;
                    true
                }
            };
            advance_erasure_progress(&mut progress, expected_revision)?;
            match self
                .jobs
                .compare_and_swap_progress(subject, expected_revision, &progress)
                .await
            {
                Ok(()) if completing => {
                    return Ok(ErasureReceipt {
                        records_removed: progress.records_removed,
                    });
                }
                Ok(()) | Err(DataSubjectError::Conflict(_)) => continue,
                Err(error) => {
                    return Err(awaken_data_governance_contract::ErasureError(
                        error.to_string(),
                    ));
                }
            }
        }
        Err(awaken_data_governance_contract::ErasureError(
            "erasure checkpoint conflict retry budget exhausted".into(),
        ))
    }
}

#[cfg(test)]
mod tests;

#[cfg(any(test, kani))]
mod erasure_verification;
