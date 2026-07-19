//! Durable parent/child Run relationships created by Agent tool calls.
//!
//! This aggregate owns relationship identity, lineage budgets, and cancellation.
//! It deliberately does not own tool execution or result delivery: those phases
//! live once in the parent Run's durable `ToolBatch` and commit through the same
//! `ThreadCommit.state` boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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

/// Stable identity of the one terminal tool result for a child Run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DelegationResultId(pub String);

impl DelegationResultId {
    #[must_use]
    pub fn for_delegation(id: &DelegationId) -> Self {
        Self(format!("delegation-result:{}:{}", id.0.len(), id.0))
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

    #[must_use]
    pub fn result_id(&self) -> DelegationResultId {
        DelegationResultId::for_delegation(&self.delegation_id)
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
    pub child_run_id: RunId,
    pub depth: u16,
    pub status: DelegationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDelegation {
    pub id: DelegationId,
    pub parent_call_id: String,
    pub target_agent_id: String,
    pub child_run_id: RunId,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
            let existing = self
                .delegations
                .get(existing_id)
                .expect("call index points at an existing delegation");
            return if existing.id == request.id
                && existing.target_agent_id == request.target_agent_id
                && existing.child_run_id == request.child_run_id
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
        let depth = self
            .parent_depth
            .checked_add(1)
            .ok_or(DelegationError::DepthOverflow)?;
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
                depth,
                status: DelegationStatus::Open,
            },
        );
        self.calls.insert(call_id, id);
        self.total_started += 1;
        Ok(TransitionResult::Applied)
    }

    pub fn complete(&mut self, id: &DelegationId) -> Result<TransitionResult, DelegationError> {
        let delegation = self
            .delegations
            .get_mut(id)
            .ok_or(DelegationError::NotFound)?;
        let (status, result) =
            transition_relationship(delegation.status, RelationshipTransition::Complete)?;
        delegation.status = status;
        Ok(result)
    }

    /// Persist cancellation intent before an adapter attempts delivery.
    pub fn end_parent(&mut self) -> Vec<RunId> {
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
                children.push(delegation.child_run_id.clone());
            }
        }
        children
    }

    pub fn validate(&self) -> Result<(), DelegationError> {
        if self.lineage.last() != Some(&self.parent_agent_id) {
            return Err(DelegationError::InvalidPersistedState(
                "lineage must end with parent agent".into(),
            ));
        }
        if self.total_started != self.delegations.len() as u32
            || self.calls.len() != self.delegations.len()
        {
            return Err(DelegationError::InvalidPersistedState(
                "indexes and total must match relationships".into(),
            ));
        }
        for (id, delegation) in &self.delegations {
            if id != &delegation.id
                || delegation.parent_run_id != self.parent_run_id
                || self.calls.get(&delegation.parent_call_id) != Some(id)
            {
                return Err(DelegationError::InvalidPersistedState(
                    "relationship identity or call index mismatch".into(),
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
            child_run_id: RunId(format!("r{n}")),
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

    #[test]
    fn parent_end_persists_idempotent_cancellation_intent() {
        let mut registry = registry(2, 2);
        registry.request(request(1, "researcher")).unwrap();
        assert_eq!(registry.end_parent(), vec![RunId("r1".into())]);
        assert_eq!(
            registry.get(&DelegationId("d1".into())).unwrap().status,
            DelegationStatus::CancelRequested
        );
        assert!(registry.end_parent().is_empty());
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

    #[kani::proof]
    fn every_relationship_effect_has_one_documented_precondition() {
        let status = symbolic_status(kani::any());
        let transition = symbolic_transition(kani::any());
        let result = transition_relationship(status, transition);
        let expected_applied = matches!(
            (status, transition),
            (DelegationStatus::Open, RelationshipTransition::Complete)
                | (
                    DelegationStatus::Open,
                    RelationshipTransition::RequestCancel
                )
        );
        assert_eq!(
            result
                .as_ref()
                .is_ok_and(|(_, effect)| *effect == TransitionResult::Applied),
            expected_applied
        );
        if let Ok((next, TransitionResult::Duplicate)) = result {
            assert_eq!(next, status);
        }
    }
}
