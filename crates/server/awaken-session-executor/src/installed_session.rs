// @formal entrypoint session_realization.concurrent_drivers_preserve_one_owner_fence
// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
//! Process-local realization state for one Session.
//!
//! The frozen manifest remains the durable authority. This private slot owns every
//! projection materialized from it so registration and terminal cleanup share one
//! lifecycle boundary instead of coordinating parallel maps.

use crate::frozen_inputs::FrozenSessionInputs;
use crate::realization_error::{RealizationError, RealizationLock};

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::session_realizations::SessionRealization;
/// The exclusion every realization phase driver must hold is contract-owned
/// (`SessionRealizationPermit`); this crate only supplies the Session slot
/// allocation as its identity.
mod realization_permit;

/// One Session's process-local state, in three layers by lifetime. The
/// Session aggregate stays the sole authority; every layer is a rebuildable
/// projection of it or a resource this process runs. Dependencies point one
/// way: `run` reads `realization` and `frozen`, `realization` reads `frozen`.
#[derive(Default)]
pub(crate) struct InstalledSession {
    /// The Session's frozen inputs, held whole. Install and every later change
    /// replace this one value; nothing amends a field of it in place.
    pub(crate) inputs: Arc<FrozenSessionInputs>,
    /// What this process realized for the Session: its Environment owner,
    /// realization lease, staged Resources and the locks that serialize them.
    pub(crate) realization: SessionRealization,
    /// The Session's running context and the dispatch it currently serves.
    pub(crate) run: SessionRunBinding,
}

/// The run layer: the Session context built for running and the durable
/// dispatch it serves. Rebuilt from the other two layers when dropped.
#[derive(Default)]
pub(crate) struct SessionRunBinding {
    pub context: Option<Arc<crate::host::SessionCtx>>,
    pub deferred_executor: Option<Arc<dyn awaken_tool_contract::tool::ToolExecutor>>,
    /// Current durable dispatch claim used by claim-fenced Resource effects.
    /// This process-local projection is replaced at every claimed resolve; the
    /// dispatch store remains the lease/epoch authority.
    pub dispatch_claim: Option<awaken_dispatch_contract::RunClaim>,
    /// The thread entered through the Session application API and its durable
    /// dispatch must therefore cross the claimed Session realization boundary.
    /// It carries no desired-state data of its own.
    pub session_dispatch: bool,
}

impl InstalledSession {
    // Run after releasing the original slot mutex, before destroying a removed
    // slot. Ordinary removal and exact retirement share these same effects.
    fn cancel_preparation_and_notify_removal(&self) {
        if let Some(preparation) = &self.realization.staging.preparation {
            preparation.cancel();
        }
        self.realization.execution.changed.notify_waiters();
    }
}

impl crate::session_realizations::SessionRunQuery for SessionRunBinding {
    fn holds_context(&self) -> bool {
        self.context.is_some()
    }

    fn has_active_run(&self) -> bool {
        self.context
            .as_ref()
            .is_some_and(|runtime| runtime.has_active_run())
    }

    fn session_dispatch(&self) -> bool {
        self.session_dispatch
    }

    fn dispatch_claim(&self) -> Option<&awaken_dispatch_contract::RunClaim> {
        self.dispatch_claim.as_ref()
    }

    fn drop_context(&mut self) {
        self.context = None;
    }

    fn install_dispatch_claim(&mut self, claim: awaken_dispatch_contract::RunClaim) {
        self.dispatch_claim = Some(claim);
    }

    fn enter_session_dispatch(&mut self) {
        self.session_dispatch = true;
    }
}

impl crate::session_realizations::SessionRealizationSource for InstalledSessions {
    fn session_ids(&self) -> Result<Vec<String>, RealizationLock> {
        Ok(self
            .slots
            .lock()
            .map_err(|_| RealizationLock::InstalledSessions)?
            .keys()
            .cloned()
            .collect())
    }

    fn remove_session(&self, session: &str) -> Result<bool, RealizationLock> {
        Ok(self
            .try_remove(session)
            .map_err(|_| RealizationLock::InstalledSessions)?
            .is_some())
    }

    fn acquire_realization<'a>(
        &'a self,
        session: &'a str,
        admit: &'a mut (
                    dyn FnMut(crate::session_realizations::SessionRealizationAccess<'_>) + Send
                ),
    ) -> futures_util::future::BoxFuture<
        'a,
        awaken_session_realization_contract::SessionRealizationPermit,
    > {
        Box::pin(async move {
            self.acquire_realization_with(session, |slot| {
                admit(crate::session_realizations::SessionRealizationAccess {
                    inputs: &slot.inputs,
                    realization: &mut slot.realization,
                    run: &mut slot.run,
                })
            })
            .await
            .0
        })
    }

    fn install(&self, session: &str) -> Result<(), RealizationLock> {
        self.visit_slot(session, true, |_| ())
            .map_err(|_| RealizationLock::InstalledSessions)?;
        Ok(())
    }

    fn visit(
        &self,
        session: &str,
        visit: &mut dyn FnMut(crate::session_realizations::SessionRealizationAccess<'_>),
    ) -> Result<bool, RealizationLock> {
        Ok(self
            .visit_slot(session, false, |slot| {
                visit(crate::session_realizations::SessionRealizationAccess {
                    inputs: &slot.inputs,
                    realization: &mut slot.realization,
                    run: &mut slot.run,
                });
            })
            .map_err(|_| RealizationLock::InstalledSessions)?
            .is_some())
    }

    fn replace_frozen(
        &self,
        session: &str,
        update: &mut dyn FnMut(&mut FrozenSessionInputs),
    ) -> Result<(), RealizationLock> {
        self.visit_slot(session, true, |slot| {
            slot.replace_frozen_with(|inputs| update(inputs))
        })
        .map_err(|_| RealizationLock::InstalledSessions)?;
        Ok(())
    }

    fn finish_realization_revocation(
        &self,
        session: &str,
        selected: &crate::session_realizations::SessionRealizationRevocationSelection,
        expected_owner: &awaken_session_realization::environment_owner::SessionEnvironmentOwner,
        disposed: bool,
    ) -> bool {
        let (removed, discarded_run) = {
            // A poisoned registry denies completion; cleanup stays pending.
            let Ok(mut slots) = self.slots.lock() else {
                return false;
            };
            let Some(slot) = slots.get_mut(session) else {
                return false;
            };
            if !selected.matches_completion(&slot.realization, expected_owner, disposed) {
                return false;
            }
            use awaken_session_realization::environment_owner::SessionEnvironmentOwner;
            if matches!(
                slot.realization.environment.owner,
                SessionEnvironmentOwner::Vacant
            ) {
                (slots.remove(session), None)
            } else {
                slot.inputs = Arc::new(slot.inputs.retained_on_revocation());
                slot.realization = slot.realization.take_retained_on_revocation();
                (None, Some(std::mem::take(&mut slot.run)))
            }
        };
        // Release the canonical slot mutex before dropping retained Runtime or
        // physical wrappers, as in the original remove() operation.
        if let Some(slot) = &removed {
            slot.cancel_preparation_and_notify_removal();
        }
        drop((removed, discarded_run));
        true
    }

    fn retain_publication(
        &self,
        session: &str,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), RealizationError> {
        // Retaining the frozen publication installs frozen inputs: install path.
        self.visit_slot(session, true, |slot| {
            slot.inputs.validate_publication(publication)?;
            if let Some(publication) = publication {
                slot.replace_frozen_with(|next| {
                    next.published_snapshot = Some(publication.clone());
                });
            }
            Ok(())
        })
        .map_err(|_| RealizationLock::InstalledSessions)?
        .unwrap_or(Ok(()))
    }

    fn publish_resource_requirements(
        &self,
        session: &str,
        workspace: &str,
        asserted: Option<&awaken_session_configuration_contract::resource::SessionRealizationLease>,
        compiled: crate::staged_resources::CompiledEffectiveInputs,
        versions: Vec<awaken_resource_contract::SkillVersion>,
        effect_key: String,
    ) -> Result<(), RealizationError> {
        self.visit_slot(session, false, |slot| {
            // The lease check and correlated cache publication share the slot
            // lock with renewal rejection. Neither a late read nor cancellation
            // may publish requirements after that rejection has linearized.
            slot.realization
                .validate_preparation_publication(asserted)?;
            slot.replace_frozen_with(|next| next.workspace = Some(workspace.to_owned()));
            slot.realization.staging.resources = compiled.staged;
            slot.realization.staging.memory_bindings = compiled.memory_bindings;
            slot.realization.staging.memory = None;
            slot.realization.staging.skills = Some(versions);
            slot.realization.staging.effect_key = Some(effect_key);
            Ok::<_, RealizationError>(())
        })
        .map_err(|_| RealizationLock::InstalledSessions)?
        .ok_or_else(|| RealizationError::session_absent(session))?
    }
}

impl SessionRunBinding {
    /// Drop a metadata-only coordination envelope so the next read rebuilds
    /// it; an executable context is kept.
    pub(crate) fn drop_coordination_envelope(&mut self) {
        if self
            .context
            .as_ref()
            .is_some_and(|context| context.is_coordination_envelope())
        {
            self.context = None;
        }
    }
}

impl InstalledSession {
    /// The only write of the frozen inputs: build the next value from a copy
    /// and publish it whole; readers holding the previous `Arc` keep it. The
    /// runtime context is assembled once with its request prefix, so a changed
    /// prefix drops only that cached context.
    pub(crate) fn replace_frozen_with(&mut self, build: impl FnOnce(&mut FrozenSessionInputs)) {
        let mut next = self.inputs.as_ref().clone();
        build(&mut next);
        if self.inputs.request_context != next.request_context {
            self.run.context = None;
        }
        self.inputs = Arc::new(next);
    }
}

#[derive(Default)]
pub(crate) struct InstalledSessions {
    slots: Mutex<HashMap<String, InstalledSession>>,
}

/// One closed root and the original local Run controls selected with it in
/// the same slot mutation. This ephemeral value carries no durable or global
/// cancellation authority; an unreadable foreground record is kept as its
/// refusal so only a caller that must signal the Run is denied.
pub(crate) struct ClosedSessionRealization {
    pub(crate) root: crate::session_realizations::SessionRealizationRevocationSelection,
    pub(crate) run: Result<
        Option<awaken_dispatch_execution::SelectedAttemptInterruption>,
        crate::host::ForegroundRunPoisoned,
    >,
    #[cfg(test)]
    pub(crate) after_check: Option<Box<dyn FnOnce() + Send>>,
}

impl ClosedSessionRealization {
    /// Close the root through `close` and capture its local Run controls
    /// inside the caller's slot mutation, without another read.
    fn select(
        slot: &mut InstalledSession,
        close: impl FnOnce(
            &mut SessionRealization,
        )
            -> Option<crate::session_realizations::SessionRealizationRevocationSelection>,
    ) -> Option<Self> {
        Some(Self {
            root: close(&mut slot.realization)?,
            run: slot
                .run
                .select_interruption()
                .map(|selected| selected.local),
            #[cfg(test)]
            after_check: None,
        })
    }
}

/// The immutable controls of one observed Run, selected before any await.
#[derive(Default)]
pub(crate) struct SelectedRunInterruption {
    pub(crate) resident: Option<Arc<crate::host::SessionCtx>>,
    pub(crate) local: Option<awaken_dispatch_execution::SelectedAttemptInterruption>,
}

impl SessionRunBinding {
    /// Called under the original Slot lock. Lock order is Slot, foreground
    /// record, Runtime registry; no await occurs inside it. The foreground
    /// identity and signal are one observed value, so the selection can never
    /// pair one Run with another attempt's signal.
    pub(crate) fn select_interruption(
        &self,
    ) -> Result<SelectedRunInterruption, crate::host::ForegroundRunPoisoned> {
        let local = match &self.context {
            Some(ctx) => Some(
                awaken_dispatch_execution::LiveAttemptControl::new(ctx.runtime.clone())
                    .select_interruption(
                        ctx.foreground.observe()?,
                        self.dispatch_claim
                            .as_ref()
                            .map(|claim| claim.run_id.clone()),
                    ),
            ),
            None => None,
        };
        Ok(SelectedRunInterruption {
            resident: self.context.clone(),
            local,
        })
    }
}

impl InstalledSessions {
    fn visit_slot<R>(
        &self,
        session: &str,
        create: bool,
        f: impl FnOnce(&mut InstalledSession) -> R,
    ) -> Result<Option<R>, PoisonError<()>> {
        let mut slots = self.slots.lock().map_err(|_| PoisonError::new(()))?;
        let slot = if create {
            Some(slots.entry(session.to_string()).or_default())
        } else {
            slots.get_mut(session)
        };
        Ok(slot.map(f))
    }

    /// Every enrolled scoped execution Environment, for realization renewal.
    #[cfg(target_os = "linux")]
    pub(crate) fn execution_environments(
        &self,
    ) -> Vec<Arc<crate::session_environment::SessionEnvironment>> {
        self.slots
            .lock()
            .expect("session runtime slots mutex poisoned")
            .values()
            .filter_map(|slot| slot.realization.environment.owner.execution_environment())
            .filter(|environment| environment.scoped_execution().is_some())
            .cloned()
            .collect()
    }

    /// The registry itself, for tests that hold or poison its lock.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn test_map(&self) -> &Mutex<HashMap<String, InstalledSession>> {
        &self.slots
    }

    /// The realizer's view of these Sessions.
    pub(crate) fn realizations(
        self: &Arc<Self>,
    ) -> crate::session_realizations::SessionRealizations {
        crate::session_realizations::SessionRealizations::new(self.clone())
    }

    pub(crate) fn close_realization(
        &self,
        session: &str,
        expected: Option<(
            &crate::session_resource::SessionRealizationLease,
            &Arc<tokio::sync::Mutex<()>>,
        )>,
    ) -> Option<ClosedSessionRealization> {
        self.modify(session, |slot| {
            if expected.is_some_and(|(_, realization)| {
                !Arc::ptr_eq(&slot.realization.execution.phase, realization)
            }) {
                return None;
            }
            ClosedSessionRealization::select(slot, |realization| {
                crate::session_realizations::SessionRealizationRevocationSelection::close(
                    realization,
                    expected.map(|(lease, _)| lease),
                )
            })
        })
        .flatten()
    }

    /// Close this original root's generation through the realization's own
    /// terminal-absence law and capture its local Run controls inside this
    /// same slot mutation.
    pub(crate) fn close_realization_generation(
        &self,
        session: &str,
        lease: &crate::session_resource::SessionRealizationLease,
        phase: &Arc<tokio::sync::Mutex<()>>,
    ) -> Option<ClosedSessionRealization> {
        self.modify(session, |slot| {
            ClosedSessionRealization::select(slot, |realization| {
                crate::session_realizations::SessionRealizationRevocationSelection::close_generation(
                    realization,
                    lease,
                    phase,
                )
            })
        })
        .flatten()
    }

    /// One linearization point closes every currently installed projection
    /// before any caller may wait for an async lifecycle lock or drain.
    pub(crate) fn close_all_realizations(&self) -> Vec<(String, ClosedSessionRealization)> {
        self.slots
            .lock()
            .expect("session runtime slots mutex poisoned")
            .iter_mut()
            .map(|(id, slot)| {
                (
                    id.clone(),
                    ClosedSessionRealization::select(slot, |realization| {
                        crate::session_realizations::SessionRealizationRevocationSelection::close(
                            realization,
                            None,
                        )
                    })
                    .expect("unconditional close always selects the current slot"),
                )
            })
            .collect()
    }

    /// Test set-up: install the Session and change its realization layer.
    #[cfg(test)]
    pub(crate) fn install_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRealization) -> R,
    ) -> R {
        self.update(session, |slot| f(&mut slot.realization))
    }

    /// The host's install path: install an empty Session slot when absent.
    /// Realization reads, writes and lock acquisitions never install one.
    pub(crate) fn install(&self, session: &str) {
        self.update(session, |_| ());
    }

    /// Publish a replacement of one Session's frozen inputs, creating its slot.
    pub(crate) fn replace_frozen_with(
        &self,
        session: &str,
        build: impl FnOnce(&mut FrozenSessionInputs),
    ) {
        self.update(session, |slot| slot.replace_frozen_with(build));
    }

    pub fn read<R>(&self, session: &str, f: impl FnOnce(&InstalledSession) -> R) -> Option<R> {
        self.visit_slot(session, false, |slot| f(slot))
            .expect("session runtime slots mutex poisoned")
    }

    pub fn update<R>(&self, session: &str, f: impl FnOnce(&mut InstalledSession) -> R) -> R {
        self.visit_slot(session, true, f)
            .expect("session runtime slots mutex poisoned")
            .expect("an installed Session is always visited")
    }

    pub fn modify<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut InstalledSession) -> R,
    ) -> Option<R> {
        self.visit_slot(session, false, f)
            .expect("session runtime slots mutex poisoned")
    }

    /// Read only the run layer of one Session's slot.
    pub(crate) fn read_run<R>(
        &self,
        session: &str,
        f: impl FnOnce(&SessionRunBinding) -> R,
    ) -> Option<R> {
        self.read(session, |slot| f(&slot.run))
    }

    /// Change only the run layer, creating the Session's slot.
    pub(crate) fn update_run<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRunBinding) -> R,
    ) -> R {
        self.update(session, |slot| f(&mut slot.run))
    }

    /// Change only the run layer of an existing slot.
    pub(crate) fn modify_run<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRunBinding) -> R,
    ) -> Option<R> {
        self.modify(session, |slot| f(&mut slot.run))
    }

    #[cfg(test)]
    pub fn remove(&self, session: &str) -> Option<InstalledSession> {
        self.try_remove(session)
            .expect("session runtime slots mutex poisoned")
    }

    fn try_remove(&self, session: &str) -> Result<Option<InstalledSession>, PoisonError<()>> {
        let removed = self
            .slots
            .lock()
            .map_err(|_| PoisonError::new(()))?
            .remove(session);
        if let Some(slot) = &removed {
            slot.cancel_preparation_and_notify_removal();
        }
        Ok(removed)
    }

    pub fn realization_leases(
        &self,
    ) -> Vec<(
        String,
        awaken_session_realization_contract::ObservedSessionRealizationLease,
    )> {
        self.slots
            .lock()
            .expect("session runtime slots mutex poisoned")
            .iter()
            .filter_map(|(session_id, slot)| {
                slot.realization
                    .execution
                    .authority
                    .proof()
                    .cloned()
                    .map(|lease| (session_id.clone(), lease))
            })
            .collect()
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.slots
            .lock()
            .expect("session runtime slots mutex poisoned")
            .keys()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub fn contains(&self, session: &str) -> bool {
        self.slots
            .lock()
            .expect("session runtime slots mutex poisoned")
            .contains_key(session)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod owner_release_tests;
