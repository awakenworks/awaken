// @formal entrypoint delegation.cancel_intent
// @formal entrypoint delegation.depth_cycle_parallel_total
// @formal entrypoint delegation.relationship_identity
// @formal entrypoint delegation.relationship_transition
// @formal evidence delegation.remote_cancel_redelivery
//! Durable parent/child Run relationships created by Agent tool calls.
//!
//! This aggregate owns relationship identity, lineage budgets, and cancellation.
//! It deliberately does not own tool execution or result delivery: those phases
//! live once in the parent Run's durable `ToolBatch` and commit through the same
//! `ThreadCommit.state` boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::agent::run::Id as RunId;

/// Stable identity of one parent tool call's delegated child Run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DelegationId(pub String);

impl DelegationId {
    /// Length-prefixing prevents ambiguous delimiter collisions.
    #[must_use]
    pub fn for_parent_call(parent_run_id: &RunId, parent_call_id: &str) -> Self {
        Self(format!(
            "delegation:{}:{}:{}:{}",
            parent_run_id.0.len(),
            parent_run_id.0,
            parent_call_id.len(),
            parent_call_id
        ))
    }

    #[must_use]
    pub fn child_run_id(&self) -> RunId {
        RunId(format!("child-run:{}:{}", self.0.len(), self.0))
    }
}

/// Durable origin of a delegated child Run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationOrigin {
    pub delegation_id: DelegationId,
    pub parent_run_id: RunId,
    pub parent_call_id: String,
    pub depth: u16,
    /// Root-to-parent Agent identities. Legacy origins may omit this; production
    /// constructors always populate it so cycle checks survive recovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_lineage: Vec<String>,
}

impl DelegationOrigin {
    /// Compatibility constructor for callers that do not own Agent identity.
    #[must_use]
    pub fn root(parent_run_id: RunId, parent_call_id: impl Into<String>) -> Self {
        Self::root_for_agent(parent_run_id, parent_call_id, String::new())
    }

    /// Build a root relationship with the initiating Agent in its lineage.
    #[must_use]
    pub fn root_for_agent(
        parent_run_id: RunId,
        parent_call_id: impl Into<String>,
        parent_agent_id: impl Into<String>,
    ) -> Self {
        let parent_call_id = parent_call_id.into();
        let parent_agent_id = parent_agent_id.into();
        Self {
            delegation_id: DelegationId::for_parent_call(&parent_run_id, &parent_call_id),
            parent_run_id,
            parent_call_id,
            depth: 1,
            agent_lineage: (!parent_agent_id.is_empty())
                .then_some(parent_agent_id)
                .into_iter()
                .collect(),
        }
    }

    /// Compatibility constructor retaining a numeric parent depth.
    pub fn nested(
        parent_run_id: RunId,
        parent_call_id: impl Into<String>,
        parent_depth: u16,
    ) -> Result<Self, DelegationError> {
        Self::nested_for_agent(
            parent_run_id,
            parent_call_id,
            parent_depth,
            &[],
            String::new(),
        )
    }

    /// Build a nested origin while carrying the recovered Agent lineage forward.
    pub fn nested_for_agent(
        parent_run_id: RunId,
        parent_call_id: impl Into<String>,
        parent_depth: u16,
        ancestor_lineage: &[String],
        parent_agent_id: impl Into<String>,
    ) -> Result<Self, DelegationError> {
        let parent_call_id = parent_call_id.into();
        let depth = parent_depth
            .checked_add(1)
            .ok_or(DelegationError::DepthOverflow)?;
        let parent_agent_id = parent_agent_id.into();
        let mut agent_lineage = ancestor_lineage.to_vec();
        if !parent_agent_id.is_empty() && agent_lineage.last() != Some(&parent_agent_id) {
            agent_lineage.push(parent_agent_id);
        }
        Ok(Self {
            delegation_id: DelegationId::for_parent_call(&parent_run_id, &parent_call_id),
            parent_run_id,
            parent_call_id,
            depth,
            agent_lineage,
        })
    }

    #[must_use]
    pub fn child_run_id(&self) -> RunId {
        self.delegation_id.child_run_id()
    }
}

/// Run-scoped delegation budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationLimits {
    pub max_depth: u16,
    pub max_parallel: u16,
    pub max_total: u32,
}

impl DelegationLimits {
    #[must_use]
    pub const fn new(max_depth: u16, max_parallel: u16, max_total: u32) -> Self {
        Self {
            max_depth,
            max_parallel,
            max_total,
        }
    }
}

/// Heap-free admission kernel shared by the aggregate and Kani. Identity/index
/// conflicts are checked by the aggregate first; this function owns only the
/// parent lifecycle, depth, cycle, parallel, and total-budget predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionError {
    ParentEnded,
    DepthOverflow,
    DepthExceeded { depth: u16, limit: u16 },
    Cycle,
    ParallelLimit { limit: u16 },
    BudgetExhausted { limit: u32 },
}

fn admit_new_delegation(
    parent_ended: bool,
    parent_depth: u16,
    limits: DelegationLimits,
    creates_cycle: bool,
    active_count: usize,
    total_started: u32,
) -> Result<u16, AdmissionError> {
    if parent_ended {
        return Err(AdmissionError::ParentEnded);
    }
    let depth = parent_depth
        .checked_add(1)
        .ok_or(AdmissionError::DepthOverflow)?;
    if depth > limits.max_depth {
        return Err(AdmissionError::DepthExceeded {
            depth,
            limit: limits.max_depth,
        });
    }
    if creates_cycle {
        return Err(AdmissionError::Cycle);
    }
    if active_count >= usize::from(limits.max_parallel) {
        return Err(AdmissionError::ParallelLimit {
            limit: limits.max_parallel,
        });
    }
    if total_started >= limits.max_total {
        return Err(AdmissionError::BudgetExhausted {
            limit: limits.max_total,
        });
    }
    Ok(depth)
}

impl Default for DelegationLimits {
    fn default() -> Self {
        Self::new(8, 8, 64)
    }
}

/// Relationship progress not already represented by the tool call lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelegationStatus {
    Open,
    Completed,
    CancelRequested,
}

impl DelegationStatus {
    #[must_use]
    pub const fn occupies_parallel_slot(self) -> bool {
        matches!(self, Self::Open | Self::CancelRequested)
    }
}

const fn requires_cancellation_delivery(status: DelegationStatus) -> bool {
    matches!(status, DelegationStatus::CancelRequested)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelationshipTransition {
    Complete,
    RequestCancel,
}

/// Heap-free production transition kernel shared by the aggregate and Kani.
/// Returning `Duplicate` is deliberately distinct from applying a transition:
/// adapters may retry, but retries cannot manufacture a second state effect.
fn transition_relationship(
    current: DelegationStatus,
    transition: RelationshipTransition,
) -> Result<(DelegationStatus, TransitionResult), DelegationError> {
    use DelegationStatus::{CancelRequested, Completed, Open};
    use RelationshipTransition::{Complete, RequestCancel};
    match (current, transition) {
        (Open, Complete) => Ok((Completed, TransitionResult::Applied)),
        (Completed, Complete) => Ok((Completed, TransitionResult::Duplicate)),
        (Open, RequestCancel) => Ok((CancelRequested, TransitionResult::Applied)),
        (CancelRequested, RequestCancel) => Ok((current, TransitionResult::Duplicate)),
        _ => Err(DelegationError::InvalidTransition),
    }
}

/// Immutable relationship plus cancellation/completion progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    pub id: DelegationId,
    pub parent_run_id: RunId,
    pub parent_call_id: String,
    pub target_agent_id: String,
    /// This immediate same-Agent edge was admitted by an explicit `self`
    /// sentinel. Legacy relationships and ordinary Agent references are false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recursive_self: bool,
    pub child_run_id: RunId,
    pub depth: u16,
    pub status: DelegationStatus,
    /// Opaque execution reference needed to address an awaiting child outside
    /// this process. The relationship owns it because cancellation must survive
    /// the parent process that first received the remote continuation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_reference: Option<serde_json::Value>,
}

/// Durable instruction for an adapter to cancel one delegated child Run.
/// Delivery is deliberately idempotent: the relationship remains
/// `CancelRequested`, so a recovering process may submit the same instruction
/// again until the remote system reflects cancellation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildRunCancellation {
    pub delegation_id: DelegationId,
    pub child_run_id: RunId,
    pub target_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_reference: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDelegation {
    pub id: DelegationId,
    pub parent_call_id: String,
    pub target_agent_id: String,
    pub recursive_self: bool,
    pub child_run_id: RunId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RelationshipIdentityFacts {
    call_index_matches: bool,
    id_matches: bool,
    parent_call_matches: bool,
    target_matches: bool,
    recursive_self_matches: bool,
    child_run_matches: bool,
    status: DelegationStatus,
}

impl RelationshipIdentityFacts {
    const fn exact(self) -> bool {
        self.call_index_matches
            && self.id_matches
            && self.parent_call_matches
            && self.target_matches
            && self.recursive_self_matches
            && self.child_run_matches
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildInvocationAdmission {
    Admit,
    Missing,
    IdentityConflict,
    Closed,
}

/// Heap-free production admission for the external child-effect boundary.
/// The aggregate projects its exact durable relationship into these facts;
/// only one present, exact, still-open relationship may enter the executor.
const fn child_invocation_admission(
    facts: Option<RelationshipIdentityFacts>,
) -> ChildInvocationAdmission {
    let Some(facts) = facts else {
        return ChildInvocationAdmission::Missing;
    };
    if !facts.exact() {
        ChildInvocationAdmission::IdentityConflict
    } else if !matches!(facts.status, DelegationStatus::Open) {
        ChildInvocationAdmission::Closed
    } else {
        ChildInvocationAdmission::Admit
    }
}

/// Scalar projection of a recovered registry. Every fact here is a number or a
/// single bit, which is what lets the recovery gate below be exhausted
/// mechanically instead of sampled by example.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PersistedRegistryFacts {
    limits: DelegationLimits,
    parent_depth: u16,
    lineage_ends_with_parent: bool,
    total_started: u32,
    delegation_count: usize,
    call_count: usize,
    active_count: usize,
    parent_ended: bool,
    has_open_relationship: bool,
}

/// One recovery defect, each owning exactly one durable message. Keeping the
/// message on the defect rather than at the call site is what makes the gate's
/// outcome a value the proof can compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedStateDefect {
    ZeroLimit,
    ParentDepthLeavesNoChildDepth,
    LineageDoesNotEndWithParent,
    IndexOrTotalMismatch,
    TotalExceedsBudget,
    ActiveExceedsParallelBudget,
    EndedParentRetainsOpenRelationship,
    DepthOverflow,
}

impl PersistedStateDefect {
    const fn message(self) -> &'static str {
        match self {
            Self::ZeroLimit => "delegation limits must be non-zero",
            Self::ParentDepthLeavesNoChildDepth => "parent depth leaves no legal child depth",
            Self::LineageDoesNotEndWithParent => "lineage must end with parent agent",
            Self::IndexOrTotalMismatch => "indexes and total must match relationships",
            Self::TotalExceedsBudget => "started delegation count exceeds its budget",
            Self::ActiveExceedsParallelBudget => {
                "active delegation count exceeds its parallel budget"
            }
            Self::EndedParentRetainsOpenRelationship => {
                "an ended parent cannot retain an open relationship"
            }
            Self::DepthOverflow => "delegation depth overflowed",
        }
    }
}

/// Heap-free recovery kernel shared by the aggregate and Kani. It owns every
/// persisted invariant that ranges over scalars rather than over the persisted
/// collections: non-zero budgets, a parent depth that still leaves a legal
/// child depth, the lineage anchor, index/counter agreement, both budget
/// ceilings, and parent termination. Relationship identity and Agent cycles
/// stay in the aggregate because they range over the collections themselves.
/// Returning the legal child depth makes this the single owner of that
/// arithmetic, so the per-relationship loop cannot recompute it differently.
fn validate_persisted_registry(facts: PersistedRegistryFacts) -> Result<u16, PersistedStateDefect> {
    if facts.limits.max_depth == 0 || facts.limits.max_parallel == 0 || facts.limits.max_total == 0
    {
        return Err(PersistedStateDefect::ZeroLimit);
    }
    if facts.parent_depth >= facts.limits.max_depth {
        return Err(PersistedStateDefect::ParentDepthLeavesNoChildDepth);
    }
    if !facts.lineage_ends_with_parent {
        return Err(PersistedStateDefect::LineageDoesNotEndWithParent);
    }
    if usize::try_from(facts.total_started).ok() != Some(facts.delegation_count)
        || facts.call_count != facts.delegation_count
    {
        return Err(PersistedStateDefect::IndexOrTotalMismatch);
    }
    if facts.total_started > facts.limits.max_total {
        return Err(PersistedStateDefect::TotalExceedsBudget);
    }
    if facts.active_count > usize::from(facts.limits.max_parallel) {
        return Err(PersistedStateDefect::ActiveExceedsParallelBudget);
    }
    if facts.parent_ended && facts.has_open_relationship {
        return Err(PersistedStateDefect::EndedParentRetainsOpenRelationship);
    }
    facts
        .parent_depth
        .checked_add(1)
        .ok_or(PersistedStateDefect::DepthOverflow)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionResult {
    Applied,
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DelegationError {
    #[error("parent Run has ended")]
    ParentEnded,
    #[error("delegation depth {depth} exceeds limit {limit}")]
    DepthExceeded { depth: u16, limit: u16 },
    #[error("delegation depth overflowed")]
    DepthOverflow,
    #[error("delegation would create an Agent cycle through {agent_id}")]
    Cycle { agent_id: String },
    #[error("parallel delegation limit {limit} reached")]
    ParallelLimit { limit: u16 },
    #[error("delegation budget {limit} exhausted")]
    BudgetExhausted { limit: u32 },
    #[error("parent call id already names a different delegation")]
    CallConflict,
    #[error("delegation id already names a different child")]
    IdentityConflict,
    #[error("child Run id already belongs to another delegation")]
    ChildRunConflict,
    #[error("unknown delegation")]
    NotFound,
    #[error("invalid delegation transition")]
    InvalidTransition,
    #[error("persisted delegation registry is invalid: {0}")]
    InvalidPersistedState(String),
}

/// Run-scoped registry committed through ordinary thread state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DelegationRegistry {
    parent_run_id: RunId,
    parent_agent_id: String,
    lineage: Vec<String>,
    parent_depth: u16,
    limits: DelegationLimits,
    parent_ended: bool,
    total_started: u32,
    delegations: BTreeMap<DelegationId, Delegation>,
    calls: BTreeMap<String, DelegationId>,
}

#[derive(Deserialize)]
struct DelegationRegistryWire {
    parent_run_id: RunId,
    parent_agent_id: String,
    lineage: Vec<String>,
    parent_depth: u16,
    limits: DelegationLimits,
    parent_ended: bool,
    total_started: u32,
    delegations: BTreeMap<DelegationId, Delegation>,
    calls: BTreeMap<String, DelegationId>,
}

impl<'de> Deserialize<'de> for DelegationRegistry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = DelegationRegistryWire::deserialize(deserializer)?;
        let registry = Self {
            parent_run_id: wire.parent_run_id,
            parent_agent_id: wire.parent_agent_id,
            lineage: wire.lineage,
            parent_depth: wire.parent_depth,
            limits: wire.limits,
            parent_ended: wire.parent_ended,
            total_started: wire.total_started,
            delegations: wire.delegations,
            calls: wire.calls,
        };
        registry.validate().map_err(D::Error::custom)?;
        Ok(registry)
    }
}

impl DelegationRegistry {
    #[must_use]
    pub fn new(
        parent_run_id: RunId,
        parent_agent_id: impl Into<String>,
        mut lineage: Vec<String>,
        parent_depth: u16,
        limits: DelegationLimits,
    ) -> Self {
        let parent_agent_id = parent_agent_id.into();
        if lineage.last() != Some(&parent_agent_id) {
            lineage.push(parent_agent_id.clone());
        }
        Self {
            parent_run_id,
            parent_agent_id,
            lineage,
            parent_depth,
            limits,
            parent_ended: false,
            total_started: 0,
            delegations: BTreeMap::new(),
            calls: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn parent_run_id(&self) -> &RunId {
        &self.parent_run_id
    }

    #[must_use]
    pub fn get(&self, id: &DelegationId) -> Option<&Delegation> {
        self.delegations.get(id)
    }

    /// Recover the original immutable child creation origin. Callbacks on the
    /// same logical Thread retain its lineage/depth; a later carrying Run is
    /// not a new delegation and must not reset these coordinates.
    pub fn child_origin(&self, id: &DelegationId) -> Result<DelegationOrigin, DelegationError> {
        let child = self.get(id).ok_or_else(|| {
            DelegationError::InvalidPersistedState("child relationship is absent".into())
        })?;
        let origin = DelegationOrigin::nested_for_agent(
            self.parent_run_id.clone(),
            child.parent_call_id.clone(),
            self.parent_depth,
            &self.lineage,
            self.parent_agent_id.clone(),
        )?;
        if child.id != origin.delegation_id
            || child.child_run_id != origin.child_run_id()
            || child.depth != origin.depth
        {
            return Err(DelegationError::InvalidPersistedState(
                "child creation origin differs from its relationship".into(),
            ));
        }
        Ok(origin)
    }

    pub fn delegations(&self) -> impl Iterator<Item = &Delegation> {
        self.delegations.values()
    }

    #[must_use]
    pub fn active_count(&self) -> usize {
        self.delegations
            .values()
            .filter(|delegation| delegation.status.occupies_parallel_slot())
            .count()
    }

    pub fn request(
        &mut self,
        request: RequestDelegation,
    ) -> Result<TransitionResult, DelegationError> {
        if self.parent_ended {
            return Err(DelegationError::ParentEnded);
        }
        if let Some(existing_id) = self.calls.get(&request.parent_call_id) {
            let existing = self.delegations.get(existing_id).ok_or_else(|| {
                DelegationError::InvalidPersistedState(
                    "call index points at a missing delegation".into(),
                )
            })?;
            return if Self::identity_facts(Some(existing_id), Some(existing), &request)
                .is_some_and(RelationshipIdentityFacts::exact)
            {
                Ok(TransitionResult::Duplicate)
            } else {
                Err(DelegationError::CallConflict)
            };
        }
        if self.delegations.contains_key(&request.id) {
            return Err(DelegationError::IdentityConflict);
        }
        if self
            .delegations
            .values()
            .any(|existing| existing.child_run_id == request.child_run_id)
        {
            return Err(DelegationError::ChildRunConflict);
        }
        let creates_cycle = self.lineage.contains(&request.target_agent_id)
            && !(request.recursive_self && self.parent_agent_id == request.target_agent_id);
        let depth = admit_new_delegation(
            self.parent_ended,
            self.parent_depth,
            self.limits,
            creates_cycle,
            self.active_count(),
            self.total_started,
        )
        .map_err(|error| match error {
            AdmissionError::ParentEnded => DelegationError::ParentEnded,
            AdmissionError::DepthOverflow => DelegationError::DepthOverflow,
            AdmissionError::DepthExceeded { depth, limit } => {
                DelegationError::DepthExceeded { depth, limit }
            }
            AdmissionError::Cycle => DelegationError::Cycle {
                agent_id: request.target_agent_id.clone(),
            },
            AdmissionError::ParallelLimit { limit } => DelegationError::ParallelLimit { limit },
            AdmissionError::BudgetExhausted { limit } => DelegationError::BudgetExhausted { limit },
        })?;

        let id = request.id.clone();
        let call_id = request.parent_call_id.clone();
        self.delegations.insert(
            id.clone(),
            Delegation {
                id: request.id,
                parent_run_id: self.parent_run_id.clone(),
                parent_call_id: request.parent_call_id,
                target_agent_id: request.target_agent_id,
                recursive_self: request.recursive_self,
                child_run_id: request.child_run_id,
                depth,
                status: DelegationStatus::Open,
                cancellation_reference: None,
            },
        );
        self.calls.insert(call_id, id);
        self.total_started += 1;
        Ok(TransitionResult::Applied)
    }

    fn identity_facts(
        indexed_id: Option<&DelegationId>,
        relationship: Option<&Delegation>,
        request: &RequestDelegation,
    ) -> Option<RelationshipIdentityFacts> {
        relationship.map(|relationship| RelationshipIdentityFacts {
            call_index_matches: indexed_id == Some(&request.id),
            id_matches: relationship.id == request.id,
            parent_call_matches: relationship.parent_call_id == request.parent_call_id,
            target_matches: relationship.target_agent_id == request.target_agent_id,
            recursive_self_matches: relationship.recursive_self == request.recursive_self,
            child_run_matches: relationship.child_run_id == request.child_run_id,
            status: relationship.status,
        })
    }

    /// Require the exact durable relationship before entering a delegated
    /// child executor. This is a read-only gate: rejection cannot create,
    /// reopen, or otherwise mutate a relationship.
    pub fn admit_child_invocation(
        &self,
        request: &RequestDelegation,
    ) -> Result<(), DelegationError> {
        let relationship = self.delegations.get(&request.id);
        let facts = Self::identity_facts(
            self.calls.get(&request.parent_call_id),
            relationship,
            request,
        );
        match child_invocation_admission(facts) {
            ChildInvocationAdmission::Admit => Ok(()),
            ChildInvocationAdmission::Missing => Err(DelegationError::NotFound),
            ChildInvocationAdmission::IdentityConflict => Err(DelegationError::IdentityConflict),
            ChildInvocationAdmission::Closed => Err(DelegationError::InvalidTransition),
        }
    }

    pub fn complete(&mut self, id: &DelegationId) -> Result<TransitionResult, DelegationError> {
        let delegation = self
            .delegations
            .get_mut(id)
            .ok_or(DelegationError::NotFound)?;
        let (status, result) =
            transition_relationship(delegation.status, RelationshipTransition::Complete)?;
        delegation.status = status;
        if result == TransitionResult::Applied {
            delegation.cancellation_reference = None;
        }
        Ok(result)
    }

    /// Remember how an adapter can address an awaiting child. Repeating the
    /// same reference is idempotent; changing it is legal when a resumed child
    /// reaches a new remote wait boundary.
    pub fn record_cancellation_reference(
        &mut self,
        id: &DelegationId,
        reference: serde_json::Value,
    ) -> Result<TransitionResult, DelegationError> {
        let delegation = self
            .delegations
            .get_mut(id)
            .ok_or(DelegationError::NotFound)?;
        if delegation.status != DelegationStatus::Open {
            return Err(DelegationError::InvalidTransition);
        }
        if delegation.cancellation_reference.as_ref() == Some(&reference) {
            return Ok(TransitionResult::Duplicate);
        }
        delegation.cancellation_reference = Some(reference);
        Ok(TransitionResult::Applied)
    }

    /// Persist cancellation intent before an adapter attempts delivery.
    pub fn end_parent(&mut self) -> Vec<ChildRunCancellation> {
        if self.parent_ended {
            return Vec::new();
        }
        self.parent_ended = true;
        let mut children = Vec::new();
        for delegation in self.delegations.values_mut() {
            let Ok((status, result)) =
                transition_relationship(delegation.status, RelationshipTransition::RequestCancel)
            else {
                continue;
            };
            delegation.status = status;
            if result == TransitionResult::Applied {
                children.push(ChildRunCancellation {
                    delegation_id: delegation.id.clone(),
                    child_run_id: delegation.child_run_id.clone(),
                    target_agent_id: delegation.target_agent_id.clone(),
                    execution_reference: delegation.cancellation_reference.clone(),
                });
            }
        }
        children
    }

    /// Every durable cancellation still requiring idempotent delivery. This is
    /// the recovery view used after a process restart.
    pub fn pending_cancellations(&self) -> impl Iterator<Item = ChildRunCancellation> + '_ {
        self.delegations
            .values()
            .filter(|delegation| requires_cancellation_delivery(delegation.status))
            .map(|delegation| ChildRunCancellation {
                delegation_id: delegation.id.clone(),
                child_run_id: delegation.child_run_id.clone(),
                target_agent_id: delegation.target_agent_id.clone(),
                execution_reference: delegation.cancellation_reference.clone(),
            })
    }

    pub fn validate(&self) -> Result<(), DelegationError> {
        let expected_depth = validate_persisted_registry(PersistedRegistryFacts {
            limits: self.limits,
            parent_depth: self.parent_depth,
            lineage_ends_with_parent: self.lineage.last() == Some(&self.parent_agent_id),
            total_started: self.total_started,
            delegation_count: self.delegations.len(),
            call_count: self.calls.len(),
            active_count: self.active_count(),
            parent_ended: self.parent_ended,
            has_open_relationship: self
                .delegations
                .values()
                .any(|delegation| delegation.status == DelegationStatus::Open),
        })
        .map_err(|defect| DelegationError::InvalidPersistedState(defect.message().into()))?;
        let mut child_runs = std::collections::HashSet::new();
        for (id, delegation) in &self.delegations {
            if id != &delegation.id
                || delegation.parent_run_id != self.parent_run_id
                || self.calls.get(&delegation.parent_call_id) != Some(id)
                || delegation.depth != expected_depth
            {
                return Err(DelegationError::InvalidPersistedState(
                    "relationship identity or call index mismatch".into(),
                ));
            }
            if self.lineage.contains(&delegation.target_agent_id)
                && !(delegation.recursive_self
                    && self.parent_agent_id == delegation.target_agent_id)
            {
                return Err(DelegationError::InvalidPersistedState(
                    "persisted relationship creates an Agent cycle".into(),
                ));
            }
            if !child_runs.insert(&delegation.child_run_id.0) {
                return Err(DelegationError::InvalidPersistedState(
                    "child Run id is shared by multiple relationships".into(),
                ));
            }
            if delegation.status == DelegationStatus::Completed
                && delegation.cancellation_reference.is_some()
            {
                return Err(DelegationError::InvalidPersistedState(
                    "a completed relationship cannot retain a cancellation reference".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(parallel: u16, total: u32) -> DelegationRegistry {
        DelegationRegistry::new(
            RunId("parent".into()),
            "coordinator",
            vec!["root".into()],
            0,
            DelegationLimits::new(3, parallel, total),
        )
    }

    fn request(n: u8, target: &str) -> RequestDelegation {
        RequestDelegation {
            id: DelegationId(format!("d{n}")),
            parent_call_id: format!("c{n}"),
            target_agent_id: target.into(),
            recursive_self: false,
            child_run_id: RunId(format!("r{n}")),
        }
    }

    fn invalid(message: &str) -> DelegationError {
        DelegationError::InvalidPersistedState(message.into())
    }

    /// Cause/effect graph over the production transition kernel, in the exact
    /// shape the Kani proof exhausts. `cargo test` cannot run that proof, so
    /// this table is what keeps a deleted or widened row out of a release: a
    /// stuttering retry must still report its successor, because the aggregate
    /// distinguishes `Duplicate` from a rejection when it decides whether a
    /// cancellation command is newly owed.
    ///
    /// | rule | status | transition | outcome |
    /// |---|---|---|---|
    /// | T1 | Open | Complete | Ok(Completed, Applied) |
    /// | T2 | Open | RequestCancel | Ok(CancelRequested, Applied) |
    /// | T3 | Completed | Complete | Ok(Completed, Duplicate) |
    /// | T4 | CancelRequested | RequestCancel | Ok(CancelRequested, Duplicate) |
    /// | T5 | Completed | RequestCancel | Err(InvalidTransition) |
    /// | T6 | CancelRequested | Complete | Err(InvalidTransition) |
    #[test]
    fn relationship_transition_table_is_total() {
        use DelegationStatus::{CancelRequested, Completed, Open};
        use RelationshipTransition::{Complete, RequestCancel};
        use TransitionResult::{Applied, Duplicate};

        let table = [
            ("T1", Open, Complete, Ok((Completed, Applied))),
            ("T2", Open, RequestCancel, Ok((CancelRequested, Applied))),
            ("T3", Completed, Complete, Ok((Completed, Duplicate))),
            (
                "T4",
                CancelRequested,
                RequestCancel,
                Ok((CancelRequested, Duplicate)),
            ),
            (
                "T5",
                Completed,
                RequestCancel,
                Err(DelegationError::InvalidTransition),
            ),
            (
                "T6",
                CancelRequested,
                Complete,
                Err(DelegationError::InvalidTransition),
            ),
        ];
        for (rule, status, transition, expected) in table {
            assert_eq!(
                transition_relationship(status, transition),
                expected,
                "{rule}"
            );
        }
    }

    #[test]
    fn relationship_request_is_idempotent_and_budgeted() {
        let mut registry = registry(1, 2);
        assert_eq!(
            registry.request(request(1, "researcher")),
            Ok(TransitionResult::Applied)
        );
        assert_eq!(
            registry.request(request(1, "researcher")),
            Ok(TransitionResult::Duplicate)
        );
        assert_eq!(
            registry.request(request(2, "writer")),
            Err(DelegationError::ParallelLimit { limit: 1 })
        );
        registry.complete(&DelegationId("d1".into())).unwrap();
        registry.request(request(2, "writer")).unwrap();
        registry.complete(&DelegationId("d2".into())).unwrap();
        assert_eq!(
            registry.request(request(3, "reviewer")),
            Err(DelegationError::BudgetExhausted { limit: 2 })
        );
        registry.validate().unwrap();
    }

    #[test]
    fn lineage_cycle_is_rejected() {
        let mut registry = registry(2, 2);
        assert_eq!(
            registry.request(request(1, "root")),
            Err(DelegationError::Cycle {
                agent_id: "root".into()
            })
        );
    }

    /// Cause/effect graph: target is already in lineage -> ordinary edge is a
    /// cycle; the exact immediate parent plus an explicit published `self` bit is
    /// a recursive copy. The exemption must not legalize an ancestor cycle.
    ///
    /// | rule | target | recursive_self | relation to parent | result |
    /// |---|---|---|---|---|
    /// | S1 | lineage member | false | any | Cycle |
    /// | S2 | coordinator | true | immediate parent | admitted + validates |
    /// | S3 | root | true | ancestor, not parent | Cycle |
    #[test]
    fn explicit_self_exempts_only_the_immediate_same_agent_edge() {
        let mut direct = registry(2, 2);
        let mut self_request = request(1, "coordinator");
        self_request.recursive_self = true;
        assert_eq!(
            direct.request(self_request),
            Ok(TransitionResult::Applied),
            "S2"
        );
        direct.validate().expect("S2 persists as a valid relation");

        let mut ancestor = DelegationRegistry::new(
            RunId("parent".into()),
            "researcher",
            vec!["root".into()],
            1,
            DelegationLimits::new(3, 2, 2),
        );
        let mut ancestor_request = request(1, "root");
        ancestor_request.recursive_self = true;
        assert_eq!(
            ancestor.request(ancestor_request),
            Err(DelegationError::Cycle {
                agent_id: "root".into()
            }),
            "S3"
        );
    }

    #[test]
    fn parent_end_persists_idempotent_cancellation_intent() {
        let mut registry = registry(2, 2);
        registry.request(request(1, "researcher")).unwrap();
        assert_eq!(
            registry.end_parent(),
            vec![ChildRunCancellation {
                delegation_id: DelegationId("d1".into()),
                child_run_id: RunId("r1".into()),
                target_agent_id: "researcher".into(),
                execution_reference: None,
            }]
        );
        assert_eq!(
            registry.get(&DelegationId("d1".into())).unwrap().status,
            DelegationStatus::CancelRequested
        );
        assert!(registry.end_parent().is_empty());
    }

    /// Cause/effect graph: relationship identity is the conjunction of parent
    /// call, delegation id, target, recursive-self bit, and child Run id. The
    /// first accepted request freezes that tuple; every replay or conflict must
    /// leave the complete registry unchanged. Parent termination then projects
    /// the same tuple plus its execution reference as the durable cancellation
    /// command, including after serde reconstruction.
    ///
    /// | rule | existing call | exact tuple | reused delegation id | reused child | relationship state | effect |
    /// |---|---|---|---|---|---|---|
    /// | I1 | yes | yes | yes | yes | Open | Duplicate; registry stutters |
    /// | I2 | yes | no | any | any | Open | CallConflict; registry stutters |
    /// | I3 | no | no | yes | no | Open | IdentityConflict; registry stutters |
    /// | I4 | no | no | no | yes | Open | ChildRunConflict; registry stutters |
    /// | C1 | yes | yes | yes | yes | Open + reference, then parent ends | exact command survives cold reload and repeated projection |
    /// | C2 | yes | yes | yes | yes | Completed before parent ends | reference cleared; no cancellation projected |
    #[test]
    fn relationship_identity_and_cancel_redelivery_share_exact_durable_binding() {
        let parent = RunId("a".into());
        let call = "b:c";
        let origin = DelegationOrigin::root_for_agent(parent.clone(), call, "coordinator");
        let delimiter_peer =
            DelegationOrigin::root_for_agent(RunId("a:b".into()), "c", "coordinator");
        assert_ne!(
            origin.delegation_id, delimiter_peer.delegation_id,
            "length framing"
        );
        assert_ne!(
            origin.child_run_id(),
            delimiter_peer.child_run_id(),
            "length framing"
        );
        assert_eq!(
            origin,
            DelegationOrigin::root_for_agent(parent.clone(), call, "coordinator"),
            "stable reconstruction"
        );

        let accepted = RequestDelegation {
            id: origin.delegation_id.clone(),
            parent_call_id: call.into(),
            target_agent_id: "researcher".into(),
            recursive_self: false,
            child_run_id: origin.child_run_id(),
        };
        let mut authoritative = DelegationRegistry::new(
            parent,
            "coordinator",
            Vec::new(),
            0,
            DelegationLimits::new(3, 4, 4),
        );
        assert_eq!(
            authoritative.request(accepted.clone()),
            Ok(TransitionResult::Applied)
        );

        let mut conflicts = Vec::new();
        let mut changed_id = accepted.clone();
        changed_id.id = DelegationId("another-delegation".into());
        conflicts.push(("I2/id", changed_id, DelegationError::CallConflict));
        let mut changed_target = accepted.clone();
        changed_target.target_agent_id = "writer".into();
        conflicts.push(("I2/target", changed_target, DelegationError::CallConflict));
        let mut changed_recursion = accepted.clone();
        changed_recursion.recursive_self = true;
        conflicts.push((
            "I2/recursive-self",
            changed_recursion,
            DelegationError::CallConflict,
        ));
        let mut changed_child = accepted.clone();
        changed_child.child_run_id = RunId("another-child".into());
        conflicts.push(("I2/child", changed_child, DelegationError::CallConflict));
        let mut reused_id = accepted.clone();
        reused_id.parent_call_id = "another-call".into();
        reused_id.child_run_id = RunId("fresh-child".into());
        conflicts.push(("I3", reused_id, DelegationError::IdentityConflict));
        let mut reused_child = accepted.clone();
        reused_child.id = DelegationId("fresh-delegation".into());
        reused_child.parent_call_id = "fresh-call".into();
        conflicts.push(("I4", reused_child, DelegationError::ChildRunConflict));

        let frozen = serde_json::to_value(&authoritative).expect("freeze authoritative registry");
        assert_eq!(
            authoritative.request(accepted.clone()),
            Ok(TransitionResult::Duplicate),
            "I1"
        );
        assert_eq!(
            serde_json::to_value(&authoritative).unwrap(),
            frozen,
            "I1 stutter"
        );
        for (rule, conflict, error) in conflicts {
            assert_eq!(authoritative.request(conflict), Err(error), "{rule}");
            assert_eq!(
                serde_json::to_value(&authoritative).unwrap(),
                frozen,
                "{rule} stutter"
            );
        }

        let execution_reference = serde_json::json!({
            "protocol": "a2a",
            "task_id": "remote-task:7",
            "context_id": "remote-context",
        });
        authoritative
            .record_cancellation_reference(&accepted.id, execution_reference.clone())
            .expect("record adapter cancellation identity");
        let expected = ChildRunCancellation {
            delegation_id: accepted.id.clone(),
            child_run_id: accepted.child_run_id.clone(),
            target_agent_id: accepted.target_agent_id.clone(),
            execution_reference: Some(execution_reference),
        };
        assert_eq!(authoritative.end_parent(), vec![expected.clone()], "C1");
        let bytes = serde_json::to_vec(&authoritative).expect("serialize ended registry");
        let mut recovered: DelegationRegistry =
            serde_json::from_slice(&bytes).expect("validate cold registry reconstruction");
        assert_eq!(
            recovered.pending_cancellations().collect::<Vec<_>>(),
            vec![expected.clone()],
            "C1 first recovery projection"
        );
        assert_eq!(
            recovered.pending_cancellations().collect::<Vec<_>>(),
            vec![expected],
            "C1 repeated recovery projection"
        );
        assert!(
            recovered.end_parent().is_empty(),
            "C1 termination replay stutters"
        );

        let mut completed = registry(2, 2);
        let settled = request(1, "researcher");
        completed.request(settled.clone()).unwrap();
        completed
            .record_cancellation_reference(&settled.id, serde_json::json!({"task": "done"}))
            .unwrap();
        completed.complete(&settled.id).unwrap();
        assert!(completed.end_parent().is_empty(), "C2");
        assert_eq!(completed.pending_cancellations().count(), 0, "C2");
        assert_eq!(
            completed.get(&settled.id).unwrap().cancellation_reference,
            None,
            "C2 completed relationships cannot retain adapter identity"
        );
    }

    #[test]
    fn child_invocation_admission_consumes_the_exact_open_relationship() {
        // Cause/effect decision table: I1 no relationship -> NotFound; I2 one
        // exact Open tuple -> admit; I3-I7 any id/call/target/self/
        // child mismatch -> conflict or missing; I9 exact Completed and I10
        // exact CancelRequested -> InvalidTransition. The gate is read-only, so
        // every row must preserve the complete serialized registry.
        let exact = request(1, "researcher");
        let empty = registry(2, 2);
        let empty_before = serde_json::to_value(&empty).unwrap();
        assert_eq!(
            empty.admit_child_invocation(&exact),
            Err(DelegationError::NotFound),
            "I1"
        );
        assert_eq!(serde_json::to_value(&empty).unwrap(), empty_before, "I1");

        let mut open = registry(2, 2);
        open.request(exact.clone()).unwrap();
        assert_eq!(open.admit_child_invocation(&exact), Ok(()), "I2");
        let open_before = serde_json::to_value(&open).unwrap();
        let mut mismatches = Vec::new();
        let mut changed_id = exact.clone();
        changed_id.id = DelegationId("other-id".into());
        mismatches.push(("I3", changed_id, DelegationError::NotFound));
        let mut changed_call = exact.clone();
        changed_call.parent_call_id = "other-call".into();
        mismatches.push(("I4", changed_call, DelegationError::IdentityConflict));
        let mut changed_target = exact.clone();
        changed_target.target_agent_id = "writer".into();
        mismatches.push(("I5", changed_target, DelegationError::IdentityConflict));
        let mut changed_self = exact.clone();
        changed_self.recursive_self = true;
        mismatches.push(("I6", changed_self, DelegationError::IdentityConflict));
        let mut changed_child = exact.clone();
        changed_child.child_run_id = RunId("other-child".into());
        mismatches.push(("I7", changed_child, DelegationError::IdentityConflict));
        for (rule, request, expected) in mismatches {
            assert_eq!(
                open.admit_child_invocation(&request),
                Err(expected),
                "{rule}"
            );
            assert_eq!(serde_json::to_value(&open).unwrap(), open_before, "{rule}");
        }

        let mut completed = open.clone();
        completed.complete(&exact.id).unwrap();
        let completed_before = serde_json::to_value(&completed).unwrap();
        assert_eq!(
            completed.admit_child_invocation(&exact),
            Err(DelegationError::InvalidTransition),
            "I9"
        );
        assert_eq!(
            serde_json::to_value(&completed).unwrap(),
            completed_before,
            "I9"
        );

        let mut cancelled = open;
        assert_eq!(cancelled.end_parent().len(), 1);
        let cancelled_before = serde_json::to_value(&cancelled).unwrap();
        assert_eq!(
            cancelled.admit_child_invocation(&exact),
            Err(DelegationError::InvalidTransition),
            "I10"
        );
        assert_eq!(
            serde_json::to_value(&cancelled).unwrap(),
            cancelled_before,
            "I10"
        );
    }

    #[test]
    fn origin_identity_is_stable_and_nested_depth_survives() {
        let root = DelegationOrigin::root_for_agent(RunId("p".into()), "c", "root");
        assert_eq!(root.child_run_id(), root.child_run_id());
        let nested = DelegationOrigin::nested_for_agent(
            RunId("child".into()),
            "next",
            root.depth,
            &root.agent_lineage,
            "researcher",
        )
        .unwrap();
        assert_eq!(nested.depth, 2);
        assert_eq!(nested.agent_lineage, vec!["root", "researcher"]);
    }

    /// Cause/effect graph over nested lineage carry-forward. The lineage is the
    /// ancestor chain every later cycle check reads, so exactly one identity may
    /// be appended and only when the caller actually names one: an anonymous
    /// caller that contributed an empty entry would both break the
    /// `lineage.last() == parent_agent_id` recovery anchor and make every
    /// empty-named target look like an ancestor.
    ///
    /// | rule | parent agent id | lineage tail | effect |
    /// |---|---|---|---|
    /// | N1 | named, differs from tail | appended once |
    /// | N2 | named, equals tail | stuttering re-entry is not appended |
    /// | N3 | empty (identity-free caller) | lineage carried forward unchanged |
    #[test]
    fn nested_lineage_appends_only_a_named_new_parent() {
        let ancestors = vec!["root".to_owned(), "coordinator".to_owned()];
        let named =
            DelegationOrigin::nested_for_agent(RunId("p".into()), "c", 1, &ancestors, "researcher")
                .expect("N1");
        assert_eq!(
            named.agent_lineage,
            vec!["root", "coordinator", "researcher"],
            "N1"
        );

        let repeated = DelegationOrigin::nested_for_agent(
            RunId("p".into()),
            "c",
            1,
            &ancestors,
            "coordinator",
        )
        .expect("N2");
        assert_eq!(repeated.agent_lineage, ancestors, "N2");

        let anonymous = DelegationOrigin::nested_for_agent(
            RunId("p".into()),
            "c",
            1,
            &ancestors,
            String::new(),
        )
        .expect("N3");
        assert_eq!(anonymous.agent_lineage, ancestors, "N3");
        assert_eq!(
            DelegationOrigin::nested(RunId("p".into()), "c", 1)
                .expect("N3 compatibility constructor")
                .agent_lineage,
            Vec::<String>::new(),
            "N3 an identity-free root cannot seed an empty ancestor"
        );
    }

    /// One recovered registry that satisfies every persisted invariant, so each
    /// recovery test below can violate exactly one of them and attribute the
    /// rejection to that cause alone.
    fn recovered() -> DelegationRegistry {
        let mut recovered = registry(2, 4);
        recovered.request(request(1, "researcher")).unwrap();
        recovered
            .validate()
            .expect("the recovery fixture must itself be valid");
        recovered
    }

    fn sole_relationship(registry: &mut DelegationRegistry) -> &mut Delegation {
        registry
            .delegations
            .get_mut(&DelegationId("d1".into()))
            .expect("recovery fixture holds exactly one relationship")
    }

    /// Add a second consistent relationship so a counting invariant can be
    /// violated without disturbing the index agreement the gate checks first.
    fn add_second_relationship(registry: &mut DelegationRegistry, status: DelegationStatus) {
        registry.delegations.insert(
            DelegationId("d2".into()),
            Delegation {
                id: DelegationId("d2".into()),
                parent_run_id: RunId("parent".into()),
                parent_call_id: "c2".into(),
                target_agent_id: "writer".into(),
                recursive_self: false,
                child_run_id: RunId("r2".into()),
                depth: 1,
                status,
                cancellation_reference: None,
            },
        );
        registry
            .calls
            .insert("c2".into(), DelegationId("d2".into()));
        registry.total_started = 2;
    }

    /// Cause/effect graph over the recovery budget gate. The three limits are
    /// independent causes: any one of them at zero makes the persisted state
    /// unrecoverable, because a zero budget can never have admitted the
    /// relationships the state already claims. A conjunction here would let a
    /// registry with two live budgets and one dead one load.
    ///
    /// | rule | max_depth | max_parallel | max_total | effect |
    /// |---|---|---|---|---|
    /// | Z1 | 0 | live | live | rejected: limits must be non-zero |
    /// | Z2 | live | 0 | live | rejected: limits must be non-zero |
    /// | Z3 | live | live | 0 | rejected: limits must be non-zero |
    /// | Z4 | live | live | live | accepted |
    #[test]
    fn each_zero_budget_alone_rejects_a_recovered_registry() {
        for (rule, limits) in [
            ("Z1", DelegationLimits::new(0, 2, 4)),
            ("Z2", DelegationLimits::new(3, 0, 4)),
            ("Z3", DelegationLimits::new(3, 2, 0)),
        ] {
            let mut zeroed = recovered();
            zeroed.limits = limits;
            assert_eq!(
                zeroed.validate(),
                Err(invalid("delegation limits must be non-zero")),
                "{rule}"
            );
        }
        recovered().validate().expect("Z4");
    }

    /// Cause/effect graph over the remaining scalar recovery causes. Each row
    /// violates exactly one of them, so no row can be reported by a neighbour's
    /// message.
    ///
    /// | rule | violated cause | effect |
    /// |---|---|---|
    /// | S1 | parent depth already consumed the depth budget | rejected: no legal child depth |
    /// | S2 | lineage does not end at the parent Agent | rejected: lineage anchor |
    /// | S3 | started counter disagrees with the relationships | rejected: indexes and total |
    /// | S4 | call index size disagrees with the relationships | rejected: indexes and total |
    /// | S5 | started counter exceeds its budget | rejected: budget |
    /// | S6 | active count exceeds the parallel budget | rejected: parallel budget |
    /// | S7 | an ended parent still holds an Open relationship | rejected: ended parent |
    #[test]
    fn each_scalar_recovery_cause_alone_rejects_a_recovered_registry() {
        let mut deep = recovered();
        deep.parent_depth = 2;
        sole_relationship(&mut deep).depth = 3;
        deep.validate()
            .expect("S1 boundary: the last parent depth below the limit still has a child depth");
        deep.parent_depth = 3;
        assert_eq!(
            deep.validate(),
            Err(invalid("parent depth leaves no legal child depth")),
            "S1"
        );

        let mut unanchored = recovered();
        unanchored.lineage = vec!["root".into()];
        assert_eq!(
            unanchored.validate(),
            Err(invalid("lineage must end with parent agent")),
            "S2"
        );

        let mut inflated_counter = recovered();
        inflated_counter.total_started = 2;
        assert_eq!(
            inflated_counter.validate(),
            Err(invalid("indexes and total must match relationships")),
            "S3"
        );

        let mut stale_index = recovered();
        stale_index
            .calls
            .insert("c2".into(), DelegationId("d2".into()));
        assert_eq!(
            stale_index.validate(),
            Err(invalid("indexes and total must match relationships")),
            "S4"
        );

        let mut overspent = recovered();
        add_second_relationship(&mut overspent, DelegationStatus::Completed);
        overspent.limits.max_total = 2;
        overspent
            .validate()
            .expect("S5 boundary: a budget of two exactly covers two started relationships");
        overspent.limits.max_total = 1;
        assert_eq!(
            overspent.validate(),
            Err(invalid("started delegation count exceeds its budget")),
            "S5"
        );

        let mut crowded = recovered();
        add_second_relationship(&mut crowded, DelegationStatus::Open);
        crowded
            .validate()
            .expect("S6 boundary: two open relationships exactly fill a parallel budget of two");
        crowded.limits.max_parallel = 1;
        assert_eq!(
            crowded.validate(),
            Err(invalid(
                "active delegation count exceeds its parallel budget"
            )),
            "S6"
        );

        let mut ended = recovered();
        ended.parent_ended = true;
        assert_eq!(
            ended.validate(),
            Err(invalid(
                "an ended parent cannot retain an open relationship"
            )),
            "S7"
        );
    }

    /// Cause/effect graph over the per-relationship identity chain. Recovery
    /// must re-derive every fact the aggregate froze at admission time, and the
    /// four facts are independent causes: a conjunction would let a registry
    /// load whose call index points at a relationship it does not name.
    ///
    /// | rule | violated fact | effect |
    /// |---|---|---|
    /// | Y1 | map key is not the relationship id | rejected: identity or call index |
    /// | Y2 | relationship names another parent Run | rejected: identity or call index |
    /// | Y3 | call index points at another relationship | rejected: identity or call index |
    /// | Y4 | relationship depth is not the one legal child depth | rejected: identity or call index |
    /// | Y5 | every fact re-derives | accepted |
    #[test]
    fn each_relationship_identity_fact_alone_rejects_a_recovered_registry() {
        let mismatch = Err(invalid("relationship identity or call index mismatch"));

        let mut rekeyed = recovered();
        let relationship = rekeyed
            .delegations
            .remove(&DelegationId("d1".into()))
            .expect("recovery fixture holds exactly one relationship");
        rekeyed
            .delegations
            .insert(DelegationId("d9".into()), relationship);
        rekeyed.calls.insert("c1".into(), DelegationId("d9".into()));
        assert_eq!(rekeyed.validate(), mismatch, "Y1");

        let mut adopted = recovered();
        sole_relationship(&mut adopted).parent_run_id = RunId("another-parent".into());
        assert_eq!(adopted.validate(), mismatch, "Y2");

        let mut misindexed = recovered();
        misindexed
            .calls
            .insert("c1".into(), DelegationId("d9".into()));
        assert_eq!(misindexed.validate(), mismatch, "Y3");

        let mut misdepthed = recovered();
        sole_relationship(&mut misdepthed).depth = 2;
        assert_eq!(misdepthed.validate(), mismatch, "Y4");

        recovered().validate().expect("Y5");
    }

    /// Cause/effect graph over the recovery cycle gate. The exemption is the
    /// conjunction of an explicit published `self` bit and the target being the
    /// immediate parent Agent; either half on its own must still be a cycle,
    /// because a recovered ancestor edge would otherwise re-enter the lineage
    /// forever. Widening the conjunction to a disjunction is the difference
    /// between a recursive copy and an unbounded delegation loop surviving a
    /// restart.
    ///
    /// | rule | target in lineage | recursive_self | target is parent Agent | effect |
    /// |---|---|---|---|---|
    /// | R1 | yes | true | yes | accepted: immediate recursive copy |
    /// | R2 | yes | true | no (ancestor) | rejected: Agent cycle |
    /// | R3 | yes | false | yes | rejected: Agent cycle |
    /// | R4 | no | false | no | accepted |
    #[test]
    fn only_the_immediate_self_edge_is_exempt_from_recovery_cycle_detection() {
        let cycle = Err(invalid("persisted relationship creates an Agent cycle"));

        let mut recursive_copy = recovered();
        let relationship = sole_relationship(&mut recursive_copy);
        relationship.target_agent_id = "coordinator".into();
        relationship.recursive_self = true;
        recursive_copy.validate().expect("R1");

        let mut ancestor = recovered();
        let relationship = sole_relationship(&mut ancestor);
        relationship.target_agent_id = "root".into();
        relationship.recursive_self = true;
        assert_eq!(ancestor.validate(), cycle, "R2");

        let mut unpublished_self = recovered();
        sole_relationship(&mut unpublished_self).target_agent_id = "coordinator".into();
        assert_eq!(unpublished_self.validate(), cycle, "R3");

        recovered().validate().expect("R4");
    }

    #[test]
    fn persisted_registry_with_a_broken_call_index_is_rejected_at_decode() {
        let mut registry = registry(2, 2);
        registry.request(request(1, "researcher")).unwrap();
        let mut value = serde_json::to_value(registry).unwrap();
        value["calls"] = serde_json::json!({"c1": "missing"});
        assert!(serde_json::from_value::<DelegationRegistry>(value).is_err());
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_status(tag: u8) -> DelegationStatus {
        match tag % 3 {
            0 => DelegationStatus::Open,
            1 => DelegationStatus::Completed,
            _ => DelegationStatus::CancelRequested,
        }
    }

    fn symbolic_transition(tag: u8) -> RelationshipTransition {
        match tag % 2 {
            0 => RelationshipTransition::Complete,
            _ => RelationshipTransition::RequestCancel,
        }
    }

    /// @obligation delegation.depth_cycle_parallel_total
    #[kani::proof]
    fn only_unsettled_relationships_occupy_a_parallel_slot() {
        let status = symbolic_status(kani::any());
        assert_eq!(
            status.occupies_parallel_slot(),
            matches!(
                status,
                DelegationStatus::Open | DelegationStatus::CancelRequested
            )
        );
    }

    /// @obligation delegation.relationship_transition
    #[kani::proof]
    fn every_relationship_effect_has_one_documented_precondition() {
        // Cause/effect decision table over the production transition kernel.
        // The assertion is deliberately total rather than conditional on a
        // successful result: every (status, transition) pair names its exact
        // successor and effect, so a deleted row cannot pass by degrading into
        // a rejection that no rule documents. Only the two crossed terminal
        // rows may reject.
        //
        // | rule | status | transition | outcome |
        // |---|---|---|---|
        // | T1 | Open | Complete | Ok(Completed, Applied) |
        // | T2 | Open | RequestCancel | Ok(CancelRequested, Applied) |
        // | T3 | Completed | Complete | Ok(Completed, Duplicate) |
        // | T4 | CancelRequested | RequestCancel | Ok(CancelRequested, Duplicate) |
        // | T5 | Completed | RequestCancel | Err(InvalidTransition) |
        // | T6 | CancelRequested | Complete | Err(InvalidTransition) |
        //
        // The successor is compared through `Result::ok` rather than against a
        // whole `DelegationError`: that error carries a `String` variant whose
        // derived equality the model checker would have to unwind, and the
        // rejected rows are pinned by their variant instead.
        let status = symbolic_status(kani::any());
        let transition = symbolic_transition(kani::any());
        let expected = match (status, transition) {
            (DelegationStatus::Open, RelationshipTransition::Complete) => {
                Some((DelegationStatus::Completed, TransitionResult::Applied))
            }
            (DelegationStatus::Open, RelationshipTransition::RequestCancel) => {
                Some((DelegationStatus::CancelRequested, TransitionResult::Applied))
            }
            (DelegationStatus::Completed, RelationshipTransition::Complete)
            | (DelegationStatus::CancelRequested, RelationshipTransition::RequestCancel) => {
                Some((status, TransitionResult::Duplicate))
            }
            _ => None,
        };
        let result = transition_relationship(status, transition);
        assert_eq!(result.as_ref().ok().copied(), expected);
        assert!(
            expected.is_some() || matches!(result, Err(DelegationError::InvalidTransition)),
            "a rejected row must reject with the one documented error"
        );
    }

    /// @obligation delegation.depth_cycle_parallel_total
    #[kani::proof]
    fn delegation_admission_requires_every_budget_and_lineage_guard() {
        let parent_ended: bool = kani::any();
        let parent_depth: u16 = kani::any();
        let limits = DelegationLimits::new(kani::any(), kani::any(), kani::any());
        let creates_cycle: bool = kani::any();
        let active_count: usize = kani::any();
        let total_started: u32 = kani::any();
        let result = admit_new_delegation(
            parent_ended,
            parent_depth,
            limits,
            creates_cycle,
            active_count,
            total_started,
        );
        let expected = !parent_ended
            && parent_depth
                .checked_add(1)
                .is_some_and(|depth| depth <= limits.max_depth)
            && !creates_cycle
            && active_count < usize::from(limits.max_parallel)
            && total_started < limits.max_total;
        assert_eq!(result.is_ok(), expected);
        if let Ok(depth) = result {
            assert_eq!(Some(depth), parent_depth.checked_add(1));
        }
    }

    /// @obligation delegation.depth_cycle_parallel_total
    #[kani::proof]
    fn recovered_registry_requires_every_scalar_invariant() {
        // Cause/effect decision table over the production recovery kernel. Each
        // cause is independent, so the acceptance predicate is written as the
        // conjunction of all of them rather than as a restatement of the
        // implementation's early returns; every defect additionally has to name
        // a cause that is actually violated.
        //
        // | cause | violated when | defect |
        // |---|---|---|
        // | V1 | any of the three budgets is zero | ZeroLimit |
        // | V2 | parent depth leaves no legal child depth | ParentDepthLeavesNoChildDepth |
        // | V3 | lineage does not end at the parent Agent | LineageDoesNotEndWithParent |
        // | V4 | counter or call index disagrees with the relationships | IndexOrTotalMismatch |
        // | V5 | started count exceeds its budget | TotalExceedsBudget |
        // | V6 | active count exceeds the parallel budget | ActiveExceedsParallelBudget |
        // | V7 | an ended parent still holds an Open relationship | EndedParentRetainsOpenRelationship |
        //
        // Effect: accepting exactly when no cause is violated, and then
        // returning the one legal child depth.
        let facts = PersistedRegistryFacts {
            limits: DelegationLimits::new(kani::any(), kani::any(), kani::any()),
            parent_depth: kani::any(),
            lineage_ends_with_parent: kani::any(),
            total_started: kani::any(),
            delegation_count: kani::any(),
            call_count: kani::any(),
            active_count: kani::any(),
            parent_ended: kani::any(),
            has_open_relationship: kani::any(),
        };
        let budgets_are_live = facts.limits.max_depth != 0
            && facts.limits.max_parallel != 0
            && facts.limits.max_total != 0;
        let depth_is_legal = facts.parent_depth < facts.limits.max_depth;
        let indexes_agree = usize::try_from(facts.total_started).ok()
            == Some(facts.delegation_count)
            && facts.call_count == facts.delegation_count;
        let budgets_hold = facts.total_started <= facts.limits.max_total
            && facts.active_count <= usize::from(facts.limits.max_parallel);
        let termination_holds = !(facts.parent_ended && facts.has_open_relationship);
        let recoverable = budgets_are_live
            && depth_is_legal
            && facts.lineage_ends_with_parent
            && indexes_agree
            && budgets_hold
            && termination_holds;

        let result = validate_persisted_registry(facts);
        assert_eq!(result.is_ok(), recoverable);
        match result {
            Ok(depth) => assert_eq!(Some(depth), facts.parent_depth.checked_add(1)),
            Err(defect) => assert!(match defect {
                PersistedStateDefect::ZeroLimit => !budgets_are_live,
                PersistedStateDefect::ParentDepthLeavesNoChildDepth => !depth_is_legal,
                PersistedStateDefect::LineageDoesNotEndWithParent =>
                    !facts.lineage_ends_with_parent,
                PersistedStateDefect::IndexOrTotalMismatch => !indexes_agree,
                PersistedStateDefect::TotalExceedsBudget =>
                    facts.total_started > facts.limits.max_total,
                PersistedStateDefect::ActiveExceedsParallelBudget =>
                    facts.active_count > usize::from(facts.limits.max_parallel),
                PersistedStateDefect::EndedParentRetainsOpenRelationship => !termination_holds,
                // A legal parent depth is strictly below `max_depth`, so the
                // successor can never overflow. The defensive arm stays because
                // the kernel owns the arithmetic; the proof pins it unreachable.
                PersistedStateDefect::DepthOverflow => false,
            }),
        }
    }

    /// @obligation delegation.relationship_identity
    #[kani::proof]
    fn child_invocation_requires_one_exact_open_relationship() {
        // Cause/effect decision table over the production executor gate:
        // absent relationship -> Missing; any call/id/target/self/child mismatch
        // -> IdentityConflict; an exact Completed/CancelRequested relationship
        // -> Closed; only the exact Open row -> Admit. No rejecting row can
        // create or reopen a relationship because the gate is read-only.
        let present: bool = kani::any();
        let facts = present.then(|| RelationshipIdentityFacts {
            call_index_matches: kani::any(),
            id_matches: kani::any(),
            parent_call_matches: kani::any(),
            target_matches: kani::any(),
            recursive_self_matches: kani::any(),
            child_run_matches: kani::any(),
            status: symbolic_status(kani::any()),
        });
        let result = child_invocation_admission(facts);
        let expected = facts
            .is_some_and(|facts| facts.exact() && matches!(facts.status, DelegationStatus::Open));
        assert_eq!(result == ChildInvocationAdmission::Admit, expected);
        if !present {
            assert_eq!(result, ChildInvocationAdmission::Missing);
        } else if facts.is_some_and(|facts| !facts.exact()) {
            assert_eq!(result, ChildInvocationAdmission::IdentityConflict);
        } else if facts
            .is_some_and(|facts| facts.exact() && !matches!(facts.status, DelegationStatus::Open))
        {
            assert_eq!(result, ChildInvocationAdmission::Closed);
        }
    }

    /// @obligation delegation.cancel_intent
    #[kani::proof]
    fn cancellation_delivery_is_enabled_only_by_durable_intent() {
        // Cause/effect decision table over the production relationship kernel:
        // Open+RequestCancel applies CancelRequested and newly enables delivery;
        // CancelRequested+RequestCancel is a stuttering retry that keeps delivery
        // enabled; Complete reaches/stutters at Completed and never enables it;
        // the two crossed terminal transitions reject without a successor.
        let status = symbolic_status(kani::any());
        let transition = symbolic_transition(kani::any());
        let before = requires_cancellation_delivery(status);
        let result = transition_relationship(status, transition);
        let after = result
            .as_ref()
            .is_ok_and(|(next, _)| requires_cancellation_delivery(*next));
        assert_eq!(
            !before && after,
            status == DelegationStatus::Open
                && transition == RelationshipTransition::RequestCancel
                && result
                    .as_ref()
                    .is_ok_and(|(_, effect)| { *effect == TransitionResult::Applied })
        );
        if transition == RelationshipTransition::Complete {
            assert!(!after);
        }
        if let Ok((next, TransitionResult::Duplicate)) = result {
            assert_eq!(next, status);
            assert_eq!(requires_cancellation_delivery(next), before);
        }
    }
}
