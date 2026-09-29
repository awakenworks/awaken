// @formal entrypoint session_realization.concurrent_drivers_preserve_one_owner_fence
// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.cross_session_reconciliation_is_bounded_and_each_control_deadline_releases_capacity_without_weakening_per_session_mutex
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
//! Process-local realization state for one Session.
//!
//! The frozen manifest remains the durable authority. This private slot owns every
//! projection materialized from it so registration and terminal cleanup share one
//! lifecycle boundary instead of coordinating parallel maps.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::memory::BoundMemory;
use crate::provisioning::StagedResources;

mod environment_owner;
pub(crate) use environment_owner::*;
mod execution_authority;
mod projection_receipt;
pub(crate) use projection_receipt::FrozenProjectionReceipt;
mod realization_permit;
/// The exclusion every realization phase driver must hold is contract-owned;
/// this crate only supplies the Session slot allocation as its identity.
pub(crate) use awaken_session_realization_contract::SessionRealizationPermit;

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

/// One Session-wide delivery decision for resources that can be represented
/// either as files or as semantic tools. The frozen Resource/Skill identities
/// remain authoritative; this value selects only their runtime projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ManagedContentDelivery {
    /// Anthropic Managed Agents compatible filesystem projection. Memory stores
    /// are mounted and Skills are discovered from their `SKILL.md` paths.
    #[default]
    ManagedFilesystem,
    /// Filesystem-free projection for Native agents whose published tool policy
    /// cannot invoke a filesystem tool. The same frozen bindings are exposed by
    /// bounded semantic tools instead.
    SemanticTools,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpProjectionState {
    /// The exact generation owner is installed before process spawn/connect.
    /// Cancellation may leave this state temporarily, but never an unowned
    /// process; the owned staging task completes or drains it canonically.
    Staging,
    Staged,
    Active,
    Draining,
    Removed,
}

#[derive(Clone, Default)]
pub(crate) struct McpStagingActivity {
    finished: Arc<AtomicBool>,
    changed: Arc<tokio::sync::Notify>,
}

impl McpStagingActivity {
    pub(crate) fn guard(&self) -> McpStagingActivityGuard {
        McpStagingActivityGuard(self.clone())
    }

    pub(crate) fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub(crate) async fn wait(&self) {
        while !self.finished.load(Ordering::Acquire) {
            let changed = self.changed.notified();
            if self.finished.load(Ordering::Acquire) {
                break;
            }
            changed.await;
        }
    }
}

pub(crate) struct McpStagingActivityGuard(McpStagingActivity);

impl Drop for McpStagingActivityGuard {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// Process-local effect for one exact durable MCP generation. This is live
/// material/connection state only; desired state remains in the Session
/// aggregate and is never reconstructed from this slot.
#[derive(Clone)]
pub(crate) struct McpGenerationProjection {
    /// The one canonical, secret-free effect input. Keep it intact so durable
    /// dispatch can replay the exact generation on the execution Worker without
    /// reconstructing a parallel MCP configuration from live transport state.
    pub request: awaken_session_mcp_contract::StageMcpAttachment,
    /// Exact immutable realization identity with only renewable expiry/key
    /// excluded. It prevents a lease extension from changing target,
    /// credential, holder, owner incarnation, epoch, or logical generation.
    pub receipt: awaken_session_mcp_contract::McpRealizationReceipt,
    pub server: Option<crate::mcp::McpTransportMaterial>,
    pub native_wiring: Option<crate::mcp::McpWiring>,
    /// Session-owned process for a Native sandbox-stdio MCP generation. ACP
    /// starts its stdio child itself and HTTP generations have no sandbox
    /// process, so both leave this empty.
    pub mcp_process: Option<Arc<dyn awaken_provisioning_contract::ProcessHandle>>,
    /// Owned spawn/connect activity. Drain waits for it before proving that a
    /// process cannot appear after cleanup has completed.
    pub staging: Option<McpStagingActivity>,
    /// One cancellation-safe cleanup owner for this exact generation.
    pub drain: Arc<tokio::sync::Mutex<()>>,
    pub state: McpProjectionState,
}

/// Process-local admission fence for one exact durable suspend operation. It
/// lives beside the MCP effect owners in the canonical Session slot; it is not
/// durable lifecycle truth and cannot authorize checkpoint or restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpQuiescenceAdmissionFence {
    pub operation_effect_id: String,
    pub activity_epoch: u64,
    pub source_effect_id: String,
    pub source_binding: String,
    pub generation: awaken_session_state_contract::SandboxGeneration,
}

impl McpQuiescenceAdmissionFence {
    pub(crate) fn new(
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
    ) -> Self {
        Self {
            operation_effect_id: operation.effect_id.clone(),
            activity_epoch: operation.activity_epoch,
            source_effect_id: source_effect_id.to_owned(),
            source_binding: source_binding.to_owned(),
            generation: generation.clone(),
        }
    }

    fn restored_by(
        &self,
        environment: &awaken_session_state_contract::SessionEnvironmentState,
    ) -> bool {
        match environment {
            awaken_session_state_contract::SessionEnvironmentState::Resident {
                binding,
                effect_id: Some(effect_id),
                generation: Some(generation),
                ..
            } => {
                generation == &self.generation
                    && binding != &self.source_binding
                    && effect_id != &self.source_effect_id
                    && effect_id != &self.operation_effect_id
            }
            _ => false,
        }
    }
}

impl SessionRuntimeSlot {
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
        if self.session_dispatch {
            self.resource_transition
                .as_ref()
                .map(|transition| transition.desired().clone())
                .or_else(|| self.manifest.clone())
        } else {
            self.manifest.clone()
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
        let Some(fence) = &self.mcp_quiescence_fence else {
            return false;
        };
        let local_effects_cleared = self.runtime.is_none()
            && self.mcp.iter().all(|projection| {
                projection.state == McpProjectionState::Removed
                    && projection.server.is_none()
                    && projection.native_wiring.is_none()
                    && projection.mcp_process.is_none()
                    && projection.staging.is_none()
            });
        if !local_effects_cleared {
            return false;
        }
        let authoritative_reopen = if fence.restored_by(environment) {
            matches!(
                (&self.environment_owner, environment),
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
                (&self.environment_owner, environment),
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
            ) && matches!(&self.environment_owner, SessionEnvironmentOwner::Vacant)
        };
        if !authoritative_reopen {
            return false;
        }
        self.mcp_quiescence_fence = None;
        true
    }
}

/// Process-local reference to the exact Control-frozen baseline. Keeping the
/// domain value intact avoids a second partial publication/layout projection:
/// context, provider selection, cold replay, and cleanup all consume the same
/// immutable coordinates.
pub(crate) type FrozenBaselineRuntimeProjection =
    awaken_session_configuration_contract::SessionBaseline;

/// The sole process-local projection of the frozen Environment snapshot.
/// Native and ACP provisioning both consume this value; no late network or
/// Sandbox override is stored beside it.
#[derive(Clone)]
pub(crate) struct FrozenEnvironmentRuntimeProjection {
    pub fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint,
    pub network: awaken_provisioning_contract::NetworkPolicy,
    pub packages: awaken_provisioning_contract::PackageRequirements,
    pub sandbox: Option<awaken_provisioning_contract::SandboxOverride>,
    pub provisioning: awaken_session_configuration_contract::SandboxProvisioning,
    pub idle_retention: awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy,
}

#[derive(Default)]
pub(crate) struct SessionRuntimeSlot {
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
    pub runtime: Option<Arc<crate::host::SessionCtx>>,
    /// Rebuildable model-only context materialized from the Session baseline's
    /// immutable transcript-prefix reference. Never committed to this Thread.
    pub request_context: Vec<awaken_agent_contract::agent::message::Message>,
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
    pub deferred_executor: Option<Arc<dyn awaken_runtime_contract::tool::ToolExecutor>>,
    /// Current durable dispatch claim used by claim-fenced Resource effects.
    /// This process-local projection is replaced at every claimed resolve; the
    /// dispatch store remains the lease/epoch authority.
    pub dispatch_claim: Option<awaken_dispatch::RunClaim>,
    pub workspace: Option<String>,
    /// Exact Agent identity copied from the frozen Session baseline. Internal
    /// history/recovery calls do not carry a wire Agent parameter, so they must
    /// resolve the publication through this projection instead of defaulting to
    /// the built-in assistant.
    pub agent_id: Option<String>,
    /// Exact immutable Agent publication supplied by the claimed activation.
    /// Dynamic application publications may not be present in the host catalog,
    /// so lease-only realization must retain this authority instead of trying
    /// to reconstruct it from the backend projection.
    pub published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
    pub model_ref: Option<String>,
    /// Process-local copy of the backend frozen in the Session baseline. It is
    /// validated against the immutable publication before runtime construction.
    pub backend_ref: Option<String>,
    /// Exact Environment projection shared by Native and ACP realization.
    pub environment_projection: Option<FrozenEnvironmentRuntimeProjection>,
    /// Original secret-free Control snapshot retained solely for durable
    /// dispatch to another Worker. Runtime provisioning consumes the decoded
    /// projection above; this value is never a second configuration authority.
    pub environment_snapshot: Option<awaken_session_configuration_contract::EnvironmentSnapshot>,
    /// Every standard MemoryStore resource frozen for this Session, keyed by
    /// binding id. These exist independently of the optional Awaken automatic
    /// memory extension selected into `memory` below.
    pub memory_bindings: HashMap<String, Arc<BoundMemory>>,
    /// The one explicitly selected automatic recall/extraction binding. This is
    /// never the authority for which standard resources are mounted.
    pub memory: Option<Arc<BoundMemory>>,
    pub mcp: Vec<McpGenerationProjection>,
    /// Closed before quiescence snapshots MCP owners. Only the atomic durable
    /// projection install can reopen after proving a new exact Resident owner,
    /// or source-free Unmaterialized plus complete local cleanup.
    pub mcp_quiescence_fence: Option<McpQuiescenceAdmissionFence>,
    /// Current Control-issued projection authority. It is a live cache used to
    /// request renewal; durable ownership remains in the Session aggregate.
    pub realization_authority: SessionRealizationAuthority,
    /// Cancellation of the current Resource preparation only. This carries no
    /// ownership: `realization_authority` and Control remain the sole authorities.
    pub resource_preparation: Option<Arc<awaken_runtime_contract::CancellationToken>>,
    /// Wakes a claim resolver waiting for the Session application's initial
    /// restart reassignment. The lease above remains the only projected fact;
    /// this notification carries no authority or parallel state.
    pub realization_changed: Arc<tokio::sync::Notify>,
    /// Whether the last Control-frozen projection contains a nonterminal MCP
    /// attachment. This supports lease supervision when effects live behind an
    /// injected realizer and therefore are not stored in `mcp` locally.
    pub has_mcp_projection: bool,
    /// The thread entered through the Session application API and its durable
    /// dispatch must therefore cross the claimed Session realization boundary.
    /// It carries no desired-state data of its own.
    pub session_dispatch: bool,
    /// Exact Control-frozen baseline projected for realization. It is never
    /// authored or mutated locally.
    pub baseline: Option<FrozenBaselineRuntimeProjection>,
    /// Exact published delegation targets projected by a managed Session.
    pub delegates: Vec<String>,
    /// Complete Session-local tool replacement; `None` inherits publication.
    pub tools: Option<awaken_session_configuration_contract::SessionToolConfiguration>,
    /// `Some([])` means the frozen manifest delivers no Skills; `None` means this
    /// embedded Session has no frozen Skill manifest.
    pub skills: Option<Vec<awaken_resource_contract::SkillVersion>>,
    /// Managed-filesystem Skill discovery metadata. Full instructions remain
    /// in the exact frozen bundle (and its later filesystem projection) and
    /// are read on demand by the ordinary `read` tool.
    pub skill_prompt: Option<crate::skills::FilesystemSkillPrompt>,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum EnvironmentResourceReconciliation {
    #[default]
    None,
    Fresh,
    Adopted,
}

impl SessionRuntimeSlot {
    pub(crate) fn close_realization_authority(
        &mut self,
        expected: Option<&awaken_session_resource_contract::SessionRealizationLease>,
    ) -> bool {
        if !self.realization_authority.close(expected) {
            return false;
        }
        if let Some(preparation) = &self.resource_preparation {
            preparation.cancel();
        }
        self.realization_changed.notify_waiters();
        true
    }

    pub(crate) fn install_realization_lease(
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
        if let SessionRealizationAuthority::Closed(Some(closed)) = &self.realization_authority
            && (awaken_session_realization_contract::realization_lease_generation_authorizes(
                closed, lease,
            ) || awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, closed,
            ))
        {
            // An assertion or delayed renewal cannot reopen a closed generation.
            return false;
        }
        if let Some(current) = self.realization_authority.lease()
            && (awaken_session_realization_contract::realization_lease_generation_authorizes(
                current, lease,
            ) || awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, current,
            ))
            && self.realization_authority.remaining().is_none()
        {
            return false;
        }
        if self.realization_authority.lease().is_some_and(|current| {
            awaken_session_realization_contract::realization_lease_generation_authorizes(
                current, lease,
            )
        }) {
            // A slow preparation must not overwrite an intervening renewal.
            return true;
        }
        if self.realization_authority.lease().is_some_and(|current| {
            !awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, current,
            )
        }) && let Some(preparation) = &self.resource_preparation
        {
            preparation.cancel();
        }
        self.realization_authority = SessionRealizationAuthority::Live(proof);
        true
    }

    /// The only renewal response application edge. Callers with an I/O
    /// snapshot must supply it unchanged; a fresh same-lock caller may use
    /// the currently visible lease. Neither can recreate absent/closed state.
    pub(crate) fn renew_realization_lease(
        &mut self,
        asserted: &awaken_session_resource_contract::SessionRealizationLease,
        renewed: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> bool {
        if self.realization_authority.lease() != Some(asserted)
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
        if self.realization_authority.remaining().is_none() || renewed.remaining_at(now).is_none() {
            return false;
        }
        self.realization_authority = SessionRealizationAuthority::Live(renewed);
        self.realization_changed.notify_waiters();
        true
    }

    pub(crate) fn resource_preparation_authorized(
        &self,
        asserted: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> bool {
        !self.realization_authority.is_closed()
            && self
                .resource_preparation
                .as_ref()
                .is_none_or(|preparation| {
                    !preparation.is_cancelled()
                        && self.realization_authority.remaining_for(asserted).is_some()
                })
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
        slot.close_realization_authority(expected).then(|| Self {
            realization: slot.realization.clone(),
            lifecycle: slot.lifecycle.clone(),
            authority: slot.realization_authority.clone(),
        })
    }

    pub(crate) fn matches(&self, slot: &SessionRuntimeSlot) -> bool {
        Arc::ptr_eq(&self.realization, &slot.realization)
            && Arc::ptr_eq(&self.lifecycle, &slot.lifecycle)
            && self.authority == slot.realization_authority
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
            if expected.is_some_and(|(_, realization)| !Arc::ptr_eq(&slot.realization, realization))
            {
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
        self.update(session, |slot| match &slot.mcp_quiescence_fence {
            Some(current) if current == &fence => Ok(false),
            Some(_) => Err(crate::HostError::internal(
                "another Session Environment quiescence fence is already installed",
            )),
            None => {
                slot.mcp_quiescence_fence = Some(fence);
                Ok(true)
            }
        })
    }

    pub(crate) fn mcp_realization_admitted(&self, session: &str) -> bool {
        self.read(session, |slot| slot.mcp_quiescence_fence.is_none())
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
            let mut prompts = slot.resources.prompts.clone();
            let delivery = slot.content_delivery.unwrap_or_default();
            let environment = slot.environment_owner.resident();
            let path_view = match environment.as_deref() {
                Some(environment) => environment
                    .agent_tools()
                    .agent_prompt_path_view(working_directory, has_acp, has_native)
                    .map_err(str::to_owned)?,
                None => Default::default(),
            };
            prompts.extend(
                slot.resources
                    .resource_prompts
                    .iter()
                    .map(|prompt| prompt.render(delivery, path_view.clone())),
            );
            if let Some(baseline) = &slot.baseline {
                prompts.extend(baseline.prompts.clone());
            }
            if let Some(skill_prompt) = &slot.skill_prompt {
                prompts.push(skill_prompt.render(&path_view, environment.as_deref())?);
            }
            Ok(prompts)
        })
        .unwrap_or_else(|| Ok(Vec::new()))
    }

    #[cfg(test)]
    pub fn realization_lock(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.update(session, |slot| slot.realization.clone())
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

    pub fn remove(&self, session: &str) -> Option<SessionRuntimeSlot> {
        let removed = self
            .0
            .lock()
            .expect("session runtime slots mutex poisoned")
            .remove(session);
        if let Some(slot) = &removed {
            if let Some(preparation) = &slot.resource_preparation {
                preparation.cancel();
            }
            slot.realization_changed.notify_waiters();
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
                slot.realization_authority
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
mod tests {
    use super::*;

    #[test]
    fn realization_authority_close_and_renewal_have_one_projection() {
        // Decision table: C1 absent/live/closed; C2 expected exact/stale/none;
        // C3 renewal snapshot exact/stale; C4 renewed same-generation expiry
        // shorter/equal/longer. Effects: close hides authority and cancels I/O;
        // stale close changes nothing; only exact live monotonic renewal wins;
        // closed replay cannot regain authority, a fresh generation can.
        // Q1 absent+exact rejects; Q2 absent+none closes; Q3 live+stale rejects;
        // Q4 live+exact closes; Q5 closed+exact retry succeeds; Q6-Q8 exact
        // renewal crosses the three expiry partitions; Q9 stale reply rejects.
        let base = crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000;
        let lease = awaken_session_resource_contract::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/1".into(),
            epoch: 1,
            expires_at_unix_ms: base + 100,
        };
        let mut slot = SessionRuntimeSlot::default();
        assert!(!slot.close_realization_authority(Some(&lease)), "Q1");
        assert!(slot.close_realization_authority(None), "Q2");
        assert!(
            !slot.resource_preparation_authorized(&lease),
            "Q2 no-preparation fallback denied"
        );
        slot.install_realization_lease(test_observed_lease(lease.clone()));
        let mut stale = lease.clone();
        stale.expires_at_unix_ms -= 1;
        let cancellation = Arc::new(awaken_runtime_contract::CancellationToken::new());
        slot.resource_preparation = Some(cancellation.clone());
        assert!(!slot.close_realization_authority(Some(&stale)), "Q3");
        assert!(!cancellation.is_cancelled(), "Q3");
        assert!(slot.close_realization_authority(Some(&lease)), "Q4");
        assert!(cancellation.is_cancelled(), "Q4");
        assert!(slot.close_realization_authority(Some(&lease)), "Q5");
        for expiry in [base + 99, base + 100, base + 101] {
            let mut renewed = lease.clone();
            renewed.expires_at_unix_ms = expiry;
            assert!(
                !slot.renew_realization_lease(&lease, test_observed_lease(renewed.clone())),
                "Q5 closed reply"
            );
            slot.install_realization_lease(test_observed_lease(renewed.clone()));
            assert!(
                slot.realization_authority.lease().is_none(),
                "Q5 closed assertion"
            );
            let mut live = SessionRuntimeSlot::default();
            live.install_realization_lease(test_observed_lease(lease.clone()));
            assert_eq!(
                live.renew_realization_lease(&lease, test_observed_lease(renewed.clone())),
                expiry >= base + 100,
                "Q6-Q8"
            );
            assert_eq!(
                live.realization_authority.lease(),
                Some(if expiry >= base + 100 {
                    &renewed
                } else {
                    &lease
                })
            );
            assert!(
                !live.renew_realization_lease(&stale, test_observed_lease(renewed)),
                "Q9"
            );
        }
        let mut next = lease;
        next.epoch += 1;
        slot.install_realization_lease(test_observed_lease(next.clone()));
        assert_eq!(
            slot.realization_authority.lease(),
            Some(&next),
            "fresh Control generation"
        );
    }

    #[test]
    fn dispatch_resource_manifest_has_one_session_generation_selector() {
        // Cause/effect graph: C1 the slot is ordinary/prepared Session; C2 a
        // desired transition is absent/present; C3 active is absent/present.
        // Effects: E1 ordinary dispatch selects active only; E2 prepared
        // dispatch selects desired before physical publication; E3 desired
        // wins over stale active; E4 a legacy Session falls back to active; E5
        // selection never publishes or replaces active.
        //
        // | Rule | C1       | C2   | C3   | Selected | Active after |
        // | S1   | ordinary | A->B | A    | A        | A            |
        // | S2   | Session  | 0->B | none | B        | none         |
        // | S3   | Session  | A->B | A    | B        | A            |
        // | S4   | Session  | none | A    | A        | A            |
        let manifest = |revision| {
            awaken_session_resource_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                revision,
                awaken_session_resource_contract::ResolvedSessionResources::default(),
            )
        };
        for (rule, session_dispatch, active, desired, expected) in [
            ("S1", false, Some(4), Some(7), 4),
            ("S2", true, None, Some(7), 7),
            ("S3", true, Some(4), Some(7), 7),
            ("S4", true, Some(4), None, 4),
        ] {
            let active = active.map(manifest);
            let transition = desired.map(|revision| {
                awaken_session_resource_contract::SessionResourceTransition::new(
                    active.clone().unwrap_or_else(|| manifest(0)),
                    manifest(revision),
                )
                .expect("test transition belongs to one Workspace")
            });
            let slot = SessionRuntimeSlot {
                session_dispatch,
                manifest: active.clone(),
                resource_transition: transition,
                ..Default::default()
            };

            assert_eq!(
                slot.dispatch_resource_manifest()
                    .unwrap_or_else(|| panic!("{rule} selected manifest"))
                    .revision,
                expected,
                "{rule} selection",
            );
            assert_eq!(slot.manifest, active, "{rule}/E5 active is unchanged");
        }
    }

    #[test]
    fn environment_only_session_lease_remains_supervised() {
        // FMECA/causal graph: C1 Environment startup outlives the initial lease;
        // C2 the Session has no MCP projection; C3 supervision filters on MCP.
        // E1 C1+C2 must still expose the lease for renewal, otherwise C3 lets
        // the durable fence expire before the Environment receipt can commit.
        let slots = SessionRuntimeSlots::default();
        slots.update("environment-only", |slot| {
            assert!(!slot.has_mcp_projection, "C2");
            slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
                test_observed_lease(awaken_session_resource_contract::SessionRealizationLease {
                    owner: "worker".into(),
                    runtime_incarnation: "worker/incarnation".into(),
                    epoch: 1,
                    expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms(
                    ) + 30_000,
                }),
            );
        });

        let leases = slots.realization_leases();
        assert_eq!(leases.len(), 1, "E1");
        assert_eq!(leases[0].0, "environment-only", "E1");
    }

    #[tokio::test]
    async fn unpolled_staging_future_drop_releases_drain_without_false_proof() {
        // Cause/effect rule U1: C1 the canonical Staging projection is already
        // installed; C2 its spawned Future captures the activity guard but is
        // dropped before first poll; C3 drain has already claimed that exact
        // owner. Effects: E1 drain remains pending before C2, E2 Future drop
        // completes the same activity, and E3 the retry emits exact Removed
        // proof only after the unstarted spawn owner is known to be gone.
        let host = std::sync::Arc::new(crate::SharedHost::new(
            std::sync::Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        ));
        let generation =
            crate::test_mcp::mcp_fence("unpolled-staging", "docs", 1, "runtime-1", 1, u64::MAX);
        let request = awaken_session_mcp_contract::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation.clone(),
            realization_id: "realize-1".into(),
            stage_idempotency_key: "stage-1".into(),
            name: "docs".into(),
            target: awaken_session_mcp_contract::McpTarget::parse_http("https://mcp.example.test")
                .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        let staging = McpStagingActivity::default();
        let guard = staging.guard();
        let unpolled = async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        };
        host.insert_mcp_projection(McpGenerationProjection {
            receipt: awaken_session_mcp_contract::McpRealizationReceipt {
                generation: generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: None,
                actual_realization_kind: None,
                receipt_fingerprint: request.fingerprint(),
            },
            request,
            server: None,
            native_wiring: None,
            mcp_process: None,
            staging: Some(staging),
            drain: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            state: McpProjectionState::Staging,
        })
        .unwrap();

        let drain_host = host.clone();
        let drain_generation = generation.clone();
        let draining = tokio::spawn(async move {
            drain_host
                .drain_mcp_projections("unpolled-staging", &[drain_generation])
                .await
        });
        loop {
            if host
                .mcp_projection(&generation)
                .is_some_and(|projection| projection.state == McpProjectionState::Draining)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!draining.is_finished(), "U1/E1 no early proof");

        drop(unpolled);
        let proof = tokio::time::timeout(std::time::Duration::from_millis(50), draining)
            .await
            .expect("U1/E2 unpolled Future drop releases drain")
            .expect("U1/E2 drain task remains owned")
            .expect("U1/E3 exact drain succeeds");
        assert_eq!(
            proof.generations.as_slice(),
            std::slice::from_ref(&generation),
            "U1/E3"
        );
        assert!(
            host.mcp_projection(&generation).is_some_and(|projection| {
                projection.state == McpProjectionState::Removed
                    && projection.mcp_process.is_none()
                    && projection.staging.is_none()
            }),
            "U1/E3 Removed only after the captured owner drops"
        );
    }

    #[test]
    fn mcp_quiescence_admission_reopens_only_from_post_suspend_authority() {
        // Cause graph: C1 exact suspend/source/generation closes admission; C2
        // the same close replays or conflicts; C3 Control projects the old
        // Resident, a wrong generation, an exact restored Resident, or an
        // explicit source-free Unmaterialized state. C4 the old Environment
        // owner is still pending or is exactly Vacant. Effects: E1 close once;
        // E2 reject conflict and retain fence; E3 stale/early projections stay
        // closed; E4 exact restore/fresh-generation authority reopens once.
        //
        // | Rule | Close | Projected Environment | Effect |
        // |---|---|---|---|
        // | F1 | exact first/replay | none | E1 / idempotent |
        // | F2 | foreign | none | E2 |
        // | F3 | exact | old Resident / wrong generation | E3 |
        // | F4 | exact | same generation + new effect/binding | E4 |
        // | F5 | exact | Unmaterialized + pending source owner | E3 |
        // | F6 | exact | Unmaterialized + Vacant + no local effects | E4 |
        let slots = SessionRuntimeSlots::default();
        let generation = awaken_session_state_contract::SandboxGeneration::new(
            &awaken_session_types::SessionId::from("mcp-admission"),
            1,
            100,
            "environment",
            "base",
        );
        let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
            "workspace",
            &awaken_session_types::SessionId::from("mcp-admission"),
            "suspend",
            &generation,
            4,
            None,
            None,
        );
        let fence = McpQuiescenceAdmissionFence::new(
            &operation,
            "source-effect",
            "source-binding",
            &generation,
        );
        assert!(
            slots
                .close_mcp_realization_admission("mcp-admission", fence.clone())
                .unwrap(),
            "F1/E1"
        );
        assert!(
            !slots
                .close_mcp_realization_admission("mcp-admission", fence.clone())
                .unwrap(),
            "F1 replay"
        );
        let mut foreign = fence.clone();
        foreign.activity_epoch += 1;
        assert!(
            slots
                .close_mcp_realization_admission("mcp-admission", foreign)
                .is_err(),
            "F2/E2"
        );
        let old_resident = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "source-binding".into(),
            effect_id: Some("source-effect".into()),
            generation: Some(generation.clone()),
            idle_since_unix_ms: None,
        };
        assert!(
            !slots.update("mcp-admission", |slot| {
                slot.environment_owner = SessionEnvironmentOwner::Preparing(
                    SessionEnvironmentPreparation::AwaitingAdoption {
                        identity: BoundSessionEnvironmentIdentity::Durable {
                            effect_id: "source-effect".into(),
                            generation: generation.clone(),
                        },
                        binding: "source-binding".into(),
                    },
                );
                slot.reopen_mcp_realization_admission_from_projection(&old_resident)
            }),
            "F3/E3 old Resident"
        );
        let wrong_generation = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "restored-binding".into(),
            effect_id: Some("restore-effect".into()),
            generation: Some(awaken_session_state_contract::SandboxGeneration::new(
                &awaken_session_types::SessionId::from("mcp-admission"),
                2,
                100,
                "environment",
                "base",
            )),
            idle_since_unix_ms: None,
        };
        assert!(
            !slots.update("mcp-admission", |slot| {
                slot.environment_owner = SessionEnvironmentOwner::Preparing(
                    SessionEnvironmentPreparation::AwaitingAdoption {
                        identity: BoundSessionEnvironmentIdentity::Durable {
                            effect_id: "restore-effect".into(),
                            generation: wrong_generation.generation().unwrap().clone(),
                        },
                        binding: "restored-binding".into(),
                    },
                );
                slot.reopen_mcp_realization_admission_from_projection(&wrong_generation)
            }),
            "F3/E3 wrong generation"
        );
        let restored = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: "restored-binding".into(),
            effect_id: Some("restore-effect".into()),
            generation: Some(generation),
            idle_since_unix_ms: None,
        };
        assert!(
            slots.update("mcp-admission", |slot| {
                slot.environment_owner = SessionEnvironmentOwner::Preparing(
                    SessionEnvironmentPreparation::AwaitingAdoption {
                        identity: BoundSessionEnvironmentIdentity::Durable {
                            effect_id: "restore-effect".into(),
                            generation: restored.generation().unwrap().clone(),
                        },
                        binding: "restored-binding".into(),
                    },
                );
                slot.reopen_mcp_realization_admission_from_projection(&restored)
            }),
            "F4/E4"
        );
        assert!(slots.mcp_realization_admitted("mcp-admission"), "F4/E4");

        slots
            .close_mcp_realization_admission("mcp-admission", fence.clone())
            .unwrap();
        assert!(
            !slots.update("mcp-admission", |slot| {
                slot.environment_owner = SessionEnvironmentOwner::Preparing(
                    SessionEnvironmentPreparation::AwaitingAdoption {
                        identity: BoundSessionEnvironmentIdentity::Durable {
                            effect_id: fence.source_effect_id.clone(),
                            generation: fence.generation.clone(),
                        },
                        binding: fence.source_binding.clone(),
                    },
                );
                slot.reopen_mcp_realization_admission_from_projection(
                    &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
                )
            }),
            "F5/E3 expiry before source disposal"
        );
        assert!(
            slots.update("mcp-admission", |slot| {
                slot.environment_owner = SessionEnvironmentOwner::Vacant;
                slot.reopen_mcp_realization_admission_from_projection(
                    &awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
                )
            }),
            "F6/E4 exact source-free projection"
        );
        assert!(slots.mcp_realization_admitted("mcp-admission"), "F6/E4");
    }
}
