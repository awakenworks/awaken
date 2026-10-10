// @formal entrypoint session_realization.concurrent_drivers_preserve_one_owner_fence
// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
//! Process-local realization state for one Session.
//!
//! The frozen manifest remains the durable authority. This private slot owns every
//! projection materialized from it so registration and terminal cleanup share one
//! lifecycle boundary instead of coordinating parallel maps.

use crate::frozen_inputs::{FrozenSessionInputs, ManagedContentDelivery};
use crate::staged_resources::EnvironmentResourceReconciliation;
pub(crate) use awaken_session_mcp::McpQuiescenceAdmissionFence;
use awaken_session_mcp::SessionMcpState;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::memory::BoundMemory;
use crate::staged_resources::StagedResources;

pub(crate) use crate::environment_owner::*;
mod execution_authority;
mod projection_receipt;
pub(crate) use projection_receipt::FrozenProjectionReceipt;
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

/// One process-local projection of aggregate authority, not another lease.
/// Closing retains the exact lease for cleanup CAS, but never for admission.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum SessionRealizationAuthority {
    #[default]
    Uninstalled,
    Live(awaken_session_realization_contract::ObservedSessionRealizationLease),
    Closed(Option<awaken_session_resource_contract::SessionRealizationLease>),
}

impl SessionRealizationAuthority {
    pub(crate) fn proof(
        &self,
    ) -> Option<&awaken_session_realization_contract::ObservedSessionRealizationLease> {
        match self {
            Self::Live(proof) => Some(proof),
            Self::Uninstalled | Self::Closed(_) => None,
        }
    }

    pub(crate) fn remaining(&self) -> Option<std::time::Duration> {
        self.proof()?
            .remaining_at(awaken_authority_clock::authority_proof_started().ok()?)
    }

    /// One local admission rule: exact aggregate generation and a still-live
    /// request-relative observation. Callers hold the existing slot mutex;
    /// neither wall time nor a retained Closed lease grants authority.
    pub(crate) fn remaining_for(
        &self,
        asserted: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Option<std::time::Duration> {
        awaken_session_realization_contract::realization_lease_generation_authorizes(
            self.lease()?,
            asserted,
        )
        .then(|| self.remaining())
        .flatten()
    }

    pub(crate) fn lease(
        &self,
    ) -> Option<&awaken_session_resource_contract::SessionRealizationLease> {
        match self {
            Self::Live(proof) => Some(&proof.lease),
            Self::Uninstalled | Self::Closed(_) => None,
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }

    /// Called under the Session slot mutex used for renewal application. The
    /// pending provider GO callback must hold this same mutex through its grant.
    /// A retry may select the retained closed identity; a stale revoker cannot.
    pub(crate) fn close(
        &mut self,
        expected: Option<&awaken_session_resource_contract::SessionRealizationLease>,
    ) -> bool {
        let retained = match self {
            Self::Live(proof) => Some(&mut proof.lease),
            Self::Closed(lease) => lease.as_mut(),
            Self::Uninstalled => None,
        };
        if expected.is_some_and(|expected| retained.as_deref() != Some(expected)) {
            return false;
        }
        *self = Self::Closed(retained.cloned());
        true
    }
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

/// The environment-realization layer: who owns the Session's Environment,
/// under which realization lease, and which Resources, memory, Skills and MCP
/// generations are staged for it. Built while realizing; revocation keeps
/// only what a retry of a retained Environment owner still needs.
#[derive(Default)]
pub(crate) struct SessionRealizationState {
    /// Serializes the canonical realization phase driver for this Session.
    /// Lease renewal may advance authority while this lock is held, but it must
    /// not start a second Stage/Publish driver. This is deliberately separate
    /// from `lifecycle`, which is re-entered by Environment materialization.
    pub realization: Arc<tokio::sync::Mutex<()>>,
    /// Serializes first materialization/rebuild for this Session without a
    /// process-wide registry lock being held across I/O.
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    /// Serializes one complete Resource-generation compare/realize/publish
    /// transition. The global lock order is `realization -> lifecycle ->
    /// resource_projection`; callers may acquire any suffix but never reverse
    /// it. Keeping this separate lets projection installation remain atomic to
    /// Runtime observers without re-entering `lifecycle` during Resource sync.
    pub resource_projection: Arc<tokio::sync::Mutex<()>>,
    /// Last accepted aggregate version and equality digest; rebuildable, never authority.
    pub(crate) frozen_projection_receipt: Option<FrozenProjectionReceipt>,
    /// Sole live Environment owner. Every state transition is driven through
    /// `host/session/environment_lifecycle.rs`; other modules may only inspect
    /// Resident/occupancy projections through its read helpers.
    pub(crate) environment_owner: SessionEnvironmentOwner,
    /// Derived once while the Session context is built. Prompt projection,
    /// mount realization, Native tools, and ACP export all consume this value;
    /// none may independently choose another delivery path.
    pub content_delivery: Option<ManagedContentDelivery>,
    /// Exact aggregate Environment phase installed only by a terminal cleanup
    /// assignment. The cleanup command uses this frozen fact to distinguish a
    /// genuinely unmaterialized/hibernated Session from Resident, Suspending,
    /// or Restoring state; absence is never interpreted as permission to reap
    /// or acknowledge a root cleanup.
    pub terminal_environment_state: Option<awaken_session_state_contract::SessionEnvironmentState>,
    /// Process-local parent of a terminal cleanup target. This is routing
    /// metadata only: the aggregate's frozen command set remains the sole work
    /// and receipt authority. It lets an accepted root receipt (or a later
    /// aggregate Completed read after response loss) retire child projections
    /// without maintaining a second cleanup registry.
    pub terminal_cleanup_root: Option<String>,
    /// Exact unavailable aggregate binding/generation authorized for one
    /// RebuildFromCommittedTruth replacement. This is a process-local command
    /// projection; the Session aggregate receipt remains the only transition
    /// authority and clears it after durable publication.
    pub environment_rebuild_source: Option<(String, Option<String>)>,
    /// Every standard MemoryStore resource frozen for this Session, keyed by
    /// binding id. These exist independently of the optional Awaken automatic
    /// memory extension selected into `memory` below.
    pub memory_bindings: HashMap<String, Arc<BoundMemory>>,
    /// The one explicitly selected automatic recall/extraction binding. This is
    /// never the authority for which standard resources are mounted.
    pub memory: Option<Arc<BoundMemory>>,
    /// The Session's MCP generation owners, quiescence fence, and frozen
    /// attachment marker. Other Host modules use only its methods.
    pub mcp: SessionMcpState,
    /// Current Control-issued projection authority. It is a live cache used to
    /// request renewal; durable ownership remains in the Session aggregate.
    pub authority: SessionRealizationAuthority,
    /// Cancellation of the current Resource preparation only. This carries no
    /// ownership: `authority` and Control remain the sole authorities.
    pub resource_preparation: Option<Arc<awaken_runtime_contract::CancellationToken>>,
    /// Wakes a claim resolver waiting for the Session application's initial
    /// restart reassignment. The lease above remains the only projected fact;
    /// this notification carries no authority or parallel state.
    pub changed: Arc<tokio::sync::Notify>,
    /// `Some([])` means the frozen manifest delivers no Skills; `None` means this
    /// embedded Session has no frozen Skill manifest.
    pub skills: Option<Vec<awaken_resource_contract::SkillVersion>>,
    /// Managed-filesystem Skill discovery metadata. Full instructions remain
    /// in the exact frozen bundle (and its later filesystem projection) and
    /// are read on demand by the ordinary `read` tool.
    pub skill_prompt: Option<crate::skill_prompt::FilesystemSkillPrompt>,
    pub resources: StagedResources,
    pub manifest: Option<awaken_session_resource_contract::SessionResourceManifest>,
    /// Exact aggregate-owned physical replacement currently being replayed.
    /// This is a rebuildable command projection, not completion state: the
    /// durable Resource aggregate remains the only active/pending authority.
    pub resource_transition: Option<awaken_session_resource_contract::SessionResourceTransition>,
    /// Fingerprint of the exact transition + optional Run claim whose immutable
    /// inputs were compiled into `resources`, `memory_bindings`, and `skills`.
    /// This avoids duplicate File/Vault/catalog reads between deferred staging
    /// and the same process's later physical Environment convergence.
    pub staged_resource_effect_key: Option<String>,
    /// How the resident Environment entered this process. Fresh substrates and
    /// adopted providers require different idempotent mount completion, but the
    /// aggregate transition remains their single desired-state authority.
    pub environment_resource_reconciliation: EnvironmentResourceReconciliation,
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

impl SessionRealizationState {
    /// Resources the Environment held before an in-flight resource transition.
    pub(crate) fn previous_resources(
        &self,
    ) -> Option<awaken_session_resource_contract::ResolvedSessionResources> {
        let transition = self.resource_transition.as_ref()?;
        Some(transition.previous().resources.clone())
    }

    /// What revocation keeps while an Environment owner is still retained:
    /// the slot's lock identities, the closed authority, the owner itself and
    /// the content it was staged with. Everything else is rebuilt.
    pub(crate) fn take_retained_on_revocation(&mut self) -> Self {
        Self {
            realization: self.realization.clone(),
            lifecycle: self.lifecycle.clone(),
            resource_projection: self.resource_projection.clone(),
            changed: self.changed.clone(),
            authority: self.authority.clone(),
            environment_owner: std::mem::take(&mut self.environment_owner),
            content_delivery: self.content_delivery,
            resources: std::mem::take(&mut self.resources),
            // Revocation clears process-local realization material but is
            // not an Environment restoration/source-disposal authority.
            // Preserve an already-closed quiescence fence so only the
            // canonical durable projection proof may reopen MCP effects.
            mcp: self.mcp.take_retained_on_revocation(),
            ..Self::default()
        }
    }

    pub(crate) fn close_authority(
        &mut self,
        expected: Option<&awaken_session_resource_contract::SessionRealizationLease>,
    ) -> bool {
        if !self.authority.close(expected) {
            return false;
        }
        if let Some(preparation) = &self.resource_preparation {
            preparation.cancel();
        }
        self.changed.notify_waiters();
        true
    }

    pub(crate) fn install_lease(
        &mut self,
        proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> bool {
        let Ok(now) = awaken_authority_clock::authority_proof_started() else {
            return false;
        };
        if proof.remaining_at(now).is_none() {
            return false;
        }
        let lease = &proof.lease;
        if let SessionRealizationAuthority::Closed(Some(closed)) = &self.authority
            && (awaken_session_realization_contract::realization_lease_generation_authorizes(
                closed, lease,
            ) || awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, closed,
            ))
        {
            // An assertion or delayed renewal cannot reopen a closed generation.
            return false;
        }
        if let Some(current) = self.authority.lease()
            && (awaken_session_realization_contract::realization_lease_generation_authorizes(
                current, lease,
            ) || awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, current,
            ))
            && self.authority.remaining().is_none()
        {
            return false;
        }
        if self.authority.lease().is_some_and(|current| {
            awaken_session_realization_contract::realization_lease_generation_authorizes(
                current, lease,
            )
        }) {
            // A slow preparation must not overwrite an intervening renewal.
            return true;
        }
        if self.authority.lease().is_some_and(|current| {
            !awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, current,
            )
        }) && let Some(preparation) = &self.resource_preparation
        {
            preparation.cancel();
        }
        self.authority = SessionRealizationAuthority::Live(proof);
        true
    }

    /// The only renewal response application edge. Callers with an I/O
    /// snapshot must supply it unchanged; a fresh same-lock caller may use
    /// the currently visible lease. Neither can recreate absent/closed state.
    pub(crate) fn renew_lease(
        &mut self,
        asserted: &awaken_session_resource_contract::SessionRealizationLease,
        renewed: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> bool {
        if self.authority.lease() != Some(asserted)
            || !awaken_session_realization_contract::realization_lease_generation_authorizes(
                &renewed.lease,
                asserted,
            )
        {
            return false;
        }
        let Some(now) = awaken_authority_clock::authority_proof_started().ok() else {
            return false;
        };
        if self.authority.remaining().is_none() || renewed.remaining_at(now).is_none() {
            return false;
        }
        self.authority = SessionRealizationAuthority::Live(renewed);
        self.changed.notify_waiters();
        true
    }

    pub(crate) fn resource_preparation_authorized(
        &self,
        asserted: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> bool {
        !self.authority.is_closed()
            && self
                .resource_preparation
                .as_ref()
                .is_none_or(|preparation| {
                    !preparation.is_cancelled() && self.authority.remaining_for(asserted).is_some()
                })
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

#[derive(Clone, Default)]
pub(crate) struct SessionRuntimeSlots(Arc<Mutex<HashMap<String, SessionRuntimeSlot>>>);

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
        self.0
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

    pub(crate) fn close_mcp_realization_admission(
        &self,
        session: &str,
        fence: McpQuiescenceAdmissionFence,
    ) -> Result<bool, crate::HostError> {
        self.update(session, |slot| slot.realization.mcp.close_admission(fence))
            .map_err(crate::HostError::from)
    }

    #[cfg(test)]
    pub(crate) fn mcp_realization_admitted(&self, session: &str) -> bool {
        self.read(session, |slot| slot.realization.mcp.admits_realization())
            .unwrap_or(true)
    }

    /// Read the one current process-local projection of all frozen prompt
    /// inputs. The durable baseline/resource aggregates remain authoritative;
    /// executors call this at attempt time so late realization cannot leave a
    /// construction-time prompt snapshot behind.
    pub fn prompts(
        &self,
        session: &str,
        working_directory: Option<&str>,
        has_acp: bool,
        has_native: bool,
    ) -> Result<Vec<String>, String> {
        self.read(session, |slot| {
            let mut prompts = slot.realization.resources.prompts.clone();
            let delivery = slot.realization.content_delivery.unwrap_or_default();
            let environment = slot.realization.environment_owner.resident();
            let path_view = match environment.as_deref() {
                Some(environment) => environment
                    .agent_tools()
                    .agent_prompt_path_view(working_directory, has_acp, has_native)
                    .map_err(str::to_owned)?,
                None => Default::default(),
            };
            prompts.extend(
                slot.realization.resources
                    .resource_prompts
                    .iter()
                    .map(|prompt| prompt.render(delivery, path_view.clone())),
            );
            if let Some(baseline) = &slot.frozen.baseline {
                prompts.extend(baseline.prompts.iter()
                    .filter(|prompt| !awaken_session_configuration_contract::SessionContextSeed::is_seed_prompt(prompt))
                    .cloned());
            }
            if let Some(skill_prompt) = &slot.realization.skill_prompt {
                prompts.push(skill_prompt.render(&path_view, environment.as_deref())?);
            }
            Ok(prompts)
        })
        .unwrap_or_else(|| Ok(Vec::new()))
    }

    /// Hold one Session's Environment `lifecycle` lock, creating its slot.
    pub(crate) async fn lock_lifecycle(&self, session: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lifecycle = self.update(session, |slot| slot.realization.lifecycle.clone());
        lifecycle.lock_owned().await
    }

    /// Hold one Session's `resource_projection` lock, creating its slot.
    pub(crate) async fn lock_resource_projection(
        &self,
        session: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let projection = self.update(session, |slot| slot.realization.resource_projection.clone());
        projection.lock_owned().await
    }

    #[cfg(test)]
    pub fn realization_lock(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.update(session, |slot| slot.realization.realization.clone())
    }

    /// The immutable Agent publication retained for one Session.
    pub(crate) fn published_snapshot(
        &self,
        session: &str,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        self.frozen(session).published_snapshot.clone()
    }

    /// Publish a replacement of one Session's frozen inputs, creating its slot.
    pub(crate) fn replace_frozen_with(
        &self,
        session: &str,
        build: impl FnOnce(&mut FrozenSessionInputs),
    ) {
        self.update(session, |slot| slot.replace_frozen_with(build));
    }

    /// The Session's current frozen inputs, taken out of the lock as one
    /// value. An unknown Session reads as the empty projection.
    pub(crate) fn frozen(&self, session: &str) -> Arc<FrozenSessionInputs> {
        self.read(session, |slot| slot.frozen.clone())
            .unwrap_or_default()
    }

    pub fn read<R>(&self, session: &str, f: impl FnOnce(&SessionRuntimeSlot) -> R) -> Option<R> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .get(session)
            .map(f)
    }

    pub fn update<R>(&self, session: &str, f: impl FnOnce(&mut SessionRuntimeSlot) -> R) -> R {
        let mut slots = self.0.lock().expect("session runtime slots mutex poisoned");
        f(slots.entry(session.to_string()).or_default())
    }

    pub fn modify<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRuntimeSlot) -> R,
    ) -> Option<R> {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .get_mut(session)
            .map(f)
    }

    /// Read only the realization layer of one Session's slot.
    pub(crate) fn read_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&SessionRealizationState) -> R,
    ) -> Option<R> {
        self.read(session, |slot| f(&slot.realization))
    }

    /// Change only the realization layer, creating the Session's slot.
    pub(crate) fn update_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRealizationState) -> R,
    ) -> R {
        self.update(session, |slot| f(&mut slot.realization))
    }

    /// Change only the realization layer of an existing slot.
    pub(crate) fn modify_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRealizationState) -> R,
    ) -> Option<R> {
        self.modify(session, |slot| f(&mut slot.realization))
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
            .0
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
        self.0
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
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .keys()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub fn contains(&self, session: &str) -> bool {
        self.0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .contains_key(session)
    }
}

#[cfg(test)]
mod tests;
