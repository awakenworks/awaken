// @formal surface obligation=session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal surface obligation=session_realization.concurrent_drivers_preserve_one_owner_fence
// @formal reason: Shared root-lease and terminal-assignment admission helpers keep local recovery and
// @formal reason: external Workers beside one CAS claim algorithm and verify the aggregate's exact
// @formal reason: realization generation, preventing a second terminal lease state machine.
//! Existing root-lease and terminal-claim admission helpers.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

pub(crate) fn verify_lease(
    session: &PersistedSession,
    asserted: &SessionRealizationLease,
    now_ms: u64,
) -> Result<(), SessionRealizationControlFailure> {
    if !session.realization.as_ref().is_some_and(|current| {
        awaken_session_configuration_contract::resource::realization_lease_authorizes(
            current, asserted, now_ms,
        )
    }) {
        return Err(SessionRealizationControlFailure::StaleOwnership);
    }
    Ok(())
}

pub(super) fn exact_generation_key(generation: &McpGenerationRef) -> String {
    awaken_agent_contract::stable_fingerprint(generation)
}
