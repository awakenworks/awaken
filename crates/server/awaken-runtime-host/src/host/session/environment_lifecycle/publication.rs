// @formal surface obligation=session_environment.owner_topology_is_exact_and_frozen
// @formal surface obligation=session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal reason: Static owner: Runtime-host transition adapter over the single process-local Environment
// @formal reason: owner retained in the Session slot. Dynamic path: preparation, durable adoption, and
// @formal reason: publication all execute inside session_slots.update and delegate to that one owner; only
// @formal reason: its hidden prepared candidate can be published after the aggregate binding sink returns
// @formal reason: exact Store-read authority, while cleanup retry only consumes the owner's retirement
// @formal reason: projection. This module adds no registry, lease authority, or parallel Environment
// @formal reason: state. Evidence: session_environment.owner_topology_is_exact_and_frozen and adjacent
// @formal reason: lifecycle tests. Optional scoped candidates bind the current Worker/Session authority
// @formal reason: view at this same enrollment edge; selected-but-unbound terminal owners gain no
// @formal reason: execution grant. The existing classification remains this file's sole surface mapping.
//! Publication edge of the one Session Environment lifecycle owner.
//!
//! This is a private split of `environment_lifecycle`, not a second owner:
//! every transition still writes the same [`SessionEnvironmentOwner`] under
//! the Session lifecycle guard.

use super::*;

impl SharedHost {
    pub(crate) fn begin_session_environment_preparation(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<UnboundSessionEnvironment, HostError> {
        self.begin_session_environment_candidate(
            thread,
            environment,
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
        )
    }

    pub(crate) fn begin_session_environment_adoption(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<UnboundSessionEnvironment, HostError> {
        self.begin_session_environment_candidate(
            thread,
            environment,
            awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt,
        )
    }

    fn begin_session_environment_candidate(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        kind: awaken_session_state_contract::SessionEnvironmentEffectKind,
    ) -> Result<UnboundSessionEnvironment, HostError> {
        self.session_slots.update(thread, |slot| {
            let candidate = slot
                .realization
                .environment_owner
                .begin_preparing(environment, kind)?;
            #[cfg(target_os = "linux")]
            self.session_slots.bind_execution_authority(
                thread,
                slot,
                &candidate.environment,
                self.execution_authority.as_ref(),
            )?;
            Ok(candidate)
        })
    }

    pub(crate) fn publish_prepared_session_environment(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
        identity: BoundSessionEnvironmentIdentity,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        self.session_slots.update_realization(thread, |r| {
            r.environment_owner.publish_prepared(candidate, identity)
        })
    }

    pub(crate) fn retain_committed_session_environment_candidate(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
        identity: BoundSessionEnvironmentIdentity,
    ) -> Result<UnboundSessionEnvironment, HostError> {
        self.session_slots.update_realization(thread, |r| {
            r.environment_owner
                .retain_committed_candidate(candidate, identity)
        })
    }

    #[cfg(test)]
    pub(crate) fn prepared_session_environment(
        &self,
        thread: &str,
    ) -> Option<UnboundSessionEnvironment> {
        self.session_slots
            .read_realization(thread, |r| match &r.environment_owner {
                SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(
                    candidate,
                )) => Some(candidate.clone()),
                _ => None,
            })
            .flatten()
    }

    /// Finish a previously interrupted unpublished cleanup before any caller
    /// may create or adopt another physical owner. A provider error, live
    /// status, cancellation, or ABA mismatch leaves the exact Retiring value.
    pub(crate) async fn retry_unpublished_session_environment_cleanup(
        &self,
        thread: &str,
    ) -> Result<(), HostError> {
        let retirement = self
            .session_slots
            .read_realization(thread, |r| match &r.environment_owner {
                SessionEnvironmentOwner::Retiring(retiring)
                    if retiring.cause
                        == SessionEnvironmentRetirementCause::UnpublishedCandidate =>
                {
                    Some(retiring.clone())
                }
                _ => None,
            })
            .flatten();
        if let Some(retirement) = retirement {
            self.dispose_and_confirm_retirement(thread, &retirement)
                .await?;
        }
        Ok(())
    }

    /// Test projection through the production authorization and persistence
    /// owner. Tests keep the durable-adoption and direct-thread cause/effect
    /// rows without restoring the removed `owns` compatibility protocol.
    #[cfg(test)]
    pub(crate) async fn persist_environment_before_publish(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
    ) -> Result<BoundSessionEnvironmentIdentity, HostError> {
        let effect = self
            .authorize_environment_effect_before_io(
                thread,
                candidate.effect_kind(),
                candidate.source_binding(),
            )
            .await?;
        self.persist_authorized_environment_before_publish(thread, candidate, &effect)
            .await
            .map_err(|failure| failure.error)
    }
}
