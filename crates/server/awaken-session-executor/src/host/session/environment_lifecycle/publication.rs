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
        self.session_slots.update(thread, |slot| {
            slot.environment_owner.begin_preparing(
                environment,
                awaken_session_contract::SessionEnvironmentEffectKind::Create,
            )
        })
    }

    pub(crate) fn begin_session_environment_adoption(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<UnboundSessionEnvironment, HostError> {
        self.session_slots.update(thread, |slot| {
            slot.environment_owner.begin_preparing(
                environment,
                awaken_session_contract::SessionEnvironmentEffectKind::Adopt,
            )
        })
    }

    pub(crate) fn publish_prepared_session_environment(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
        identity: BoundSessionEnvironmentIdentity,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        self.session_slots.update(thread, |slot| {
            slot.environment_owner.publish_prepared(candidate, identity)
        })
    }

    #[cfg(test)]
    pub(crate) fn prepared_session_environment(
        &self,
        thread: &str,
    ) -> Option<UnboundSessionEnvironment> {
        self.session_slots
            .read(thread, |slot| match &slot.environment_owner {
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
            .read(thread, |slot| match &slot.environment_owner {
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
        let source_binding = match &candidate.origin {
            crate::session_slot::UnboundSessionEnvironmentOrigin::New => None,
            crate::session_slot::UnboundSessionEnvironmentOrigin::Adoption
            | crate::session_slot::UnboundSessionEnvironmentOrigin::DurableAdoption(_) => {
                Some(candidate.binding.as_str())
            }
        };
        let effect = self
            .authorize_environment_effect_before_io(thread, candidate.effect_kind(), source_binding)
            .await?;
        self.persist_authorized_environment_before_publish(thread, candidate, &effect)
            .await
            .map_err(|failure| failure.error)
    }
}
