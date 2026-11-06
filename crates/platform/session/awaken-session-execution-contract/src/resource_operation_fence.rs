// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Static owner: the closed wire union of authorities a Runtime-authored Resource effect may
// @formal reason: carry. It decides nothing: each Resource transport matches the arm and validates its
// @formal reason: claim or realization lease against the durable owner. Evidence: the persisted-wire golden
// @formal reason: tests and claimed-worker conformance; a product-requirement classification, not a proof.
//! The one closed fencing vocabulary for Runtime-authored Resource effects.
//!
//! Four of its five arms are Session realization authority; the fifth is the
//! dispatch claim that ordinary Run work retains. This contract already owns
//! or depends on every arm's type, so the union lives with the Session.

use awaken_worker_contract::RunClaim;

/// The one closed fencing vocabulary for Runtime-authored Resource effects.
/// Ordinary Run work retains its dispatch claim; terminal recovery carries the
/// aggregate-derived cleanup command and realization lease; checkpoint source
/// release carries the existing Environment operation whose root state already
/// owns the durable checkpoint. Resources remains generic over this value and
/// owns no execution authority of its own.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "type",
    content = "fence",
    rename_all = "snake_case",
    deny_unknown_fields
)]
// This is the single short-lived, closed transport union for mutually
// exclusive Resource operations. Boxing only the terminal arm would change
// the public Rust contract while preserving the same wire authority.
#[allow(clippy::large_enum_variant)]
pub enum ResourceOperationFence {
    Run(RunClaim),
    SessionMaterialization(crate::SessionResourceMaterializationIntent),
    RepositoryPublication {
        command: Box<crate::SessionRepositoryPublicationCommand>,
        lease: crate::SessionRealizationLease,
    },
    CheckpointRelease(crate::SessionEnvironmentOperation),
    Terminal(crate::SessionTerminalCleanupEffect),
}

impl From<RunClaim> for ResourceOperationFence {
    fn from(claim: RunClaim) -> Self {
        Self::Run(claim)
    }
}

impl ResourceOperationFence {
    /// Original Run identity only; consumers retain their own data contract
    /// and callers still validate the existing live authority independently.
    pub fn run_claim_identity(&self) -> Option<(&str, &str, u64)> {
        match self {
            Self::Run(claim) => Some((&claim.run_id.0, &claim.owner, claim.epoch)),
            _ => None,
        }
    }
}
