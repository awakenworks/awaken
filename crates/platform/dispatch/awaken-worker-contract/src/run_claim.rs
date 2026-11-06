// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: the serializable claim a Coordinator grants a Worker for one Run.
// @formal reason: It carries the Run, owner and fencing epoch only; the dispatch store that mints it
// @formal reason: and the Coordinator edge that checks it keep every decision. The epoch wire law
// @formal reason: is the lease contract's; this classification makes no direct proof claim.
//! The claim a Coordinator grants a Worker for one Run.
//!
//! It is the Coordinator↔Worker vocabulary every claim-fenced call carries;
//! `awaken-dispatch-contract` mints it from its lease and re-exports it.

use awaken_agent_contract::agent::run::Id as RunId;
use serde::{Deserialize, Serialize};

/// Durable execution authority for one claimed Run.
///
/// Keeping the run, owner, and fencing epoch together prevents commit, settle,
/// and cancellation code from accidentally mixing fields from different claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunClaim {
    pub run_id: RunId,
    pub owner: String,
    #[serde(deserialize_with = "awaken_lease_contract::fence::Epoch::deserialize_u64")]
    pub epoch: u64,
}
