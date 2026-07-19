//! Durable parent/child Run relationships for Agent delegation.
//!
//! A delegated Agent is a normal child Run. This module owns only the durable
//! relationship between the parent and those child Runs: identity, limits,
//! cancellation propagation, and exactly-once result delivery. It deliberately
//! does not duplicate a child's `RunState`; local children read that state from
//! committed Run facts and remote children are projected through the same
//! delegation result vocabulary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::agent::run::Id as RunId;

/// Stable identity of one parent tool call's delegated child Run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DelegationId(pub String);

impl DelegationId {
    /// Derive the stable relationship id from the parent's durable Run and tool
    /// call identities. Length-prefixing prevents ambiguous delimiter collisions.
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

    /// The stable first-class child Run identity for this relationship.
    #[must_use]
    pub fn child_run_id(&self) -> RunId {
        RunId(format!("child-run:{}:{}", self.0.len(), self.0))
    }
}

/// Stable identity of a child result. Retries must reuse it so delivery is
/// idempotent rather than appending the result to the parent twice.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DelegationResultId(pub String);

impl DelegationResultId {
    /// One child Run produces one terminal result. Retries derive the same id.
    #[must_use]
    pub fn for_delegation(id: &DelegationId) -> Self {
        Self(format!("delegation-result:{}:{}", id.0.len(), id.0))
    }
}

/// Whether the child executes in this deployment or behind a remote Agent
/// adapter. This affects routing only; both kinds use the same lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelegationKind {
    Local,
    Remote,
}

/// The first-class origin recorded for a child Run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationOrigin {
    pub delegation_id: DelegationId,
    pub parent_run_id: RunId,
    pub parent_call_id: String,
    pub depth: u16,
}

impl DelegationOrigin {
    /// Build the root parent → child origin used by existing non-recursive
    /// runtimes. A nested runtime carries its parent's `depth` forward and uses
    /// [`Self::nested`] instead.
    #[must_use]
    pub fn root(parent_run_id: RunId, parent_call_id: impl Into<String>) -> Self {
        let parent_call_id = parent_call_id.into();
        Self {
            delegation_id: DelegationId::for_parent_call(&parent_run_id, &parent_call_id),
            parent_run_id,
            parent_call_id,
            depth: 1,
        }
    }

    /// Build a nested child origin, failing closed on depth overflow.
    pub fn nested(
        parent_run_id: RunId,
        parent_call_id: impl Into<String>,
        parent_depth: u16,
    ) -> Result<Self, DelegationError> {
        let parent_call_id = parent_call_id.into();
        let depth = parent_depth
            .checked_add(1)
            .ok_or(DelegationError::DepthOverflow)?;
        Ok(Self {
            delegation_id: DelegationId::for_parent_call(&parent_run_id, &parent_call_id),
            parent_run_id,
            parent_call_id,
            depth,
        })
    }

    #[must_use]
    pub fn child_run_id(&self) -> RunId {
        self.delegation_id.child_run_id()
    }

    #[must_use]
    pub fn result_id(&self) -> DelegationResultId {
        DelegationResultId::for_delegation(&self.delegation_id)
    }
}

/// Limits inherited by a parent Run's delegation group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationLimits {
    /// Maximum child depth below a root Run. Zero forbids delegation.
    pub max_depth: u16,
    /// Maximum children executing or awaiting at the same time.
    pub max_parallel: u16,
    /// Maximum children this parent may create over its whole lifetime.
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

/// Opaque continuation returned by an awaiting child. `revision` must increase
/// when the child awaits again, preventing an old continuation from resuming a
/// newer child state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegationContinuation {
    pub revision: u64,
    pub value: serde_json::Value,
}

/// Durable result produced by an ended child, before or after parent delivery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegationResult {
    pub id: DelegationResultId,
    pub output: String,
}

/// Coordination state of one delegation. Child execution state remains in the
/// child Run aggregate; this state records only what the parent/child handoff
/// still needs to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DelegationState {
    /// Relationship committed, child submission not yet confirmed.
    Requested,
    /// Child Run exists and may be driven independently.
    Active,
    /// Child awaits more input under this durable continuation.
    Awaiting(DelegationContinuation),
    /// Child ended and its result is durable, but the parent has not committed it.
    ResultPending(DelegationResult),
    /// Parent committed this result at `parent_run_version`.
    Delivered {
        result_id: DelegationResultId,
        parent_run_version: u64,
    },
    /// Parent ended/cancelled and durable cancellation must reach the child.
    CancelRequested,
    /// Child cancellation was acknowledged.
    Cancelled,
    /// A child result arrived after the parent ended and was intentionally ignored.
    Discarded { result_id: DelegationResultId },
}

impl DelegationState {
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Requested | Self::Active | Self::Awaiting(_) | Self::CancelRequested
        )
    }
}

/// The immutable parent/child relationship plus its coordination state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delegation {
    pub id: DelegationId,
    pub parent_run_id: RunId,
    pub parent_call_id: String,
    pub target_agent_id: String,
    pub child_run_id: RunId,
    pub kind: DelegationKind,
    pub depth: u16,
    pub state: DelegationState,
}

/// Input for idempotently creating one child relationship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDelegation {
    pub id: DelegationId,
    pub parent_call_id: String,
    pub target_agent_id: String,
    pub child_run_id: RunId,
    pub kind: DelegationKind,
}

/// Result of an idempotent delegation creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestResult {
    Created,
    Existing,
}

/// Result of applying a child/parent handoff event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryResult {
    Applied,
    Duplicate,
    LateResultIgnored,
}

/// Small, payload-free kernel of the child-result handoff. The aggregate uses
/// this function before applying payload/identity checks, which lets Kani prove
/// the same transition rules that production executes without modeling JSON,
/// strings, or maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultPhase {
    Open,
    Pending,
    Delivered,
    ParentEnded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultEvent {
    ChildEnded,
    ParentCommitted,
    ParentEnded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultEffect {
    RecordPending,
    MarkDelivered,
    Discard,
    Duplicate,
    Reject,
}

const fn result_transition(phase: ResultPhase, event: ResultEvent) -> ResultEffect {
    match (phase, event) {
        (ResultPhase::Open, ResultEvent::ChildEnded) => ResultEffect::RecordPending,
        (ResultPhase::Pending, ResultEvent::ParentCommitted) => ResultEffect::MarkDelivered,
        (ResultPhase::Open | ResultPhase::Pending, ResultEvent::ParentEnded) => {
            ResultEffect::Discard
        }
        (ResultPhase::Pending | ResultPhase::Delivered, ResultEvent::ChildEnded)
        | (ResultPhase::Delivered, ResultEvent::ParentCommitted)
        | (ResultPhase::ParentEnded, ResultEvent::ParentEnded) => ResultEffect::Duplicate,
        (ResultPhase::ParentEnded, ResultEvent::ChildEnded) => ResultEffect::Discard,
        (ResultPhase::ParentEnded, ResultEvent::ParentCommitted)
        | (ResultPhase::Open, ResultEvent::ParentCommitted)
        | (ResultPhase::Delivered, ResultEvent::ParentEnded) => ResultEffect::Reject,
    }
}

/// A fail-closed rejection of an invalid parent/child transition.
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
    #[error("invalid delegation transition from {state}")]
    InvalidTransition { state: &'static str },
    #[error("stale delegation continuation revision")]
    StaleContinuation,
    #[error("result id conflicts with the already recorded result")]
    ResultConflict,
    #[error("persisted delegation group is invalid: {0}")]
    InvalidPersistedState(String),
}

/// All delegated children owned by one parent Run. The full value is durable and
/// serializable, so a different process can continue delivery or cancellation
/// after either the parent or a child process crashes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegationGroup {
    parent_run_id: RunId,
    parent_agent_id: String,
    /// Root-to-parent lineage, including `parent_agent_id` as the last element.
    lineage: Vec<String>,
    parent_depth: u16,
    limits: DelegationLimits,
    parent_ended: bool,
    total_started: u32,
    delegations: BTreeMap<DelegationId, Delegation>,
    calls: BTreeMap<String, DelegationId>,
}

impl DelegationGroup {
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
    pub fn parent_ended(&self) -> bool {
        self.parent_ended
    }

    pub fn delegations(&self) -> impl Iterator<Item = &Delegation> {
        self.delegations.values()
    }

    #[must_use]
    pub fn get(&self, id: &DelegationId) -> Option<&Delegation> {
        self.delegations.get(id)
    }

    #[must_use]
    pub fn active_count(&self) -> usize {
        self.delegations
            .values()
            .filter(|delegation| delegation.state.is_active())
            .count()
    }

    /// Idempotently create a first-class child Run relationship.
    pub fn request(
        &mut self,
        request: RequestDelegation,
    ) -> Result<RequestResult, DelegationError> {
        if self.parent_ended {
            return Err(DelegationError::ParentEnded);
        }
        if let Some(existing_id) = self.calls.get(&request.parent_call_id) {
            let existing = self
                .delegations
                .get(existing_id)
                .expect("call index only contains existing delegations");
            return if existing.id == request.id
                && existing.target_agent_id == request.target_agent_id
                && existing.child_run_id == request.child_run_id
                && existing.kind == request.kind
            {
                Ok(RequestResult::Existing)
            } else {
                Err(DelegationError::CallConflict)
            };
        }
        if let Some(existing) = self.delegations.get(&request.id) {
            return if existing.parent_call_id == request.parent_call_id
                && existing.target_agent_id == request.target_agent_id
                && existing.child_run_id == request.child_run_id
                && existing.kind == request.kind
            {
                Ok(RequestResult::Existing)
            } else {
                Err(DelegationError::IdentityConflict)
            };
        }
        if self
            .delegations
            .values()
            .any(|existing| existing.child_run_id == request.child_run_id)
        {
            return Err(DelegationError::ChildRunConflict);
        }

        let depth = self
            .parent_depth
            .checked_add(1)
            .ok_or(DelegationError::DepthExceeded {
                depth: u16::MAX,
                limit: self.limits.max_depth,
            })?;
        if depth > self.limits.max_depth {
            return Err(DelegationError::DepthExceeded {
                depth,
                limit: self.limits.max_depth,
            });
        }
        if self.lineage.contains(&request.target_agent_id) {
            return Err(DelegationError::Cycle {
                agent_id: request.target_agent_id,
            });
        }
        if self.active_count() >= usize::from(self.limits.max_parallel) {
            return Err(DelegationError::ParallelLimit {
                limit: self.limits.max_parallel,
            });
        }
        if self.total_started >= self.limits.max_total {
            return Err(DelegationError::BudgetExhausted {
                limit: self.limits.max_total,
            });
        }

        let id = request.id.clone();
        let call_id = request.parent_call_id.clone();
        self.delegations.insert(
            id.clone(),
            Delegation {
                id: request.id,
                parent_run_id: self.parent_run_id.clone(),
                parent_call_id: request.parent_call_id,
                target_agent_id: request.target_agent_id,
                child_run_id: request.child_run_id,
                kind: request.kind,
                depth,
                state: DelegationState::Requested,
            },
        );
        self.calls.insert(call_id, id);
        self.total_started += 1;
        Ok(RequestResult::Created)
    }

    pub fn mark_active(&mut self, id: &DelegationId) -> Result<DeliveryResult, DelegationError> {
        let delegation = self.delegation_mut(id)?;
        match delegation.state {
            DelegationState::Requested => {
                delegation.state = DelegationState::Active;
                Ok(DeliveryResult::Applied)
            }
            DelegationState::Active => Ok(DeliveryResult::Duplicate),
            _ => Err(invalid_transition(&delegation.state)),
        }
    }

    pub fn mark_awaiting(
        &mut self,
        id: &DelegationId,
        continuation: DelegationContinuation,
    ) -> Result<DeliveryResult, DelegationError> {
        let delegation = self.delegation_mut(id)?;
        match &delegation.state {
            DelegationState::Requested | DelegationState::Active => {
                delegation.state = DelegationState::Awaiting(continuation);
                Ok(DeliveryResult::Applied)
            }
            DelegationState::Awaiting(current) if continuation.revision == current.revision => {
                Ok(DeliveryResult::Duplicate)
            }
            DelegationState::Awaiting(current) if continuation.revision > current.revision => {
                delegation.state = DelegationState::Awaiting(continuation);
                Ok(DeliveryResult::Applied)
            }
            DelegationState::Awaiting(_) => Err(DelegationError::StaleContinuation),
            _ => Err(invalid_transition(&delegation.state)),
        }
    }

    /// Record the durable child result. If the parent already ended, the result is
    /// retained as discarded evidence and can never reopen the parent Run.
    pub fn record_result(
        &mut self,
        id: &DelegationId,
        result: DelegationResult,
    ) -> Result<DeliveryResult, DelegationError> {
        let parent_ended = self.parent_ended;
        let delegation = self.delegation_mut(id)?;
        if parent_ended {
            debug_assert!(matches!(
                result_transition(ResultPhase::ParentEnded, ResultEvent::ChildEnded),
                ResultEffect::Discard
            ));
            return match &delegation.state {
                DelegationState::Discarded { result_id } if result_id == &result.id => {
                    Ok(DeliveryResult::Duplicate)
                }
                DelegationState::Delivered { result_id, .. } if result_id == &result.id => {
                    Ok(DeliveryResult::Duplicate)
                }
                DelegationState::Discarded { .. } | DelegationState::Delivered { .. } => {
                    Err(DelegationError::ResultConflict)
                }
                _ => {
                    delegation.state = DelegationState::Discarded {
                        result_id: result.id,
                    };
                    Ok(DeliveryResult::LateResultIgnored)
                }
            };
        }
        let phase = result_phase(&delegation.state);
        match result_transition(phase, ResultEvent::ChildEnded) {
            ResultEffect::RecordPending => {
                delegation.state = DelegationState::ResultPending(result);
                Ok(DeliveryResult::Applied)
            }
            ResultEffect::Duplicate => match &delegation.state {
                DelegationState::ResultPending(current) if current.id == result.id => {
                    Ok(DeliveryResult::Duplicate)
                }
                DelegationState::Delivered { result_id, .. } if result_id == &result.id => {
                    Ok(DeliveryResult::Duplicate)
                }
                DelegationState::ResultPending(_) | DelegationState::Delivered { .. } => {
                    Err(DelegationError::ResultConflict)
                }
                _ => Err(invalid_transition(&delegation.state)),
            },
            ResultEffect::Discard | ResultEffect::MarkDelivered | ResultEffect::Reject => {
                Err(invalid_transition(&delegation.state))
            }
        }
    }

    /// Mark a pending result as committed by the parent. Replaying the same
    /// delivery is a no-op; a different result id fails closed.
    pub fn deliver_result(
        &mut self,
        id: &DelegationId,
        result_id: &DelegationResultId,
        parent_run_version: u64,
    ) -> Result<DeliveryResult, DelegationError> {
        if self.parent_ended {
            return Ok(DeliveryResult::LateResultIgnored);
        }
        let delegation = self.delegation_mut(id)?;
        let phase = result_phase(&delegation.state);
        match result_transition(phase, ResultEvent::ParentCommitted) {
            ResultEffect::MarkDelivered => {
                let DelegationState::ResultPending(result) = &delegation.state else {
                    unreachable!("result phase and delegation state diverged")
                };
                if &result.id != result_id {
                    return Err(DelegationError::ResultConflict);
                }
                delegation.state = DelegationState::Delivered {
                    result_id: result_id.clone(),
                    parent_run_version,
                };
                Ok(DeliveryResult::Applied)
            }
            ResultEffect::Duplicate => match &delegation.state {
                DelegationState::Delivered {
                    result_id: current, ..
                } if current == result_id => Ok(DeliveryResult::Duplicate),
                DelegationState::Delivered { .. } => Err(DelegationError::ResultConflict),
                _ => Err(invalid_transition(&delegation.state)),
            },
            ResultEffect::Discard | ResultEffect::RecordPending | ResultEffect::Reject => {
                Err(invalid_transition(&delegation.state))
            }
        }
    }

    /// End the parent and durably request cancellation of every active child.
    /// Pending results become discarded; delivered history remains immutable.
    pub fn end_parent(&mut self) -> Vec<RunId> {
        if self.parent_ended {
            return Vec::new();
        }
        self.parent_ended = true;
        let mut cancel = Vec::new();
        for delegation in self.delegations.values_mut() {
            let result_effect =
                result_transition(result_phase(&delegation.state), ResultEvent::ParentEnded);
            match &delegation.state {
                DelegationState::Requested
                | DelegationState::Active
                | DelegationState::Awaiting(_) => {
                    delegation.state = DelegationState::CancelRequested;
                    cancel.push(delegation.child_run_id.clone());
                }
                DelegationState::ResultPending(result) => {
                    debug_assert_eq!(result_effect, ResultEffect::Discard);
                    delegation.state = DelegationState::Discarded {
                        result_id: result.id.clone(),
                    };
                }
                DelegationState::CancelRequested
                | DelegationState::Cancelled
                | DelegationState::Delivered { .. }
                | DelegationState::Discarded { .. } => {}
            }
        }
        cancel
    }

    pub fn confirm_cancelled(
        &mut self,
        id: &DelegationId,
    ) -> Result<DeliveryResult, DelegationError> {
        let delegation = self.delegation_mut(id)?;
        match delegation.state {
            DelegationState::CancelRequested => {
                delegation.state = DelegationState::Cancelled;
                Ok(DeliveryResult::Applied)
            }
            DelegationState::Cancelled => Ok(DeliveryResult::Duplicate),
            _ => Err(invalid_transition(&delegation.state)),
        }
    }

    /// Validate a value restored after a process crash before it becomes live.
    pub fn validate(&self) -> Result<(), DelegationError> {
        if self.lineage.last() != Some(&self.parent_agent_id) {
            return Err(DelegationError::InvalidPersistedState(
                "lineage does not end at the parent Agent".to_string(),
            ));
        }
        let delegation_count = u32::try_from(self.delegations.len()).map_err(|_| {
            DelegationError::InvalidPersistedState(
                "delegation count does not fit the durable budget type".to_string(),
            )
        })?;
        if self.total_started != delegation_count
            || self.calls.len() != self.delegations.len()
            || self.total_started > self.limits.max_total
        {
            return Err(DelegationError::InvalidPersistedState(
                "delegation indexes or budget count diverged".to_string(),
            ));
        }
        let expected_depth = self.parent_depth.checked_add(1).ok_or_else(|| {
            DelegationError::InvalidPersistedState("delegation depth overflowed".to_string())
        })?;
        if expected_depth > self.limits.max_depth {
            return Err(DelegationError::InvalidPersistedState(
                "delegation depth exceeds its inherited limit".to_string(),
            ));
        }
        let mut children = std::collections::BTreeSet::new();
        for (id, delegation) in &self.delegations {
            if id != &delegation.id
                || delegation.parent_run_id != self.parent_run_id
                || delegation.depth != expected_depth
                || self.calls.get(&delegation.parent_call_id) != Some(id)
                || !children.insert(&delegation.child_run_id.0)
            {
                return Err(DelegationError::InvalidPersistedState(
                    "a parent/child identity invariant was violated".to_string(),
                ));
            }
            if self.lineage.contains(&delegation.target_agent_id) {
                return Err(DelegationError::InvalidPersistedState(
                    "a persisted delegation contains an Agent cycle".to_string(),
                ));
            }
            if self.parent_ended
                && matches!(
                    delegation.state,
                    DelegationState::Requested
                        | DelegationState::Active
                        | DelegationState::Awaiting(_)
                        | DelegationState::ResultPending(_)
                )
            {
                return Err(DelegationError::InvalidPersistedState(
                    "an ended parent retained actionable child state".to_string(),
                ));
            }
        }
        if self.active_count() > usize::from(self.limits.max_parallel) {
            return Err(DelegationError::InvalidPersistedState(
                "parallel delegation limit was exceeded".to_string(),
            ));
        }
        Ok(())
    }

    fn delegation_mut(&mut self, id: &DelegationId) -> Result<&mut Delegation, DelegationError> {
        self.delegations
            .get_mut(id)
            .ok_or(DelegationError::NotFound)
    }
}

fn invalid_transition(state: &DelegationState) -> DelegationError {
    let state = match state {
        DelegationState::Requested => "Requested",
        DelegationState::Active => "Active",
        DelegationState::Awaiting(_) => "Awaiting",
        DelegationState::ResultPending(_) => "ResultPending",
        DelegationState::Delivered { .. } => "Delivered",
        DelegationState::CancelRequested => "CancelRequested",
        DelegationState::Cancelled => "Cancelled",
        DelegationState::Discarded { .. } => "Discarded",
    };
    DelegationError::InvalidTransition { state }
}

fn result_phase(state: &DelegationState) -> ResultPhase {
    match state {
        DelegationState::Requested
        | DelegationState::Active
        | DelegationState::Awaiting(_)
        | DelegationState::CancelRequested => ResultPhase::Open,
        DelegationState::ResultPending(_) => ResultPhase::Pending,
        DelegationState::Delivered { .. } => ResultPhase::Delivered,
        DelegationState::Cancelled | DelegationState::Discarded { .. } => ResultPhase::ParentEnded,
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_phase(tag: u8) -> ResultPhase {
        match tag % 4 {
            0 => ResultPhase::Open,
            1 => ResultPhase::Pending,
            2 => ResultPhase::Delivered,
            _ => ResultPhase::ParentEnded,
        }
    }

    fn symbolic_event(tag: u8) -> ResultEvent {
        match tag % 3 {
            0 => ResultEvent::ChildEnded,
            1 => ResultEvent::ParentCommitted,
            _ => ResultEvent::ParentEnded,
        }
    }

    #[kani::proof]
    fn a_result_can_be_marked_delivered_only_from_pending() {
        let phase = symbolic_phase(kani::any());
        let effect = result_transition(phase, ResultEvent::ParentCommitted);
        assert!(!matches!(effect, ResultEffect::MarkDelivered) || phase == ResultPhase::Pending);
    }

    #[kani::proof]
    fn an_ended_parent_never_accepts_a_result_delivery() {
        let event = symbolic_event(kani::any());
        let effect = result_transition(ResultPhase::ParentEnded, event);
        assert!(!matches!(
            effect,
            ResultEffect::RecordPending | ResultEffect::MarkDelivered
        ));
    }

    #[kani::proof]
    fn exactly_once_effects_have_unique_preconditions() {
        let phase = symbolic_phase(kani::any());
        let event = symbolic_event(kani::any());
        let effect = result_transition(phase, event);
        if matches!(effect, ResultEffect::RecordPending) {
            assert_eq!((phase, event), (ResultPhase::Open, ResultEvent::ChildEnded));
        }
        if matches!(effect, ResultEffect::MarkDelivered) {
            assert_eq!(
                (phase, event),
                (ResultPhase::Pending, ResultEvent::ParentCommitted)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(max_parallel: u16, max_total: u32) -> DelegationGroup {
        DelegationGroup::new(
            RunId("parent-run".into()),
            "coordinator",
            vec!["root".into()],
            0,
            DelegationLimits::new(3, max_parallel, max_total),
        )
    }

    fn request(n: u8, kind: DelegationKind) -> RequestDelegation {
        RequestDelegation {
            id: DelegationId(format!("delegation-{n}")),
            parent_call_id: format!("call-{n}"),
            target_agent_id: format!("agent-{n}"),
            child_run_id: RunId(format!("child-run-{n}")),
            kind,
        }
    }

    fn result(n: u8) -> DelegationResult {
        DelegationResult {
            id: DelegationResultId(format!("result-{n}")),
            output: format!("output-{n}"),
        }
    }

    #[test]
    fn child_run_identity_and_parent_relationship_are_first_class() {
        let mut group = group(2, 2);
        assert_eq!(
            group.request(request(1, DelegationKind::Local)),
            Ok(RequestResult::Created)
        );
        let child = group.get(&DelegationId("delegation-1".into())).unwrap();
        assert_eq!(child.parent_run_id, RunId("parent-run".into()));
        assert_eq!(child.child_run_id, RunId("child-run-1".into()));
        assert_eq!(child.parent_call_id, "call-1");
        assert_eq!(child.depth, 1);
    }

    #[test]
    fn parallel_children_are_bounded_and_release_capacity_after_ending() {
        let mut group = group(2, 3);
        group.request(request(1, DelegationKind::Local)).unwrap();
        group.request(request(2, DelegationKind::Remote)).unwrap();
        assert_eq!(group.active_count(), 2);
        assert_eq!(
            group.request(request(3, DelegationKind::Local)),
            Err(DelegationError::ParallelLimit { limit: 2 })
        );
        group
            .record_result(&DelegationId("delegation-1".into()), result(1))
            .unwrap();
        assert_eq!(group.active_count(), 1);
        assert_eq!(
            group.request(request(3, DelegationKind::Local)),
            Ok(RequestResult::Created)
        );
    }

    #[test]
    fn parent_and_child_recover_independently_from_serialized_truth() {
        let mut before_crash = group(2, 2);
        before_crash
            .request(request(1, DelegationKind::Remote))
            .unwrap();
        before_crash
            .mark_awaiting(
                &DelegationId("delegation-1".into()),
                DelegationContinuation {
                    revision: 1,
                    value: serde_json::json!({"remote_task":"t1"}),
                },
            )
            .unwrap();
        let bytes = serde_json::to_vec(&before_crash).unwrap();

        let mut recovered: DelegationGroup = serde_json::from_slice(&bytes).unwrap();
        recovered.validate().unwrap();
        recovered
            .record_result(&DelegationId("delegation-1".into()), result(1))
            .unwrap();
        assert!(matches!(
            recovered
                .get(&DelegationId("delegation-1".into()))
                .unwrap()
                .state,
            DelegationState::ResultPending(_)
        ));
    }

    #[test]
    fn child_end_is_durable_before_parent_delivery_and_delivery_is_exactly_once() {
        let mut group = group(1, 1);
        let id = DelegationId("delegation-1".into());
        let result_id = DelegationResultId("result-1".into());
        group.request(request(1, DelegationKind::Local)).unwrap();
        assert_eq!(
            group.record_result(&id, result(1)),
            Ok(DeliveryResult::Applied)
        );
        assert!(matches!(
            group.get(&id).unwrap().state,
            DelegationState::ResultPending(_)
        ));
        assert_eq!(
            group.deliver_result(&id, &result_id, 7),
            Ok(DeliveryResult::Applied)
        );
        assert_eq!(
            group.deliver_result(&id, &result_id, 7),
            Ok(DeliveryResult::Duplicate)
        );
    }

    #[test]
    fn late_result_after_parent_end_is_discarded_and_never_delivered() {
        let mut group = group(1, 1);
        let id = DelegationId("delegation-1".into());
        group.request(request(1, DelegationKind::Remote)).unwrap();
        assert_eq!(group.end_parent(), vec![RunId("child-run-1".into())]);
        assert_eq!(
            group.record_result(&id, result(1)),
            Ok(DeliveryResult::LateResultIgnored)
        );
        assert!(matches!(
            group.get(&id).unwrap().state,
            DelegationState::Discarded { .. }
        ));
        assert_eq!(
            group.deliver_result(&id, &DelegationResultId("result-1".into()), 8),
            Ok(DeliveryResult::LateResultIgnored)
        );
    }

    #[test]
    fn cancellation_request_survives_a_process_restart() {
        let mut group = group(2, 2);
        group.request(request(1, DelegationKind::Local)).unwrap();
        group.request(request(2, DelegationKind::Remote)).unwrap();
        let mut children = group.end_parent();
        children.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            children,
            vec![RunId("child-run-1".into()), RunId("child-run-2".into())]
        );
        let bytes = serde_json::to_vec(&group).unwrap();
        let mut recovered: DelegationGroup = serde_json::from_slice(&bytes).unwrap();
        recovered.validate().unwrap();
        assert_eq!(
            recovered.confirm_cancelled(&DelegationId("delegation-2".into())),
            Ok(DeliveryResult::Applied)
        );
    }

    #[test]
    fn depth_cycle_parallelism_and_total_budget_fail_closed() {
        let mut cycle = DelegationGroup::new(
            RunId("p".into()),
            "coordinator",
            vec!["root".into()],
            0,
            DelegationLimits::new(1, 2, 2),
        );
        let mut cycle_request = request(1, DelegationKind::Local);
        cycle_request.target_agent_id = "root".into();
        assert!(matches!(
            cycle.request(cycle_request),
            Err(DelegationError::Cycle { .. })
        ));

        let mut too_deep = DelegationGroup::new(
            RunId("p".into()),
            "parent",
            vec![],
            2,
            DelegationLimits::new(2, 1, 1),
        );
        assert!(matches!(
            too_deep.request(request(1, DelegationKind::Local)),
            Err(DelegationError::DepthExceeded { .. })
        ));

        let mut budget = group(1, 1);
        budget.request(request(1, DelegationKind::Local)).unwrap();
        budget
            .record_result(&DelegationId("delegation-1".into()), result(1))
            .unwrap();
        assert_eq!(
            budget.request(request(2, DelegationKind::Local)),
            Err(DelegationError::BudgetExhausted { limit: 1 })
        );
    }

    #[test]
    fn local_and_remote_children_follow_identical_delivery_transitions() {
        fn trace(kind: DelegationKind) -> Vec<DelegationState> {
            let mut group = group(1, 1);
            let id = DelegationId("delegation-1".into());
            group.request(request(1, kind)).unwrap();
            let mut states = vec![group.get(&id).unwrap().state.clone()];
            group.mark_active(&id).unwrap();
            states.push(group.get(&id).unwrap().state.clone());
            group.record_result(&id, result(1)).unwrap();
            states.push(group.get(&id).unwrap().state.clone());
            group
                .deliver_result(&id, &DelegationResultId("result-1".into()), 2)
                .unwrap();
            states.push(group.get(&id).unwrap().state.clone());
            states
        }
        assert_eq!(trace(DelegationKind::Local), trace(DelegationKind::Remote));
    }

    #[test]
    fn duplicate_request_reuses_the_same_child_and_conflicts_fail_closed() {
        let mut group = group(1, 1);
        let initial_request = request(1, DelegationKind::Local);
        assert_eq!(
            group.request(initial_request.clone()),
            Ok(RequestResult::Created)
        );
        assert_eq!(group.request(initial_request), Ok(RequestResult::Existing));
        let mut conflict = request(2, DelegationKind::Remote);
        conflict.parent_call_id = "call-1".into();
        assert_eq!(group.request(conflict), Err(DelegationError::CallConflict));
    }

    #[test]
    fn durable_ids_are_deterministic_and_delimiter_safe() {
        let a = DelegationOrigin::root(RunId("parent:a".into()), "call");
        let b = DelegationOrigin::root(RunId("parent".into()), "a:call");
        assert_ne!(a.delegation_id, b.delegation_id);
        assert_eq!(a, DelegationOrigin::root(RunId("parent:a".into()), "call"));
        assert_eq!(a.child_run_id(), a.delegation_id.child_run_id());
        assert_eq!(
            a.result_id(),
            DelegationResultId::for_delegation(&a.delegation_id)
        );
    }

    #[test]
    fn nested_origin_increments_depth_and_overflow_fails_closed() {
        let nested = DelegationOrigin::nested(RunId("parent".into()), "call", 7).unwrap();
        assert_eq!(nested.depth, 8);
        assert_eq!(
            DelegationOrigin::nested(RunId("parent".into()), "call", u16::MAX),
            Err(DelegationError::DepthOverflow)
        );
    }
}
