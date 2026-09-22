//! Durable physical-reclamation vocabulary for platform resources.
//!
//! This is resource lifecycle state, not an authorization decision. A PEP/PDP has
//! already authorized the logical delete before an application service creates an
//! intent. Consequently these values contain intrinsic Workspace ownership,
//! resource identity, retention, references and fenced-work metadata only—never a
//! principal, API key, role, policy, Org, Project or WorkUnit.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Resource families governed by the Resources context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    File,
    MemoryStore,
    Repository,
    Skill,
}

/// Stable target of one logical-delete/reclamation workflow.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceTarget {
    pub workspace_id: String,
    pub kind: ResourceKind,
    pub resource_id: String,
}

impl ResourceTarget {
    #[must_use]
    pub fn new(
        workspace_id: impl Into<String>,
        kind: ResourceKind,
        resource_id: impl Into<String>,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            kind,
            resource_id: resource_id.into(),
        }
    }

    fn validate(&self) -> Result<(), ResourcePurgeError> {
        for (name, value) in [
            ("workspace_id", self.workspace_id.as_str()),
            ("resource_id", self.resource_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(ResourcePurgeError::Invalid(
                    format!("target {name} must not be empty").into(),
                ));
            }
        }
        Ok(())
    }
}

/// Intrinsic references which postpone physical deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceReferenceKind {
    LogicalLifecycle,
    #[serde(alias = "workspace_grant")]
    WorkspaceOwnership,
    AgentBinding,
    SessionBinding,
    Artifact,
    RuntimeHandle,
    ExtractionIntent,
    RetentionHold,
}

/// One opaque reference reported by a resource/reference index.
///
/// `reference_id` identifies the holder for audit and retry convergence. It is
/// deliberately not a principal or an authorization-policy subject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceReference {
    pub kind: ResourceReferenceKind,
    pub reference_id: String,
}

/// Workspace-scoped reference row. This is an internal resource-lifecycle fact,
/// `WorkspaceOwnership` means a File data-plane ownership/reference edge and
/// never carries an authorization grant or the principal that created it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceReferenceRecord {
    pub target: ResourceTarget,
    pub reference: ResourceReference,
}

/// Result of atomically fencing one physical resource identity for reclamation.
///
/// The fence key is `(kind, resource_id)`, deliberately excluding Workspace:
/// content-addressed data such as File blobs may be referenced by more than one
/// Workspace and must not be deleted while any such reference exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireResourceReclamationOutcome {
    /// This intent installed the fence.
    Acquired,
    /// A retry found the fence already owned by this same intent.
    AlreadyOwned,
    /// Intrinsic resource references prevented the fence from being installed.
    Blocked(Vec<ResourceReferenceRecord>),
    /// A different reclamation intent currently owns the physical identity.
    Contended,
}

// Preserve historical receipt bytes when no proposal was purged.
fn zero_count(value: &u64) -> bool {
    *value == 0
}

/// Per-kind, immutable proof returned by the physical adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourcePurgeEvidence {
    File {
        blob_deleted: bool,
    },
    MemoryStore {
        heads_deleted: u64,
        versions_deleted: u64,
        #[serde(default, skip_serializing_if = "zero_count")]
        copy_conflicts_deleted: u64,
    },
    Repository {
        local_realizations_deleted: u64,
    },
    Skill {
        versions_deleted: u64,
    },
}

impl ResourcePurgeEvidence {
    #[must_use]
    pub fn kind(&self) -> ResourceKind {
        match self {
            Self::File { .. } => ResourceKind::File,
            Self::MemoryStore { .. } => ResourceKind::MemoryStore,
            Self::Repository { .. } => ResourceKind::Repository,
            Self::Skill { .. } => ResourceKind::Skill,
        }
    }
}

/// Durable evidence that physical reclamation completed for one tombstone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePurgeReceipt {
    pub purged_at_unix_ms: u64,
    pub evidence: ResourcePurgeEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePurgeStatus {
    Pending,
    Claimed,
    Completed,
}

impl ResourcePurgeStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        self == Self::Completed
    }
}

/// Closed admission kernel shared by every mutation which consumes a resource
/// reclamation claim. Callers project the durable claim fields into these four
/// predicates; no completion, retry or defer path can bypass this decision.
const fn resource_purge_claim_is_current(
    status_is_claimed: bool,
    owner_matches: bool,
    generation_matches: bool,
    lease_is_live: bool,
) -> bool {
    status_is_claimed && owner_matches && generation_matches && lease_is_live
}

/// Durable reclamation intent. A resource owner may persist this before logical
/// denial; its ownership reference must then protect the unfinished prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePurgeIntent {
    pub intent_id: String,
    pub idempotency_key: String,
    pub target: ResourceTarget,
    /// Frozen configuration generation which was current at logical deletion.
    /// Files have no mutable configuration and therefore use `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_version: Option<u64>,
    pub requested_at_unix_ms: u64,
    pub not_before_unix_ms: u64,
    pub status: ResourcePurgeStatus,
    pub attempts: u32,
    pub revision: u64,
    pub claim_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub blockers: Vec<ResourceReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ResourcePurgeReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourcePurgeError {
    #[error(transparent)]
    Blob(#[from] crate::FileStoreError),
    #[error("invalid resource purge intent: {0}")]
    Invalid(crate::ResourceErrorDetail),
    #[error("resource purge intent `{0}` was not found")]
    NotFound(String),
    #[error("resource purge idempotency key `{0}` has different content")]
    IdempotencyConflict(String),
    #[error("resource purge intent `{0}` changed concurrently")]
    RevisionConflict(String),
    #[error("resource purge intent is already claimed until {lease_expires_at_unix_ms}")]
    LeaseHeld { lease_expires_at_unix_ms: u64 },
    #[error("resource purge retention window has not elapsed until {not_before_unix_ms}")]
    RetentionHeld { not_before_unix_ms: u64 },
    #[error("stale resource purge claim")]
    StaleClaim,
    #[error("resource `{kind:?}/{resource_id}` is fenced for physical reclamation")]
    ReclamationFenced {
        kind: ResourceKind,
        resource_id: String,
    },
    #[error("resource reclamation fence is owned by another intent")]
    StaleReclamationFence,
    #[error("invalid resource purge transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: ResourcePurgeStatus,
        to: ResourcePurgeStatus,
    },
    #[error("resource purge repository failure: {0}")]
    Storage(crate::ResourceErrorDetail),
}

impl ResourcePurgeIntent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        intent_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        target: ResourceTarget,
        config_version: Option<u64>,
        requested_at_unix_ms: u64,
        not_before_unix_ms: u64,
    ) -> Result<Self, ResourcePurgeError> {
        let intent = Self {
            intent_id: intent_id.into(),
            idempotency_key: idempotency_key.into(),
            target,
            config_version,
            requested_at_unix_ms,
            not_before_unix_ms,
            status: ResourcePurgeStatus::Pending,
            attempts: 0,
            revision: 0,
            claim_generation: 0,
            claim_owner: None,
            lease_expires_at_unix_ms: None,
            blockers: Vec::new(),
            receipt: None,
            last_error: None,
        };
        intent.validate()?;
        Ok(intent)
    }

    pub fn validate(&self) -> Result<(), ResourcePurgeError> {
        self.target.validate()?;
        for (name, value) in [
            ("intent_id", self.intent_id.as_str()),
            ("idempotency_key", self.idempotency_key.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(ResourcePurgeError::Invalid(
                    format!("{name} must not be empty").into(),
                ));
            }
        }
        if self
            .config_version
            .is_some_and(|config_version| config_version == 0)
        {
            return Err(ResourcePurgeError::Invalid(
                "config_version must be positive when present".into(),
            ));
        }
        if self.not_before_unix_ms < self.requested_at_unix_ms {
            return Err(ResourcePurgeError::Invalid(
                "not_before_unix_ms precedes requested_at_unix_ms".into(),
            ));
        }
        if self
            .blockers
            .iter()
            .any(|reference| reference.reference_id.trim().is_empty())
        {
            return Err(ResourcePurgeError::Invalid(
                "reference_id must not be empty".into(),
            ));
        }
        Ok(())
    }

    /// Compare immutable request fields only; delivery after progress is idempotent.
    #[must_use]
    pub fn same_request(&self, other: &Self) -> bool {
        self.intent_id == other.intent_id
            && self.idempotency_key == other.idempotency_key
            && self.target == other.target
            && self.config_version == other.config_version
    }

    pub fn claim(
        &mut self,
        owner: &str,
        now_unix_ms: u64,
        lease_ms: u64,
    ) -> Result<u64, ResourcePurgeError> {
        if owner.trim().is_empty() || lease_ms == 0 {
            return Err(ResourcePurgeError::Invalid(
                "claim owner and positive lease are required".into(),
            ));
        }
        if self.status.is_terminal() {
            return Err(ResourcePurgeError::InvalidTransition {
                from: self.status,
                to: ResourcePurgeStatus::Claimed,
            });
        }
        if self.not_before_unix_ms > now_unix_ms {
            return Err(ResourcePurgeError::RetentionHeld {
                not_before_unix_ms: self.not_before_unix_ms,
            });
        }
        if self
            .lease_expires_at_unix_ms
            .is_some_and(|expires| expires > now_unix_ms)
        {
            return Err(ResourcePurgeError::LeaseHeld {
                lease_expires_at_unix_ms: self.lease_expires_at_unix_ms.unwrap_or_default(),
            });
        }
        self.claim_generation = self
            .claim_generation
            .checked_add(1)
            .ok_or_else(|| ResourcePurgeError::Invalid("claim generation exhausted".into()))?;
        self.attempts = self.attempts.saturating_add(1);
        self.status = ResourcePurgeStatus::Claimed;
        self.claim_owner = Some(owner.to_string());
        self.lease_expires_at_unix_ms = Some(
            now_unix_ms
                .checked_add(lease_ms)
                .ok_or_else(|| ResourcePurgeError::Invalid("claim lease overflow".into()))?,
        );
        self.blockers.clear();
        self.last_error = None;
        self.bump_revision()?;
        Ok(self.claim_generation)
    }

    /// Release a claim because live references still exist. This is normal
    /// convergence, not a terminal failure.
    pub fn defer(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
        mut blockers: Vec<ResourceReference>,
    ) -> Result<(), ResourcePurgeError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        if blockers.is_empty() {
            return Err(ResourcePurgeError::Invalid(
                "defer requires at least one blocker".into(),
            ));
        }
        blockers.sort();
        blockers.dedup();
        self.blockers = blockers;
        self.status = ResourcePurgeStatus::Pending;
        self.clear_claim();
        self.bump_revision()
    }

    pub fn retry(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
        error: impl Into<String>,
    ) -> Result<(), ResourcePurgeError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        self.status = ResourcePurgeStatus::Pending;
        self.last_error = Some(error.into());
        self.clear_claim();
        self.bump_revision()
    }

    pub fn complete(
        &mut self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
        receipt: ResourcePurgeReceipt,
    ) -> Result<(), ResourcePurgeError> {
        self.require_claim(owner, generation, now_unix_ms)?;
        if receipt.evidence.kind() != self.target.kind {
            return Err(ResourcePurgeError::Invalid(
                "receipt evidence kind does not match target".into(),
            ));
        }
        self.receipt = Some(receipt);
        self.status = ResourcePurgeStatus::Completed;
        self.clear_claim();
        self.bump_revision()
    }

    fn require_claim(
        &self,
        owner: &str,
        generation: u64,
        now_unix_ms: u64,
    ) -> Result<(), ResourcePurgeError> {
        let valid = resource_purge_claim_is_current(
            self.status == ResourcePurgeStatus::Claimed,
            self.claim_owner.as_deref() == Some(owner),
            self.claim_generation == generation,
            self.lease_expires_at_unix_ms
                .is_some_and(|expires| expires > now_unix_ms),
        );
        if valid {
            Ok(())
        } else {
            Err(ResourcePurgeError::StaleClaim)
        }
    }

    fn clear_claim(&mut self) {
        self.claim_owner = None;
        self.lease_expires_at_unix_ms = None;
    }

    fn bump_revision(&mut self) -> Result<(), ResourcePurgeError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| ResourcePurgeError::Invalid("revision exhausted".into()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutResourcePurgeOutcome {
    Inserted,
    Existing,
}

/// Atomic resource-plane fence around physical deletion.
///
/// Implementations must serialize `acquire_reclamation` with reference writes:
/// acquiring succeeds only when the physical identity has no references, and a
/// reference writer must fail closed while a fence exists. This is lifecycle
/// consistency—not an IAM grant, policy lock, or authorization decision.
#[async_trait]
pub trait ResourceReclamationFence: Send + Sync {
    async fn acquire_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<AcquireResourceReclamationOutcome, ResourcePurgeError>;

    /// Release only a fence owned by `intent_id`. Absence is an idempotent
    /// success; a fence owned by another intent fails closed.
    async fn release_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<bool, ResourcePurgeError>;
}

/// Durable storage port. Implementations compare `revision` on update so a stale
/// worker can never overwrite a recovered claim or receipt.
#[async_trait]
pub trait ResourcePurgeRepository: ResourceReclamationFence + Send + Sync {
    async fn put(
        &self,
        intent: ResourcePurgeIntent,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError>;
    async fn get(&self, intent_id: &str)
    -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError>;
    async fn recoverable(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError>;
    async fn save(
        &self,
        expected_revision: u64,
        intent: ResourcePurgeIntent,
    ) -> Result<(), ResourcePurgeError>;
}

/// Durable reverse-reference index used by deletion safety predicates.
///
/// Writers are the application services which own the corresponding binding or
/// activation lifecycle. A stale extra row leaks storage safely; an omitted row
/// would be unsafe, so adapters must commit reference creation before exposing
/// the referencing aggregate.
#[async_trait]
pub trait ResourceReferenceIndex: Send + Sync {
    async fn add_reference(
        &self,
        record: ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError>;
    async fn remove_reference(
        &self,
        record: &ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError>;
    /// Atomically replace every reference owned by one application holder. This
    /// prevents a manifest replacement from exposing either missing or stale
    /// safety edges across a crash.
    async fn replace_references(
        &self,
        kind: ResourceReferenceKind,
        reference_id: &str,
        records: Vec<ResourceReferenceRecord>,
    ) -> Result<(), ResourcePurgeError>;
    async fn references(
        &self,
        target: &ResourceTarget,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError>;
    /// Cross-Workspace lookup is internal to physical GC. It is required for a
    /// globally content-addressed File blob shared by multiple Workspace owners.
    async fn references_for_resource(
        &self,
        kind: ResourceKind,
        resource_id: &str,
    ) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError>;
}

/// Composition convenience for one adapter implementing both durable lifecycle
/// SPIs. It adds no behavior and keeps callers dependent on the two segregated
/// interfaces above.
pub trait ResourceReclamationRepository: ResourcePurgeRepository + ResourceReferenceIndex {}

impl<T> ResourceReclamationRepository for T where T: ResourcePurgeRepository + ResourceReferenceIndex
{}

/// Narrow application port used by resource owners which must schedule physical
/// cleanup without depending on the reclaimer repository or worker protocol.
#[async_trait]
pub trait ResourcePurgeScheduler: Send + Sync {
    async fn schedule_purge(
        &self,
        target: ResourceTarget,
        config_version: Option<u64>,
        requested_at_unix_ms: u64,
        not_before_unix_ms: u64,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError>;
}

/// Resume only logical effects authorized by the exact durable purge intent.
/// This stage owns no queue or physical deletion. Errors prevent fence acquisition;
/// unknown intents must leave unrelated logical owners and references untouched.
#[async_trait]
pub trait ResourcePurgePreparation: Send + Sync {
    async fn prepare(&self, intent: &ResourcePurgeIntent) -> Result<(), ResourcePurgeError>;
}

/// One independently replaceable safety predicate. Composition can combine
/// catalog lifecycle, binding indexes, runtime handles and retention sources
/// without making a resource store depend on IAM or another bounded context.
#[async_trait]
pub trait ResourcePurgeGuard: Send + Sync {
    async fn blockers(
        &self,
        target: &ResourceTarget,
        config_version: Option<u64>,
        now_unix_ms: u64,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError>;
}

/// Idempotent per-kind physical deletion port. Implementations must return a
/// successful receipt when the requested physical state was already reached.
#[async_trait]
pub trait ResourcePhysicalReclaimer: Send + Sync {
    async fn purge(
        &self,
        target: &ResourceTarget,
        config_version: Option<u64>,
    ) -> Result<ResourcePurgeEvidence, ResourcePurgeError>;
}

#[cfg(test)]
mod tests {
    #[test]
    fn memory_purge_evidence_adds_nonzero_candidates_without_rewriting_legacy_receipts() {
        // RC-WIRE legacy/no-candidate => exact unchanged JSON; a nonzero count
        // roundtrips so resource reclamation accounts for retained content.
        let legacy =
            serde_json::json!({"kind":"memory_store","heads_deleted":1,"versions_deleted":2});
        for count in [0, 3] {
            let mut wire = legacy.clone();
            if count != 0 {
                wire["copy_conflicts_deleted"] = serde_json::json!(count);
            }
            let evidence: super::ResourcePurgeEvidence =
                serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(evidence).unwrap(), wire);
        }
    }
    use proptest::prelude::*;
    use serde_json::Value;

    use super::*;

    fn intent(kind: ResourceKind) -> ResourcePurgeIntent {
        ResourcePurgeIntent::new(
            "purge-1",
            "delete:ws-a:file-1:1",
            ResourceTarget::new("ws-a", kind, "file-1"),
            None,
            10,
            20,
        )
        .unwrap()
    }

    #[test]
    fn lifecycle_wire_is_authorization_free_and_reads_legacy_ownership_name() {
        let mut value = intent(ResourceKind::File);
        value.blockers.push(ResourceReference {
            kind: ResourceReferenceKind::WorkspaceOwnership,
            reference_id: "workspace-a".into(),
        });
        let wire = serde_json::to_value(&value).unwrap();
        let object = wire.as_object().unwrap();
        for forbidden in [
            "principal",
            "api_key",
            "role",
            "policy",
            "organization_id",
            "project_id",
            "work_unit_id",
        ] {
            assert!(!object.contains_key(forbidden));
        }
        let legacy: ResourceReferenceKind =
            serde_json::from_value(Value::String("workspace_grant".into())).unwrap();
        assert_eq!(legacy, ResourceReferenceKind::WorkspaceOwnership);
        assert_eq!(
            serde_json::to_value(ResourceReferenceKind::WorkspaceOwnership).unwrap(),
            Value::String("workspace_ownership".into())
        );
    }

    #[test]
    fn retention_claim_defer_and_reclaim_are_fenced() {
        let mut value = intent(ResourceKind::File);
        assert_eq!(
            value.claim("worker-a", 19, 10),
            Err(ResourcePurgeError::RetentionHeld {
                not_before_unix_ms: 20
            })
        );
        let first = value.claim("worker-a", 20, 10).unwrap();
        assert_eq!(first, 1);
        assert!(matches!(
            value.claim("worker-b", 21, 10),
            Err(ResourcePurgeError::LeaseHeld { .. })
        ));
        value
            .defer(
                "worker-a",
                first,
                21,
                vec![ResourceReference {
                    kind: ResourceReferenceKind::SessionBinding,
                    reference_id: "session-1".into(),
                }],
            )
            .unwrap();
        let second = value.claim("worker-b", 22, 10).unwrap();
        assert_eq!(second, 2);
        assert_eq!(
            value.complete(
                "worker-a",
                first,
                23,
                ResourcePurgeReceipt {
                    purged_at_unix_ms: 23,
                    evidence: ResourcePurgeEvidence::File { blob_deleted: true },
                }
            ),
            Err(ResourcePurgeError::StaleClaim)
        );
        value
            .complete(
                "worker-b",
                second,
                23,
                ResourcePurgeReceipt {
                    purged_at_unix_ms: 23,
                    evidence: ResourcePurgeEvidence::File { blob_deleted: true },
                },
            )
            .unwrap();
        assert_eq!(value.status, ResourcePurgeStatus::Completed);
    }

    #[test]
    fn receipt_admission_requires_the_exact_live_claim() {
        // Causes: C1 status is Claimed; C2 owner matches; C3 generation matches;
        // C4 lease exists and has not expired. Effect E1 permits a mutation that
        // can attach the purge receipt; E2 rejects it as stale. Constraint: all
        // four durable claim facts are conjunctive, so changing any one fact
        // from the sole accepting rule fails closed. Decision table:
        // R1=1111=>E1; R2=0xxx=>E2; R3=10xx=>E2; R4=110x=>E2; R5=1110=>E2.
        for (claimed, owner, generation, live, expected) in [
            (true, true, true, true, true),
            (false, true, true, true, false),
            (true, false, true, true, false),
            (true, true, false, true, false),
            (true, true, true, false, false),
        ] {
            assert_eq!(
                resource_purge_claim_is_current(claimed, owner, generation, live),
                expected
            );
        }
    }

    #[test]
    fn receipt_kind_must_match_the_target() {
        let mut value = intent(ResourceKind::File);
        let generation = value.claim("worker", 20, 10).unwrap();
        assert!(matches!(
            value.complete(
                "worker",
                generation,
                21,
                ResourcePurgeReceipt {
                    purged_at_unix_ms: 21,
                    evidence: ResourcePurgeEvidence::Skill {
                        versions_deleted: 1
                    },
                }
            ),
            Err(ResourcePurgeError::Invalid(_))
        ));
    }

    #[test]
    fn immutable_request_comparison_ignores_progress() {
        let original = intent(ResourceKind::File);
        let mut progressed = original.clone();
        progressed.claim("worker", 20, 10).unwrap();
        assert!(original.same_request(&progressed));
    }

    proptest! {
        #[test]
        fn retention_is_a_hard_lower_bound(requested in 0u64..1_000_000, delay in 1u64..1_000_000) {
            let not_before = requested.saturating_add(delay);
            let mut value = ResourcePurgeIntent::new(
                "purge", "key", ResourceTarget::new("ws", ResourceKind::File, "file"),
                None, requested, not_before,
            ).unwrap();
            let held = matches!(
                value.claim("worker", not_before - 1, 1),
                Err(ResourcePurgeError::RetentionHeld { .. })
            );
            prop_assert!(held);
            prop_assert!(value.claim("worker", not_before, 1).is_ok());
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn stale_resource_purge_claim_cannot_commit_receipt() {
    // Symbolic form of the receipt-admission decision table: Kani covers every
    // C1/C2/C3/C4 combination and proves E1 is exactly their conjunction.
    let status_is_claimed = kani::any::<bool>();
    let owner_matches = kani::any::<bool>();
    let generation_matches = kani::any::<bool>();
    let lease_is_live = kani::any::<bool>();
    let admitted = resource_purge_claim_is_current(
        status_is_claimed,
        owner_matches,
        generation_matches,
        lease_is_live,
    );

    assert_eq!(
        admitted,
        status_is_claimed && owner_matches && generation_matches && lease_is_live
    );
    if !(status_is_claimed && owner_matches && generation_matches && lease_is_live) {
        assert!(!admitted);
    }
}
