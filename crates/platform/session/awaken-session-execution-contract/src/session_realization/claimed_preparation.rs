// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This borrowed role preserves original Dispatch exclusion and raw Run coordinates;
// @formal reason: it grants no durable execution authority or replacement of the Session root CAS.
//! Borrowed authority for credential preparation before physical Run admission.
//!
//! The authenticated Dispatch adapter keeps its original exact-claim guard
//! alive while exposing these observations. This role is neither a wire DTO nor
//! a reusable authorization token; every mutation revalidates its owner clock.

use super::SessionRealizationControlFailure;

pub trait ClaimedSessionCredentialPreparationContext: Send + Sync {
    fn claim(&self) -> &awaken_worker_contract::RunClaim;
    fn session_id(&self) -> &str;
    fn thread_id(&self) -> &awaken_agent_contract::ThreadId;
    fn activation(&self) -> &awaken_runtime_contract::RunActivation;
    fn execution_scope(&self) -> Option<&awaken_agent_contract::ExecutionScopeRef>;
    fn session_work_authority(
        &self,
    ) -> &awaken_session_application_contract::SessionWorkAuthorityReceipt;

    /// Require the original held claim, a live lease, no cancellation and an
    /// empty physical-attempt slot from that same locked row. A new claim or a
    /// process restart alone is never predecessor-stop evidence.
    fn verify_at(&self, now_unix_ms: u64) -> Result<(), SessionRealizationControlFailure>;
}

impl dyn ClaimedSessionCredentialPreparationContext + '_ {
    /// Check the shared frozen coordinates as well as the retained owner's
    /// current exclusion. Session activity and mutation laws remain with the
    /// application command; this check grants no mutation by itself.
    pub fn verify_session_at(
        &self,
        session_id: &str,
        now_unix_ms: u64,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.verify_at(now_unix_ms)?;
        if self.session_id() != session_id
            || self.session_work_authority().session_id().as_str() != session_id
            || self.claim().run_id != self.activation().run_id
            || self.thread_id() != &self.activation().thread_id
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "claimed preparation targets another Session or Run".into(),
            ));
        }
        Ok(())
    }
}
