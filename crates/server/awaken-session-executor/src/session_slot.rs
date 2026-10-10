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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) use crate::environment_owner::*;
mod execution_authority;
pub(crate) use crate::session_realizations::{
    FrozenProjectionReceipt, SessionRealizationAuthority, SessionRealizationState,
};
/// The exclusion every realization phase driver must hold is contract-owned
/// (`SessionRealizationPermit`); this crate only supplies the Session slot
/// allocation as its identity.
mod realization_permit;

/// Host fixtures use the actual Control-clock coordinate for live replies.
/// Deliberately rejects expired input: an expiry test must construct and age
/// its request observation explicitly, never silently manufacture fresh TTL.
#[cfg(test)]
pub(crate) fn test_lease_receipt(
    lease: awaken_session_resource_contract::SessionRealizationLease,
) -> awaken_session_realization_contract::SessionRealizationLeaseReceipt {
    awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
        lease,
        crate::terminal_repository_publication::runtime_unix_now_ms(),
    )
    .expect("live Control-clock fixture lease")
}

#[cfg(test)]
pub(crate) fn test_observed_lease(
    lease: awaken_session_resource_contract::SessionRealizationLease,
) -> awaken_session_realization_contract::ObservedSessionRealizationLease {
    let started = awaken_authority_clock::authority_proof_started().unwrap();
    test_lease_receipt(lease).observed(started)
}

/// Deliberately aged evidence for negative Host tests. Keep the exact durable
/// fence bytes, but exhaust the local proof on the real platform clock before
/// returning it. This is never the fallback for a failed live fixture.
#[cfg(test)]
pub(crate) fn test_expired_observed_lease(
    lease: awaken_session_resource_contract::SessionRealizationLease,
) -> awaken_session_realization_contract::ObservedSessionRealizationLease {
    let started = awaken_authority_clock::authority_proof_started().unwrap();
    let proof = awaken_session_realization_contract::SessionRealizationLeaseReceipt {
        lease,
        remaining_ms: std::num::NonZeroU64::new(1).unwrap(),
    }
    .observed(started);
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert!(
        proof
            .remaining_at(awaken_authority_clock::authority_proof_started().unwrap())
            .is_none()
    );
    proof
}

impl SessionRuntimeSlot {
    /// Install a projected Environment owner and, in the same slot lock, let
    /// the canonical projection reopen MCP admission when the run layer holds
    /// no local effect.
    pub(crate) fn install_projected_environment_owner(
        &mut self,
        owner: SessionEnvironmentOwner,
        state: &awaken_session_state_contract::SessionEnvironmentState,
    ) {
        self.realization.environment_owner = owner;
        self.reopen_mcp_realization_admission_from_projection(state);
    }

    /// Select the one Resource manifest carried by a newly authored durable
    /// Run dispatch. A prepared Session carries the aggregate's desired
    /// generation so a cold Worker can realize that exact transition; legacy
    /// Sessions fall back to the physically active manifest. Ordinary Runs use
    /// active truth only and are never promoted by a staged transition.
    ///
    /// This read is intentionally side-effect free. Publishing `manifest`
    /// remains the exclusive completion edge for physical Resource effects.
    pub(crate) fn dispatch_resource_manifest(
        &self,
    ) -> Option<awaken_session_resource_contract::SessionResourceManifest> {
        if self.run.session_dispatch {
            self.realization
                .resource_transition
                .as_ref()
                .map(|transition| transition.desired().clone())
                .or_else(|| self.realization.manifest.clone())
        } else {
            self.realization.manifest.clone()
        }
    }

    /// Consume the process-local fence only while installing one authoritative
    /// durable Environment projection and while every old local effect owner is
    /// already gone. This keeps expiry and restore from reopening MCP admission
    /// during the gap between their durable transition and source cleanup.
    pub(crate) fn reopen_mcp_realization_admission_from_projection(
        &mut self,
        environment: &awaken_session_state_contract::SessionEnvironmentState,
    ) -> bool {
        let Some(fence) = self.realization.mcp.quiescence_fence() else {
            return false;
        };
        let local_effects_cleared =
            self.run.runtime.is_none() && self.realization.mcp.local_effects_cleared();
        if !local_effects_cleared {
            return false;
        }
        let authoritative_reopen = if fence.restored_by(environment) {
            matches!(
                (&self.realization.environment_owner, environment),
                (
                    SessionEnvironmentOwner::Preparing(
                        SessionEnvironmentPreparation::AwaitingAdoption {
                            identity: BoundSessionEnvironmentIdentity::Durable {
                                effect_id: owner_effect,
                                generation: owner_generation,
                            },
                            binding: owner_binding,
                        },
                    ),
                    awaken_session_state_contract::SessionEnvironmentState::Resident {
                        binding,
                        effect_id: Some(effect_id),
                        generation: Some(generation),
                        ..
                    },
                ) if owner_effect == effect_id
                    && owner_generation == generation
                    && owner_binding == binding
            ) || matches!(
                (&self.realization.environment_owner, environment),
                (
                    SessionEnvironmentOwner::Resident(BoundSessionEnvironment {
                        identity: BoundSessionEnvironmentIdentity::Durable {
                            effect_id: owner_effect,
                            generation: owner_generation,
                        },
                        binding: owner_binding,
                        ..
                    }),
                    awaken_session_state_contract::SessionEnvironmentState::Resident {
                        binding,
                        effect_id: Some(effect_id),
                        generation: Some(generation),
                        ..
                    },
                ) if owner_effect == effect_id
                    && owner_generation == generation
                    && owner_binding == binding
            )
        } else {
            matches!(
                environment,
                awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
            ) && matches!(
                &self.realization.environment_owner,
                SessionEnvironmentOwner::Vacant
            )
        };
        if !authoritative_reopen {
            return false;
        }
        self.realization.mcp.reopen_admission();
        true
    }
}

/// One Session's process-local state, in three layers by lifetime. The
/// Session aggregate stays the sole authority; every layer is a rebuildable
/// projection of it or a resource this process runs. Dependencies point one
/// way: `run` reads `realization` and `frozen`, `realization` reads `frozen`.
#[derive(Default)]
pub(crate) struct SessionRuntimeSlot {
    /// The Session's frozen inputs, held whole. Install and every later change
    /// replace this one value; nothing amends a field of it in place.
    pub(crate) frozen: Arc<FrozenSessionInputs>,
    /// What this process realized for the Session: its Environment owner,
    /// realization lease, staged Resources and the locks that serialize them.
    pub(crate) realization: SessionRealizationState,
    /// The Session's running context and the dispatch it currently serves.
    pub(crate) run: SessionRunState,
}

/// The run layer: the Session context built for running and the durable
/// dispatch it serves. Rebuilt from the other two layers when dropped.
#[derive(Default)]
pub(crate) struct SessionRunState {
    pub runtime: Option<Arc<crate::host::SessionCtx>>,
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

impl crate::session_realizations::SessionRunQuery for SessionRunState {
    fn holds_context(&self) -> bool {
        self.runtime.is_some()
    }

    fn session_dispatch(&self) -> bool {
        self.session_dispatch
    }

    fn dispatch_claim(&self) -> Option<&awaken_dispatch_contract::RunClaim> {
        self.dispatch_claim.as_ref()
    }

    fn drop_context(&mut self) {
        self.runtime = None;
    }

    fn enter_session_dispatch(&mut self) {
        self.session_dispatch = true;
    }
}

impl crate::session_realizations::SessionRealizationSource
    for Mutex<HashMap<String, SessionRuntimeSlot>>
{
    fn visit(
        &self,
        session: &str,
        create: bool,
        visit: &mut dyn FnMut(crate::session_realizations::SessionRealizationAccess<'_>),
    ) {
        let mut slots = self.lock().expect("session runtime slots mutex poisoned");
        let slot = if create {
            Some(slots.entry(session.to_string()).or_default())
        } else {
            slots.get_mut(session)
        };
        if let Some(slot) = slot {
            visit(crate::session_realizations::SessionRealizationAccess {
                frozen: &slot.frozen,
                realization: &mut slot.realization,
                run: &mut slot.run,
            });
        }
    }
}

impl SessionRunState {
    /// Drop a metadata-only coordination envelope so the next read rebuilds
    /// it; an executable context is kept.
    pub(crate) fn drop_coordination_envelope(&mut self) {
        if self
            .runtime
            .as_ref()
            .is_some_and(|context| context.is_coordination_envelope())
        {
            self.runtime = None;
        }
    }
}

impl SessionRuntimeSlot {
    /// The only write of the frozen inputs: build the next value from a copy
    /// and publish it whole; readers holding the previous `Arc` keep it. The
    /// runtime context is assembled once with its request prefix, so a changed
    /// prefix drops only that cached context.
    pub(crate) fn replace_frozen_with(&mut self, build: impl FnOnce(&mut FrozenSessionInputs)) {
        let mut next = self.frozen.as_ref().clone();
        build(&mut next);
        if self.frozen.request_context != next.request_context {
            self.run.runtime = None;
        }
        self.frozen = Arc::new(next);
    }
}

#[derive(Clone)]
pub(crate) struct SessionRuntimeSlots {
    slots: Arc<Mutex<HashMap<String, SessionRuntimeSlot>>>,
    /// The realizer's view of these same Sessions; the realization-layer
    /// operations live there and are reached through `Deref`.
    realizations: crate::session_realizations::SessionRealizations,
}

impl Default for SessionRuntimeSlots {
    fn default() -> Self {
        let slots: Arc<Mutex<HashMap<String, SessionRuntimeSlot>>> = Arc::default();
        Self {
            realizations: crate::session_realizations::SessionRealizations::new(slots.clone()),
            slots,
        }
    }
}

impl std::ops::Deref for SessionRuntimeSlots {
    type Target = crate::session_realizations::SessionRealizations;

    fn deref(&self) -> &Self::Target {
        &self.realizations
    }
}

/// Ephemeral cleanup selection, captured at the close edge. Existing lock Arcs
/// identify the slot allocation; equal Session ids and leases cannot defeat
/// this identity after removal/recreation. No second generation counter exists.
pub(crate) struct SessionRealizationRevocationSelection {
    pub realization: Arc<tokio::sync::Mutex<()>>,
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    authority: SessionRealizationAuthority,
}

impl SessionRealizationRevocationSelection {
    fn close(
        slot: &mut SessionRuntimeSlot,
        expected: Option<&awaken_session_resource_contract::SessionRealizationLease>,
    ) -> Option<Self> {
        slot.realization.close_authority(expected).then(|| Self {
            realization: slot.realization.realization.clone(),
            lifecycle: slot.realization.lifecycle.clone(),
            authority: slot.realization.authority.clone(),
        })
    }

    pub(crate) fn matches(&self, slot: &SessionRuntimeSlot) -> bool {
        Arc::ptr_eq(&self.realization, &slot.realization.realization)
            && Arc::ptr_eq(&self.lifecycle, &slot.realization.lifecycle)
            && self.authority == slot.realization.authority
    }
}

impl SessionRuntimeSlots {
    /// The realizer's view of these Sessions.
    pub(crate) fn realizations(&self) -> crate::session_realizations::SessionRealizations {
        self.realizations.clone()
    }

    pub(crate) fn close_realization(
        &self,
        session: &str,
        expected: Option<(
            &awaken_session_resource_contract::SessionRealizationLease,
            &Arc<tokio::sync::Mutex<()>>,
        )>,
    ) -> Option<SessionRealizationRevocationSelection> {
        self.modify(session, |slot| {
            if expected.is_some_and(|(_, realization)| {
                !Arc::ptr_eq(&slot.realization.realization, realization)
            }) {
                return None;
            }
            SessionRealizationRevocationSelection::close(slot, expected.map(|(lease, _)| lease))
        })
        .flatten()
    }

    /// One linearization point closes every currently installed projection
    /// before any caller may wait for an async lifecycle lock or drain.
    pub(crate) fn close_all_realizations(
        &self,
    ) -> Vec<(String, SessionRealizationRevocationSelection)> {
        self.slots
            .lock()
            .expect("session runtime slots mutex poisoned")
            .iter_mut()
            .map(|(id, slot)| {
                (
                    id.clone(),
                    SessionRealizationRevocationSelection::close(slot, None)
                        .expect("unconditional close always selects the current slot"),
                )
            })
            .collect()
    }

    /// Publish a replacement of one Session's frozen inputs, creating its slot.
    pub(crate) fn replace_frozen_with(
        &self,
        session: &str,
        build: impl FnOnce(&mut FrozenSessionInputs),
    ) {
        self.update(session, |slot| slot.replace_frozen_with(build));
    }

    pub fn read<R>(&self, session: &str, f: impl FnOnce(&SessionRuntimeSlot) -> R) -> Option<R> {
        self.slots
            .lock()
            .expect("session runtime slots mutex poisoned")
            .get(session)
            .map(f)
    }

    pub fn update<R>(&self, session: &str, f: impl FnOnce(&mut SessionRuntimeSlot) -> R) -> R {
        let mut slots = self
            .slots
            .lock()
            .expect("session runtime slots mutex poisoned");
        f(slots.entry(session.to_string()).or_default())
    }

    pub fn modify<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRuntimeSlot) -> R,
    ) -> Option<R> {
        self.slots
            .lock()
            .expect("session runtime slots mutex poisoned")
            .get_mut(session)
            .map(f)
    }

    /// Read only the run layer of one Session's slot.
    pub(crate) fn read_run<R>(
        &self,
        session: &str,
        f: impl FnOnce(&SessionRunState) -> R,
    ) -> Option<R> {
        self.read(session, |slot| f(&slot.run))
    }

    /// Change only the run layer, creating the Session's slot.
    pub(crate) fn update_run<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRunState) -> R,
    ) -> R {
        self.update(session, |slot| f(&mut slot.run))
    }

    /// Change only the run layer of an existing slot.
    pub(crate) fn modify_run<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRunState) -> R,
    ) -> Option<R> {
        self.modify(session, |slot| f(&mut slot.run))
    }

    pub fn remove(&self, session: &str) -> Option<SessionRuntimeSlot> {
        let removed = self
            .slots
            .lock()
            .expect("session runtime slots mutex poisoned")
            .remove(session);
        if let Some(slot) = &removed {
            if let Some(preparation) = &slot.realization.resource_preparation {
                preparation.cancel();
            }
            slot.realization.changed.notify_waiters();
        }
        removed
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
