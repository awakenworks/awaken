//! Original Host composition tests over canonical realization code.
use super::*;
use crate::environment_owner::{
    BoundSessionEnvironment, BoundSessionEnvironmentIdentity, LegacyDirectEnvironmentProvenance,
    RetiringEnvironmentOwner, RetiringSessionEnvironment, SessionEnvironmentOwner,
    SessionEnvironmentPreparation, SessionEnvironmentRetirementCause, UnboundSessionEnvironment,
};
use crate::host_error::HostError;
use crate::session_environment::SessionEnvironmentAdoptionDisposition;

use state::RetirementSelection;
use std::sync::Arc;

use awaken_session_realization::environment_lifecycle::*;

#[cfg(test)]
#[path = "environment_lifecycle/tests.rs"]
mod tests;

#[cfg(test)]
impl crate::host::SharedHost {
    pub async fn session_environment_handle(
        &self,
        thread: &str,
    ) -> Option<awaken_provisioning_contract::SandboxHandle> {
        self.realizer
            .session_environment(thread)
            .await
            .map(|env| env.lifecycle().handle())
    }

    pub fn install_session_environment_owner_projection(
        &self,
        thread: &str,
        workspace_id: &str,
        state: &awaken_session_state_contract::SessionEnvironmentState,
    ) -> Result<(), HostError> {
        self.realizer.sessions.update_session(thread, |slot| {
            let owner = slot.realization.environment.owner.after_projection(
                thread,
                workspace_id,
                state,
                false,
                false,
            )?;
            slot.realization
                .install_owner(owner, state, slot.run.holds_context());
            Ok(())
        })
    }

    pub fn install_test_resident_session_environment(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
    ) {
        let realizations = self.realizer.sessions.clone();
        let binding = serde_json::to_string(&environment.lifecycle().handle())
            .expect("test Environment handle serializes");
        let receipt = awaken_session_state_contract::SessionEnvironmentReceipt::new(
            thread,
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
            binding.clone(),
            None,
        );
        realizations.update_realization(thread, |r| {
            // A same-handle ABA test replaces only the process-local Arc. Keep
            // its existing Root-generated identity instead of downgrading it.
            if let SessionEnvironmentOwner::Resident(owned) = &mut r.environment.owner
                && owned.binding == binding
                && owned.environment.lifecycle().handle() == environment.lifecycle().handle()
            {
                owned.environment = environment;
                return;
            }
            r.environment.owner = SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
                identity: BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::Direct(receipt),
                ),
                binding,
                environment,
            });
        });
    }

    pub fn observe_retirement_status<E>(
        &self,
        thread: &str,
        retirement: &RetiringSessionEnvironment,
        status: Result<awaken_provisioning_contract::SandboxStatus, E>,
    ) -> Result<bool, E> {
        let realizations = self.realizer.sessions.clone();
        match realizations.modify_realization(thread, |r| {
            r.environment
                .owner
                .observe_retirement_status(retirement, status)
        }) {
            Some(observed) => observed,
            None => Ok(false),
        }
    }

    pub async fn stop_and_confirm_retirement(
        &self,
        thread: &str,
        retirement: &RetiringSessionEnvironment,
    ) -> Result<bool, HostError> {
        use awaken_session_environment::test_support::SandboxEnvironmentAccess as _;
        let environment = retirement.owned.environment();
        environment
            .lifecycle()
            .stop_bound_processes()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        self.observe_retirement_status(
            thread,
            retirement,
            environment.sandbox_environment().status().await,
        )
        .map_err(|error| HostError::internal(error.to_string()))
    }

    pub async fn discard_session_environment(
        &self,
        thread: &str,
        expected: &Arc<crate::session_environment::SessionEnvironment>,
    ) -> bool {
        let realizations = self.realizer.sessions.clone();
        let _lifecycle = realizations.lock_lifecycle(thread).await;
        let Ok(Some(retirement)) = self.realizer.retire_current_environment(
            thread,
            SessionEnvironmentRetirementCause::RecoveryDiscard,
            RetirementSelection::Exact(expected),
        ) else {
            return false;
        };
        self.stop_and_confirm_retirement(thread, &retirement.owner)
            .await
            .unwrap_or(false)
    }

    pub fn prepared_session_environment(&self, thread: &str) -> Option<UnboundSessionEnvironment> {
        let realizations = self.realizer.sessions.clone();
        realizations
            .read_realization(thread, |r| match &r.environment.owner {
                SessionEnvironmentOwner::Preparing(SessionEnvironmentPreparation::Candidate(
                    candidate,
                )) => Some(candidate.clone()),
                _ => None,
            })
            .flatten()
    }

    pub async fn persist_environment_before_publish(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
    ) -> Result<BoundSessionEnvironmentIdentity, HostError> {
        let effect = self
            .realizer
            .authorize_environment_effect_before_io(thread, candidate.ordinary_intent()?)
            .await?;
        self.realizer
            .persist_authorized_environment_before_publish(thread, candidate, &effect)
            .await
            .map_err(|failure| failure.error)
    }
}
