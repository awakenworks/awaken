// @formal evidence session_environment.terminal_fence_admits_only_inflight_binding_receipts
// Static owner: Runtime-host transition adapter over the single process-local Environment
// owner retained in the Session slot. Dynamic path: preparation, durable adoption, and
// publication all execute inside installed_sessions.update and delegate to that one owner; only
// its hidden prepared candidate can be published after the aggregate binding sink returns
// exact Store-read authority, while cleanup retry only consumes the owner's retirement
// projection. This module adds no registry, lease authority, or parallel Environment
// state. Evidence: session_environment.owner_topology_is_exact_and_frozen and adjacent
// lifecycle tests. Optional scoped candidates bind the current Worker/Session authority
// view at this same enrollment edge; selected-but-unbound terminal owners gain no
// execution grant. The source ledger remains this file's sole surface mapping.
//! Publication edge of the one Session Environment lifecycle owner.
//!
//! This is a private split of `environment_lifecycle`, not a second owner:
//! every transition still writes the same [`SessionEnvironmentOwner`] under
//! the Session lifecycle guard.

use super::{SessionEnvironmentOwner, SessionEnvironmentRetirementCause};
use crate::realization_error::{AuthorityLoss, PendingState, RealizationError, RealizationLock};
use std::sync::Arc;

use crate::environment_owner::{
    BoundSessionEnvironmentIdentity, LegacyDirectEnvironmentProvenance, UnboundSessionEnvironment,
};
use crate::session_environment::{
    AuthorizedSessionEnvironmentEffect, EnvironmentBindingPersistenceError,
    SessionEnvironmentProvider,
};
use awaken_session_state_contract::{
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentEffectKind,
};

impl crate::environment_holding::EnvironmentHolding {
    pub fn begin_session_environment_adoption(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        intent: Option<&awaken_session_state_contract::SessionEnvironmentEffectIntent>,
    ) -> Result<UnboundSessionEnvironment, RealizationError> {
        self.begin_session_environment_candidate(
            thread,
            environment,
            awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt,
            intent,
        )
    }

    fn begin_session_environment_candidate(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        kind: awaken_session_state_contract::SessionEnvironmentEffectKind,
        intent: Option<&awaken_session_state_contract::SessionEnvironmentEffectIntent>,
    ) -> Result<UnboundSessionEnvironment, RealizationError> {
        if intent.is_some_and(|intent| intent.session_id().as_str() != thread) {
            return Err(RealizationError::invariant(
                "candidate intent names another Session",
            ));
        }
        self.sessions.update_session(thread, |slot| {
            let candidate =
                slot.realization
                    .environment
                    .owner
                    .begin_preparing(environment, kind, intent)?;
            #[cfg(target_os = "linux")]
            self.execution
                .bind(thread, slot.realization, &candidate.environment)?;
            Ok(candidate)
        })?
    }

    pub fn begin_session_environment_preparation(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        intent: Option<&awaken_session_state_contract::SessionEnvironmentEffectIntent>,
    ) -> Result<UnboundSessionEnvironment, RealizationError> {
        self.begin_session_environment_candidate(
            thread,
            environment,
            intent.map_or(
                awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
                |intent| intent.kind().clone(),
            ),
            intent,
        )
    }

    pub fn publish_prepared_session_environment(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
        identity: BoundSessionEnvironmentIdentity,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, RealizationError> {
        self.sessions.update_realization(thread, |r| {
            r.environment.owner.publish_prepared(candidate, identity)
        })?
    }

    pub fn retain_committed_session_environment_candidate(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
        identity: BoundSessionEnvironmentIdentity,
    ) -> Result<UnboundSessionEnvironment, RealizationError> {
        self.sessions.update_realization(thread, |r| {
            r.environment
                .owner
                .retain_committed_candidate(candidate, identity)
        })?
    }

    /// Finish a previously interrupted unpublished cleanup before any caller
    /// may create or adopt another physical owner. A provider error, live
    /// status, cancellation, or ABA mismatch leaves the exact Retiring value.
    pub async fn retry_unpublished_session_environment_cleanup(
        &self,
        thread: &str,
    ) -> Result<(), RealizationError> {
        let retirement = self
            .sessions
            .read_realization(thread, |r| match &r.environment.owner {
                SessionEnvironmentOwner::Retiring(retiring)
                    if retiring.cause
                        == SessionEnvironmentRetirementCause::UnpublishedCandidate =>
                {
                    Some(retiring.clone())
                }
                _ => None,
            })?
            .flatten();
        if let Some(retirement) = retirement {
            self.dispose_and_confirm_retirement(thread, &retirement)
                .await?;
        }
        Ok(())
    }
}

impl crate::environment_holding::EnvironmentHolding {
    /// Select the environment provider from the immutable publication already
    /// frozen for this Session. Callers without an Agent parameter (deferred
    /// tool realization and terminal cleanup) must use this path rather than a
    /// process-wide default.
    pub fn projected_session_environment_provider(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<&SessionEnvironmentProvider, RealizationError> {
        let provisioning = self
            .publication
            .projected_session_model_provisioning(thread, agent)?;
        self.session_environment_provider_for_thread(thread, &provisioning)
    }
    pub async fn authorize_environment_effect_before_io(
        &self,
        thread: &str,
        intent: &SessionEnvironmentEffectIntent,
    ) -> Result<AuthorizedSessionEnvironmentEffect, RealizationError> {
        let realizations = &self.sessions;
        intent.verify().map_err(RealizationError::from)?;
        if intent.session_id().as_str() != thread {
            return Err(RealizationError::invariant(
                "Environment request names another Session",
            ));
        }
        let (claimed_ordinary_run, managed_projection, closed) = realizations
            .modify_session(thread, |slot| {
                let managed = slot.inputs.baseline.is_some() || slot.run.session_dispatch();
                let ordinary = slot.realization.execution.authority.lease().is_none()
                    && slot.run.dispatch_claim().is_some()
                    && !managed;
                (
                    ordinary,
                    managed,
                    slot.realization.execution.authority.is_closed(),
                )
            })?
            .unwrap_or_default();
        if closed {
            return Err(RealizationError::authority_lost(
                AuthorityLoss::RealizationClosed,
                "closed Session realization cannot authorize an Environment effect",
            ));
        }
        let sink = self
            .environment_binding_sink
            .read()
            .map_err(|_| RealizationError::Poisoned(RealizationLock::EnvironmentBindingSink))?
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
                .map_err(RealizationError::Run)?,
            None if !managed_projection && intent.realization().is_none() => {
                SessionEnvironmentEffectAuthorization::Unowned {
                    intent: Box::new(intent.clone()),
                }
            }
            None => {
                return Err(RealizationError::authority_lost(
                    AuthorityLoss::BindingAuthorityMissing,
                    "durable Session Environment effect has no binding authority",
                ));
            }
        };
        authorization
            .verify_for(intent)
            .map_err(RealizationError::from)?;
        let original = authorization.intent();
        if matches!(
            authorization,
            SessionEnvironmentEffectAuthorization::Unowned { .. }
        ) && (managed_projection || original.realization().is_some())
        {
            return Err(RealizationError::pending(
                PendingState::OwnedRootMissing,
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
                        .map_err(RealizationError::from)
                })
                .transpose()?
        } else {
            None
        };
        let provider_authority = provider_fence
            .as_ref()
            .map(|fence| {
                self.execution
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

    pub async fn persist_authorized_environment_before_publish(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<BoundSessionEnvironmentIdentity, EnvironmentBindingPersistenceError> {
        let captured = candidate.ordinary_intent()?;
        if captured != effect.authorization.intent() {
            return Err(RealizationError::invariant(
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

    pub async fn persist_authorized_environment_binding(
        &self,
        thread: &str,
        binding: String,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<BoundSessionEnvironmentIdentity, EnvironmentBindingPersistenceError> {
        let receipt = self
            .execution
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
                    return Err(RealizationError::invariant(
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
            .map_err(|_| RealizationError::Poisoned(RealizationLock::EnvironmentBindingSink))?
            .clone()
            .ok_or_else(|| {
                RealizationError::invariant(
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
            .map_err(RealizationError::Run)?;
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

    pub async fn bind_deferred_dispatch_before_publish(
        &self,
        thread: &str,
        binding: &str,
    ) -> Result<bool, RealizationError> {
        let claim = self
            .sessions
            .modify_session(thread, |slot| slot.run.dispatch_claim().cloned())?
            .flatten();
        let Some(claim) = claim else {
            return Ok(false);
        };
        let outcome = self
            .dispatch_execution
            .as_ref()
            .ok_or_else(|| {
                RealizationError::invariant(
                    "database-less Worker requires an injected dispatch binding",
                )
            })?
            .bind_sandbox(&claim, binding)
            .await
            .map_err(|error| {
                RealizationError::pending(
                    PendingState::DispatchSandboxBindingUnavailable,
                    error.to_string(),
                )
            })?;
        if !outcome.applied() {
            return Err(RealizationError::authority_lost(
                AuthorityLoss::ClaimStale,
                "deferred sandbox binding was fenced by a replacement claim",
            ));
        }
        Ok(true)
    }
}
