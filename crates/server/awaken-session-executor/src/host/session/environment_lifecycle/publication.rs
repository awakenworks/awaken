//! Publication edge of the one Session Environment lifecycle owner.
//!
//! This is a private split of `environment_lifecycle`, not a second owner:
//! every transition still writes the same [`SessionEnvironmentOwner`] under
//! the Session lifecycle guard.

use super::state::committed_identity;
use super::*;
use crate::session_slot::UnboundSessionEnvironmentOrigin;

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

    /// Resume the one create/adopt candidate through the existing idempotent
    /// repository, dispatch-binding, durable-binding, and publication edges.
    /// No provider creation happens here, so a cancelled Future retries the
    /// exact retained Arc instead of creating a parallel physical owner.
    pub(crate) async fn complete_prepared_session_environment(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
        bind_deferred_dispatch: bool,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let environment = candidate.environment.clone();
        if candidate.requires_initial_provisioning()
            && let Err(error) = self
                .realize_thread_repositories(thread, environment.as_ref())
                .await
        {
            let _ = self
                .dispose_unpublished_session_environment(thread, &environment)
                .await;
            return Err(error);
        }
        if candidate.requires_initial_provisioning() && bind_deferred_dispatch {
            match self
                .bind_deferred_dispatch_before_publish(thread, &candidate.binding)
                .await
            {
                Ok(_) => {}
                Err(error) => {
                    let _ = self
                        .dispose_unpublished_session_environment(thread, &environment)
                        .await;
                    return Err(error);
                }
            }
        }
        let identity = match self
            .persist_environment_before_publish(thread, candidate)
            .await
        {
            Ok(identity) => identity,
            Err(error) => {
                // Commit outcome may be unknown. Retain the exact hidden
                // candidate so retry reads the Store authority and never
                // disposes a binding that may already be durable.
                return Err(error);
            }
        };
        self.publish_prepared_session_environment(thread, candidate, identity)
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

    /// Persist before publication and return only Store-read committed identity.
    /// Direct Threads receive an explicit legacy/direct provenance instead of a
    /// fabricated durable generation. A direct adoption of an already durable
    /// binding retains the exact projected identity rather than relabeling it as
    /// a new local effect.
    pub(crate) async fn persist_environment_before_publish(
        &self,
        thread: &str,
        candidate: &UnboundSessionEnvironment,
    ) -> Result<BoundSessionEnvironmentIdentity, HostError> {
        let binding = serde_json::to_string(&candidate.environment.handle())
            .map_err(|error| HostError::internal(error.to_string()))?;
        let sink = self
            .environment_binding_sink
            .read()
            .expect("environment binding sink lock poisoned")
            .clone();
        let sink_owns_thread = match sink.as_ref() {
            Some(sink) => sink
                .owns(thread)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?,
            None => false,
        };
        if !sink_owns_thread {
            if let UnboundSessionEnvironmentOrigin::DurableAdoption(identity) = &candidate.origin {
                return Ok(identity.clone());
            }
            let receipt = awaken_session_contract::SessionEnvironmentReceipt::new(
                thread,
                candidate.effect_kind(),
                binding,
                None,
            );
            return Ok(BoundSessionEnvironmentIdentity::LegacyDirect(
                LegacyDirectEnvironmentProvenance::Direct(receipt),
            ));
        }
        let sink = sink.expect("an owning environment binding sink is installed");
        let mut realization = self
            .session_slots
            .read(thread, |slot| slot.realization_lease.clone())
            .flatten();
        // FMECA/causal graph: C1 a slow realization finishes under lease L1;
        // C2 heartbeat/reclaim projects L2 before its receipt commits; C3 a
        // second renewal projects L3 while the L2 retry is in flight. E1 never
        // publishes under stale authority; E2 follows every exact notification;
        // E3 consumes only Store-read committed generation evidence.
        const FENCE_CATCH_UP_ATTEMPTS: usize = 4;
        let mut persistence_error = None;
        for attempt in 0..FENCE_CATCH_UP_ATTEMPTS {
            let receipt = awaken_session_contract::SessionEnvironmentReceipt::new(
                thread,
                candidate.effect_kind(),
                binding.clone(),
                realization.clone(),
            );
            match sink.persist(receipt.clone()).await {
                Ok(committed) => return committed_identity(&receipt, committed),
                Err(error)
                    if error.code == "session_realization_stale"
                        && attempt + 1 < FENCE_CATCH_UP_ATTEMPTS =>
                {
                    let changed = self
                        .session_slots
                        .update(thread, |slot| slot.realization_changed.clone());
                    let replacement =
                        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            loop {
                                let notified = changed.notified();
                                let current = self
                                    .session_slots
                                    .read(thread, |slot| slot.realization_lease.clone())
                                    .flatten();
                                if current.is_some() && current != realization {
                                    break current;
                                }
                                notified.await;
                            }
                        })
                        .await;
                    match replacement {
                        Ok(current) => realization = current,
                        Err(_) => {
                            persistence_error = Some(error);
                            break;
                        }
                    }
                }
                Err(error) => {
                    persistence_error = Some(error);
                    break;
                }
            }
        }
        let error = persistence_error.unwrap_or_else(|| {
            awaken_session_contract::RunError::internal(
                "Session environment binding fence catch-up exhausted",
            )
        });
        Err(HostError::internal(error.to_string()))
    }
}
