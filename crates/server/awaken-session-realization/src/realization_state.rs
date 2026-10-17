// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
//! The realization state of one installed Session, held under the installed
//! Session's lock: one state per realization component (Environment holding,
//! Resource staging, terminal effects, execution authority) and the
//! process-local projection of the Control-issued realization authority.
//! This module is the leaf of the crate's module graph: it names the state the
//! components act on and depends on none of them.

pub mod projection_receipt;
pub use projection_receipt::FrozenProjectionReceipt;

use crate::environment_owner::{
    BoundSessionEnvironment, BoundSessionEnvironmentIdentity, SessionEnvironmentOwner,
    SessionEnvironmentPreparation,
};
use crate::memory_binding::MemoryBinding;
use awaken_session_configuration_contract::resource as session_resource;
use std::sync::Arc;

/// One process-local projection of aggregate authority, not another lease.
/// Closing retains the exact lease for cleanup CAS, but never for admission.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionRealizationAuthority {
    #[default]
    Uninstalled,
    Live(awaken_session_realization_contract::ObservedSessionRealizationLease),
    Closed(Option<session_resource::SessionRealizationLease>),
}

impl SessionRealizationAuthority {
    pub fn proof(
        &self,
    ) -> Option<&awaken_session_realization_contract::ObservedSessionRealizationLease> {
        match self {
            Self::Live(proof) => Some(proof),
            Self::Uninstalled | Self::Closed(_) => None,
        }
    }

    pub fn remaining(&self) -> Option<std::time::Duration> {
        self.proof()?
            .remaining_at(awaken_authority_clock::authority_proof_started().ok()?)
    }

    /// One local admission rule: exact aggregate generation and a still-live
    /// request-relative observation. Callers hold the existing installed Session lock;
    /// neither wall time nor a retained Closed lease grants authority.
    pub fn remaining_for(
        &self,
        asserted: &session_resource::SessionRealizationLease,
    ) -> Option<std::time::Duration> {
        awaken_session_realization_contract::realization_lease_generation_authorizes(
            self.lease()?,
            asserted,
        )
        .then(|| self.remaining())
        .flatten()
    }

    pub fn lease(&self) -> Option<&session_resource::SessionRealizationLease> {
        match self {
            Self::Live(proof) => Some(&proof.lease),
            Self::Uninstalled | Self::Closed(_) => None,
        }
    }

    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }

    /// Called under the installed Session lock used for renewal application. The
    /// pending provider GO callback must hold this same mutex through its grant.
    /// A retry may select the retained closed identity; a stale revoker cannot.
    pub fn close(&mut self, expected: Option<&session_resource::SessionRealizationLease>) -> bool {
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

/// The environment-realization layer: who owns the Session's Environment,
/// under which realization lease, and which Resources, memory, Skills and MCP
/// generations are staged for it. Built while realizing; revocation keeps
/// only what a retry of a retained Environment owner still needs.
///
/// One state per realization component. Each is reached through a shared view
/// or a mutable view of that one state only; a change that spans states is a
/// named method of this type, so no caller edits two components' state through
/// one borrow.
#[derive(Default)]
pub struct SessionRealization {
    environment: EnvironmentHoldingState,
    staging: ResourceStagingState,
    effects: TerminalEffectsState,
    execution: ExecutionAuthorityState,
}

impl SessionRealization {
    /// The Environment holding state.
    pub fn environment(&self) -> &EnvironmentHoldingState {
        &self.environment
    }

    /// The Environment holding state, for a change of that state alone.
    pub fn environment_mut(&mut self) -> &mut EnvironmentHoldingState {
        &mut self.environment
    }

    /// The Resource staging state.
    pub fn staging(&self) -> &ResourceStagingState {
        &self.staging
    }

    /// The Resource staging state, for a change of that state alone.
    pub fn staging_mut(&mut self) -> &mut ResourceStagingState {
        &mut self.staging
    }

    /// The terminal effects state.
    pub fn effects(&self) -> &TerminalEffectsState {
        &self.effects
    }

    /// The terminal effects state, for a change of that state alone.
    pub fn effects_mut(&mut self) -> &mut TerminalEffectsState {
        &mut self.effects
    }

    /// The execution authority state.
    pub fn execution(&self) -> &ExecutionAuthorityState {
        &self.execution
    }

    /// The execution authority state, for a change of that state alone.
    pub fn execution_mut(&mut self) -> &mut ExecutionAuthorityState {
        &mut self.execution
    }

    /// The original canonical MCP root rule, read under the installed Session lock.
    /// Absence remains explicit and Closed never grants continuing admission.
    pub fn mcp_generation_root_authority(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Option<bool> {
        if self.execution.authority.is_closed() {
            return Some(false);
        }
        self.execution.authority.lease().map(|current| {
            let asserted =
                awaken_session_configuration_contract::resource::SessionRealizationLease {
                    owner: current.owner.clone(),
                    runtime_incarnation: generation.runtime_incarnation().to_owned(),
                    epoch: generation.lease_epoch().get(),
                    expires_at_unix_ms: generation.lease_expires_at_unix_ms(),
                };
            self.execution.authority.remaining_for(&asserted).is_some()
        })
    }

    pub fn validate_preparation_publication(
        &self,
        asserted: Option<&session_resource::SessionRealizationLease>,
    ) -> Result<(), awaken_session_types::RunError> {
        if self.staging.preparation.is_some()
            && !asserted.is_some_and(|lease| self.resource_preparation_authorized(lease))
        {
            return Err(awaken_session_types::RunError::unavailable_classified(
                "session_realization_authority_lost",
                "Resource preparation cannot publish after realization authority ended",
            ));
        }
        Ok(())
    }

    /// Resources the Environment held before an in-flight resource transition.
    pub fn previous_resources(&self) -> Option<session_resource::ResolvedSessionResources> {
        let transition = self.staging.transition.as_ref()?;
        Some(transition.previous().resources.clone())
    }

    /// What revocation keeps while an Environment owner is still retained:
    /// the installed Session's lock identities, the closed authority, the owner itself and
    /// the content it was staged with. Everything else is rebuilt.
    pub fn take_retained_on_revocation(&mut self) -> Self {
        Self {
            execution: ExecutionAuthorityState {
                phase: self.execution.phase.clone(),
                changed: self.execution.changed.clone(),
                authority: self.execution.authority.clone(),
                ..Default::default()
            },
            environment: EnvironmentHoldingState {
                lifecycle: self.environment.lifecycle.clone(),
                owner: std::mem::take(&mut self.environment.owner),
                ..Default::default()
            },
            staging: ResourceStagingState {
                projection: self.staging.projection.clone(),
                content_delivery: self.staging.content_delivery,
                resources: std::mem::take(&mut self.staging.resources),
                // Only the canonical durable projection may reopen MCP effects.
                mcp: self.staging.mcp.take_retained_on_revocation(),
                ..Default::default()
            },
            effects: Default::default(),
        }
    }

    /// The Environment's arrival or disposal decides how Resource staging
    /// reconciles its Resources with it next.
    pub fn set_resource_reconciliation(
        &mut self,
        reconciliation: crate::staged_resources::EnvironmentResourceReconciliation,
    ) {
        self.staging.reconciliation = reconciliation;
    }

    /// End a Resource preparation scope: release its cancellation and, when the
    /// scope introduced the current lease generation and nothing published
    /// under it, withdraw that lease. Another preparation or generation is left
    /// untouched.
    pub fn end_resource_preparation(
        &mut self,
        cancellation: &Arc<awaken_runtime_contract::CancellationToken>,
        introduced: Option<&session_resource::SessionRealizationLease>,
    ) {
        if !self
            .staging
            .preparation
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, cancellation))
        {
            return;
        }
        self.staging.preparation = None;
        if introduced.is_some_and(|asserted| {
            self.execution.authority.lease().is_some_and(|current| {
                awaken_session_realization_contract::realization_lease_generation_authorizes(
                    current, asserted,
                )
            })
        }) {
            self.execution.authority = SessionRealizationAuthority::Uninstalled;
            self.execution.changed.notify_waiters();
        }
    }

    /// The installed Session was removed: its Resource preparation can no
    /// longer publish, and every waiter on its authority re-reads and finds it
    /// gone. Run after releasing the installed Sessions' lock.
    pub fn end_on_removal(&self) {
        if let Some(preparation) = &self.staging.preparation {
            preparation.cancel();
        }
        self.execution.changed.notify_waiters();
    }

    pub fn close_authority(
        &mut self,
        expected: Option<&session_resource::SessionRealizationLease>,
    ) -> bool {
        if !self.execution.authority.close(expected) {
            return false;
        }
        if let Some(preparation) = &self.staging.preparation {
            preparation.cancel();
        }
        self.execution.changed.notify_waiters();
        true
    }

    pub fn install_lease(
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
        if let SessionRealizationAuthority::Closed(Some(closed)) = &self.execution.authority
            && (awaken_session_realization_contract::realization_lease_generation_authorizes(
                closed, lease,
            ) || awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, closed,
            ))
        {
            // An assertion or delayed renewal cannot reopen a closed generation.
            return false;
        }
        if let Some(current) = self.execution.authority.lease()
            && (awaken_session_realization_contract::realization_lease_generation_authorizes(
                current, lease,
            ) || awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, current,
            ))
            && self.execution.authority.remaining().is_none()
        {
            return false;
        }
        if self.execution.authority.lease().is_some_and(|current| {
            awaken_session_realization_contract::realization_lease_generation_authorizes(
                current, lease,
            )
        }) {
            // A slow preparation must not overwrite an intervening renewal.
            return true;
        }
        if self.execution.authority.lease().is_some_and(|current| {
            !awaken_session_realization_contract::realization_lease_generation_authorizes(
                lease, current,
            )
        }) && let Some(preparation) = &self.staging.preparation
        {
            preparation.cancel();
        }
        self.execution.authority = SessionRealizationAuthority::Live(proof);
        true
    }

    /// The only renewal response application edge. Callers with an I/O
    /// snapshot must supply it unchanged; a fresh same-lock caller may use
    /// the currently visible lease. Neither can recreate absent/closed state.
    pub fn renew_lease(
        &mut self,
        asserted: &session_resource::SessionRealizationLease,
        renewed: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> bool {
        if self.execution.authority.lease() != Some(asserted)
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
        if self.execution.authority.remaining().is_none() || renewed.remaining_at(now).is_none() {
            return false;
        }
        self.execution.authority = SessionRealizationAuthority::Live(renewed);
        self.execution.changed.notify_waiters();
        true
    }

    pub fn resource_preparation_authorized(
        &self,
        asserted: &session_resource::SessionRealizationLease,
    ) -> bool {
        !self.execution.authority.is_closed()
            && self.staging.preparation.as_ref().is_none_or(|preparation| {
                !preparation.is_cancelled()
                    && self.execution.authority.remaining_for(asserted).is_some()
            })
    }
}

impl SessionRealization {
    /// Install a projected Environment owner and, in the same installed Session lock, let
    /// the canonical projection reopen MCP admission when the run layer holds
    /// no local effect.
    pub fn install_owner(
        &mut self,
        owner: SessionEnvironmentOwner,
        state: &awaken_session_state_contract::SessionEnvironmentState,
        holds_context: bool,
    ) {
        self.environment.owner = owner;
        self.reopen_mcp_realization_admission_from_projection(state, holds_context);
    }

    /// Consume the process-local fence only while installing one authoritative
    /// durable Environment projection and while every old local effect owner is
    /// already gone. This keeps expiry and restore from reopening MCP admission
    /// during the gap between their durable transition and source cleanup.
    pub fn reopen_mcp_realization_admission_from_projection(
        &mut self,
        environment: &awaken_session_state_contract::SessionEnvironmentState,
        holds_context: bool,
    ) -> bool {
        let Some(fence) = self.staging.mcp.quiescence_fence() else {
            return false;
        };
        let local_effects_cleared = !holds_context && self.staging.mcp.local_effects_cleared();
        if !local_effects_cleared {
            return false;
        }
        let authoritative_reopen = if fence.restored_by(environment) {
            matches!(
                (&self.environment.owner, environment),
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
                (&self.environment.owner, environment),
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
            ) && matches!(&self.environment.owner, SessionEnvironmentOwner::Vacant)
        };
        if !authoritative_reopen {
            return false;
        }
        self.staging.mcp.reopen_admission();
        true
    }
}

/// Per-Session state held under the installed Session lock.
#[derive(Default)]
pub struct EnvironmentHoldingState {
    /// Serializes first materialization/rebuild for this Session without a
    /// process-wide registry lock being held across I/O.
    pub lifecycle: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Sole live Environment owner. Every state transition is driven through
    /// `environment_lifecycle.rs`; other modules may only inspect
    /// Resident/occupancy projections through its read helpers.
    pub owner: crate::environment_owner::SessionEnvironmentOwner,
    /// Exact unavailable aggregate binding/generation authorized for one
    /// RebuildFromCommittedTruth replacement. This is a process-local command
    /// projection; the Session aggregate receipt remains the only transition
    /// authority and clears it after durable publication.
    pub rebuild_source: Option<(String, Option<String>)>,
}

/// Per-Session state held under the installed Session lock.
#[derive(Default)]
pub struct ResourceStagingState {
    /// Serializes one complete Resource-generation compare/realize/publish
    /// transition. The global lock order is `realization -> lifecycle ->
    /// resource_projection`; callers may acquire any suffix but never reverse
    /// it. Keeping this separate lets projection installation remain atomic to
    /// Runtime observers without re-entering `lifecycle` during Resource sync.
    pub projection: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Derived once while the Session context is built. Prompt projection,
    /// mount realization, Native tools, and ACP export all consume this value;
    /// none may independently choose another delivery path.
    pub content_delivery: Option<crate::frozen_inputs::ManagedContentDelivery>,
    /// Every standard MemoryStore resource frozen for this Session, keyed by
    /// binding id. These exist independently of the optional Awaken automatic
    /// memory extension selected into `memory` below.
    pub memory_bindings: std::collections::HashMap<String, Arc<MemoryBinding>>,
    /// The one explicitly selected automatic recall/extraction binding. This is
    /// never the authority for which standard resources are mounted.
    pub memory: Option<Arc<MemoryBinding>>,
    /// The Session's MCP generation owners, quiescence fence, and frozen
    /// attachment marker. Other Host modules use only its methods.
    pub mcp: awaken_session_mcp::SessionMcpState,
    /// Cancellation of the current Resource preparation only. This carries no
    /// ownership: `authority` and Control remain the sole authorities.
    pub preparation: Option<Arc<awaken_runtime_contract::CancellationToken>>,
    /// `Some([])` means the frozen manifest delivers no Skills; `None` means this
    /// embedded Session has no frozen Skill manifest.
    pub skills: Option<Vec<awaken_resource_contract::SkillVersion>>,
    /// Managed-filesystem Skill discovery metadata. Full instructions remain
    /// in the exact frozen bundle (and its later filesystem projection) and
    /// are read on demand by the ordinary `read` tool.
    pub skill_prompt: Option<crate::skill_delivery::FilesystemSkillPrompt>,
    pub resources: crate::staged_resources::StagedResources,
    pub manifest: Option<awaken_session_configuration_contract::resource::SessionResourceManifest>,
    /// Exact aggregate-owned physical replacement currently being replayed.
    /// This is a rebuildable command projection, not completion state: the
    /// durable Resource aggregate remains the only active/pending authority.
    pub transition:
        Option<awaken_session_configuration_contract::resource::SessionResourceTransition>,
    /// Fingerprint of the exact transition + optional Run claim whose immutable
    /// inputs were compiled into `resources`, `memory_bindings`, and `skills`.
    /// This avoids duplicate File/Vault/catalog reads between deferred staging
    /// and the same process's later physical Environment convergence.
    pub effect_key: Option<String>,
    /// How the resident Environment entered this process. Fresh substrates and
    /// adopted providers require different idempotent mount completion, but the
    /// aggregate transition remains their single desired-state authority.
    pub reconciliation: crate::staged_resources::EnvironmentResourceReconciliation,
}

impl ResourceStagingState {
    /// Select the one Resource manifest carried by a newly authored durable
    /// Run dispatch. A prepared Session carries the aggregate's desired
    /// generation so a cold Worker can realize that exact transition; legacy
    /// Sessions fall back to the physically active manifest. Ordinary Runs use
    /// active truth only and are never promoted by a staged transition.
    ///
    /// This read is intentionally side-effect free. Publishing `manifest`
    /// remains the exclusive completion edge for physical Resource effects.
    pub fn run_manifest(
        &self,
        session_dispatch: bool,
    ) -> Option<awaken_session_configuration_contract::resource::SessionResourceManifest> {
        if session_dispatch {
            self.transition
                .as_ref()
                .map(|transition| transition.desired().clone())
                .or_else(|| self.manifest.clone())
        } else {
            self.manifest.clone()
        }
    }
}

/// Per-Session state held under the installed Session lock.
#[derive(Default)]
pub struct TerminalEffectsState {
    /// One complete Control-issued terminal assignment. Its Environment state
    /// and no-Create witness are observed together; authority remains in `authority`.
    pub assignment: Option<awaken_session_realization_contract::SessionTerminalCleanupAssignment>,
    /// Process-local parent of a terminal cleanup target. This is routing
    /// metadata only: the aggregate's frozen command set remains the sole work
    /// and receipt authority. It lets an accepted root receipt (or a later
    /// aggregate Completed read after response loss) retire child projections
    /// without maintaining a second cleanup registry.
    pub root: Option<String>,
}

/// Per-Session state held under the installed Session lock.
#[derive(Default)]
pub struct ExecutionAuthorityState {
    /// Serializes the canonical realization phase driver for this Session.
    /// Lease renewal may advance authority while this lock is held, but it must
    /// not start a second Stage/Publish driver. This is deliberately separate
    /// from `lifecycle`, which is re-entered by Environment materialization.
    pub phase: Arc<tokio::sync::Mutex<()>>,
    /// Last accepted aggregate version and equality digest; rebuildable, never authority.
    pub frozen_receipt: Option<projection_receipt::FrozenProjectionReceipt>,
    /// Current Control-issued projection authority. It is a live cache used to
    /// request renewal; durable ownership remains in the Session aggregate.
    pub authority: SessionRealizationAuthority,
    /// Wakes a claim resolver waiting for the Session application's initial
    /// restart reassignment. The lease above remains the only projected fact;
    /// this notification carries no authority or parallel state.
    pub changed: Arc<tokio::sync::Notify>,
}
