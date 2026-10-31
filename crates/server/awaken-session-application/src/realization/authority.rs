// @formal surface obligation=session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal surface obligation=session_realization.concurrent_drivers_preserve_one_owner_fence
// @formal reason: Shared root-lease and terminal-assignment admission helpers keep local recovery and
// @formal reason: external Workers beside one CAS claim algorithm and verify the aggregate's exact
// @formal reason: realization generation, preventing a second terminal lease state machine.
//! Existing root-lease and terminal-claim admission helpers.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

pub(super) fn verify_lease(
    session: &PersistedSession,
    asserted: &SessionRealizationLease,
    now_ms: u64,
) -> Result<(), SessionRealizationControlFailure> {
    if !session.realization.as_ref().is_some_and(|current| {
        awaken_session_realization_contract::realization_lease_authorizes(current, asserted, now_ms)
    }) {
        return Err(SessionRealizationControlFailure::StaleOwnership);
    }
    Ok(())
}

pub(super) fn exact_generation_key(generation: &McpGenerationRef) -> String {
    awaken_agent_contract::stable_fingerprint(generation)
}

pub(super) fn generation_set_authorizes(
    current: &[McpGenerationRef],
    asserted: &[McpGenerationRef],
) -> bool {
    // A bijection is required: two expiry variants of one generation cannot
    // stand in for the receipt of another attachment.
    let mut matched = BTreeSet::new();
    current.len() == asserted.len()
        && current.iter().all(|expected| {
            asserted
                .iter()
                .position(|actual| {
                    awaken_session_realization_contract::realization_generation_authorizes(
                        expected, actual,
                    )
                })
                .is_some_and(|index| matched.insert(index))
        })
}
