// @formal evidence session_environment.terminal_fence_admits_only_inflight_binding_receipts
//! Authorized Environment effects and durable publication.
//!
//! The realizer uses the installed Session's original claim and realization
//! authority, the selected binding sink and the same dispatch execution role.
//! No queue discovery or Host callback participates in publication.

use std::sync::Arc;

use crate::environment_owner::{
    BoundSessionEnvironmentIdentity, LegacyDirectEnvironmentProvenance, UnboundSessionEnvironment,
};
use crate::host_error::HostError;
use crate::session_environment::{
    AuthorizedSessionEnvironmentEffect, EnvironmentBindingPersistenceError,
    SessionEnvironmentProvider,
};
use awaken_session_state_contract::{
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentEffectKind,
};

impl crate::realization_executor::RealizationExecutor {
    /// Select the environment provider from the immutable publication already
    /// frozen for this Session. Callers without an Agent parameter (deferred
    /// tool realization and terminal cleanup) must use this path rather than a
    /// process-wide default.
    pub(crate) fn projected_session_environment_provider(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<&SessionEnvironmentProvider, HostError> {
        let provisioning = self
            .publication
            .projected_session_model_provisioning(thread, agent)?;
        self.environments
            .session_environment_provider_for_thread(thread, &provisioning)
    }
    pub(crate) async fn authorize_environment_effect_before_io(
        &self,
        thread: &str,
        intent: &SessionEnvironmentEffectIntent,
    ) -> Result<AuthorizedSessionEnvironmentEffect, HostError> {
        let realizations = &self.environments.sessions;
        intent
            .verify()
            .map_err(|error| HostError::internal(error.to_string()))?;
        if intent.session_id().as_str() != thread {
            return Err(HostError::internal(
                "Environment request names another Session",
            ));
        }
        let (claimed_ordinary_run, managed_projection, closed) = realizations
            .modify_session(thread, |slot| {
                let managed = slot.inputs.baseline.is_some() || slot.run.session_dispatch();
                let ordinary = slot.realization.authority.lease().is_none()
                    && slot.run.dispatch_claim().is_some()
                    && !managed;
                (ordinary, managed, slot.realization.authority.is_closed())
            })
            .unwrap_or_default();
        if closed {
            return Err(HostError::unavailable_classified(
                "session_environment_realization_closed",
                "closed Session realization cannot authorize an Environment effect",
            ));
        }
        let sink = self
            .environment_binding_sink
            .read()
            .expect("environment binding sink lock poisoned")
            .clone();
        // A claimed ordinary Run and a Managed Session already have distinct
        // durable owners. The former fences its local Environment through the
        // Run claim and `DispatchQueue::bind_sandbox`; only the latter may ask
        // the Session root to authorize/persist an aggregate Environment effect.
        // Sending an ordinary Run to a remote Session sink cannot be authorized
        // (it has no realization lease) and duplicates the queue-owned binding.
        let authorization = match sink {
            Some(_) if claimed_ordinary_run => SessionEnvironmentEffectAuthorization::Unowned {
                intent: Box::new(intent.clone()),
            },
            Some(sink) => sink
                .authorize(intent)
                .await
                .map_err(crate::host_error::from_run_error)?,
            None if !managed_projection && intent.realization().is_none() => {
                SessionEnvironmentEffectAuthorization::Unowned {
                    intent: Box::new(intent.clone()),
                }
            }
            None => {
                return Err(HostError::unavailable_classified(
                    "session_environment_binding_authority_missing",
                    "durable Session Environment effect has no binding authority",
                ));
            }
        };
        authorization
            .verify_for(intent)
            .map_err(|error| HostError::internal(error.to_string()))?;
        let original = authorization.intent();
        if matches!(
            authorization,
            SessionEnvironmentEffectAuthorization::Unowned { .. }
        ) && (managed_projection || original.realization().is_some())
        {
            return Err(HostError::unavailable_classified(
                "session_environment_owned_root_missing",
                "a Managed Session cannot fall back to an unowned Environment effect",
            ));
        }
        let provider_fence = if !matches!(
            authorization,
            SessionEnvironmentEffectAuthorization::Unowned { .. }
        ) {
            original
                .realization()
                .map(|realization| {
                    realization
                        .sandbox_effect_fence(original.effect_id())
                        .map_err(|error| HostError::internal(error.to_string()))
                })
                .transpose()?
        } else {
            None
        };
        let provider_authority = provider_fence
            .as_ref()
            .map(|fence| {
                self.environments
                    .execution
                    .provider_effect_authority(thread, fence)
                    .map(|source| {
                        Arc::new(source)
                            as Arc<dyn awaken_provisioning_contract::CurrentExecutionAuthority>
                    })
            })
            .transpose()?;
        Ok(AuthorizedSessionEnvironmentEffect {
            authorization,
            provider_fence,
            provider_authority,
        })
    }

    pub(crate) async fn persist_authorized_environment_before_publish(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<BoundSessionEnvironmentIdentity, EnvironmentBindingPersistenceError> {
        let captured = candidate.ordinary_intent()?;
        if captured != effect.authorization.intent() {
            return Err(HostError::internal(
                "candidate changed its original Environment effect intent",
            )
            .into());
        }
        if matches!(
            effect.authorization,
            SessionEnvironmentEffectAuthorization::Unowned { .. }
        ) && let Some(identity) = candidate.durable_identity()
        {
            return Ok(identity.clone());
        }
        self.persist_authorized_environment_binding(thread, candidate.binding.clone(), effect)
            .await
    }

    pub(crate) async fn persist_authorized_environment_binding(
        &self,
        thread: &str,
        binding: String,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<BoundSessionEnvironmentIdentity, EnvironmentBindingPersistenceError> {
        let receipt = self
            .environments
            .current_environment_receipt(binding.clone(), effect)?;
        // A Resource reservation extends the already-published Environment
        // handle under the Session root. It is not a Run placement and must not
        // rewrite `RunDispatch.sandbox`: the slot can legitimately retain the
        // preceding settled attempt's claim while an idle Session accepts a
        // live Resource generation. Create/Adopt/Rebuild remain the only raw
        // dispatch-cache writers and retain their exact claim fence below.
        let binds_deferred_dispatch = !matches!(
            effect.authorization.intent().kind(),
            SessionEnvironmentEffectKind::ResourceProjectionReservation { .. }
        );
        match &effect.authorization {
            SessionEnvironmentEffectAuthorization::Unowned { .. } => {
                if binds_deferred_dispatch {
                    self.bind_deferred_dispatch_before_publish(thread, &binding)
                        .await?;
                }
                return Ok(BoundSessionEnvironmentIdentity::LegacyDirect(
                    LegacyDirectEnvironmentProvenance::Direct(receipt),
                ));
            }
            SessionEnvironmentEffectAuthorization::AlreadyApplied {
                binding: committed, ..
            } => {
                if committed != &binding {
                    return Err(HostError::internal(
                        "Session Environment effect already committed another binding",
                    )
                    .into());
                }
            }
            SessionEnvironmentEffectAuthorization::Authorized { .. } => {}
        }
        let sink = self
            .environment_binding_sink
            .read()
            .expect("environment binding sink lock poisoned")
            .clone()
            .ok_or_else(|| {
                HostError::internal(
                    "authorized Session Environment effect has no durable binding sink",
                )
            })?;
        // Publishing needs Store-read generated identity, not merely a positive
        // authorization readback. If the response is lost, retain the exact
        // Candidate and retry this idempotent persist instead of inventing a
        // process-local generation.
        let committed = sink
            .persist(receipt.clone())
            .await
            .map_err(crate::host_error::from_run_error)?;
        let committed_identity =
            BoundSessionEnvironmentIdentity::from_committed_receipt(&receipt, committed)?;
        if binds_deferred_dispatch {
            self.bind_deferred_dispatch_before_publish(thread, &binding)
                .await
                .map_err(|error| EnvironmentBindingPersistenceError {
                    error,
                    committed_identity: Some(committed_identity.clone()),
                })?;
        }
        Ok(committed_identity)
    }

    async fn bind_deferred_dispatch_before_publish(
        &self,
        thread: &str,
        binding: &str,
    ) -> Result<bool, HostError> {
        let claim = self
            .environments
            .sessions
            .modify_session(thread, |slot| slot.run.dispatch_claim().cloned())
            .flatten();
        let Some(claim) = claim else {
            return Ok(false);
        };
        let outcome = self
            .dispatch_execution
            .as_ref()
            .ok_or_else(|| {
                HostError::internal("database-less Worker requires an injected dispatch binding")
            })?
            .bind_sandbox(&claim, binding)
            .await
            .map_err(|error| {
                HostError::unavailable_classified(
                    "dispatch_sandbox_binding_unavailable",
                    error.to_string(),
                )
            })?;
        if !outcome.applied() {
            return Err(HostError::unavailable_classified(
                "dispatch_claim_stale",
                "deferred sandbox binding was fenced by a replacement claim",
            ));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
