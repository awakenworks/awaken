// @formal surface obligation=credential.attempt_delivery_is_exact_epoch_bound_and_authority_preserving
// @formal reason: Static owner: read-only in-memory projection of the binding already frozen by the
// @formal reason: Dispatch claim. Dynamic path: under the parent store mutex it returns a binding only for
// @formal reason: the exact live owner, epoch and candidate; expiry, replacement and foreign candidates
// @formal reason: return absence. The production delivery-admission kernel and shared
// @formal reason: Memory/SQLite/PostgreSQL conformance table own exact authority and epoch rules; this
// @formal reason: classification does not claim a proof of mutex scheduling or backend refinement.
use awaken_runtime_contract::{AttemptCredentialBinding, CandidateFingerprint};

use super::{DispatchState, RunClaim, State};

pub(super) fn current(
    state: &State,
    claim: &RunClaim,
    candidate: &CandidateFingerprint,
    now_ms: u64,
) -> Option<AttemptCredentialBinding> {
    let row = state.rows.get(&claim.run_id)?;
    if row.state != DispatchState::Leased
        || row.lease_epoch != claim.epoch
        || row
            .lease
            .as_ref()
            .is_none_or(|lease| lease.owner != claim.owner || lease.expires_ms <= now_ms)
    {
        return None;
    }
    row.credential_bindings
        .iter()
        .find(|binding| &binding.candidate_fingerprint == candidate)
        .cloned()
}
