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
