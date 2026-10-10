// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
//! The realization layer of one installed Session: its Environment owner,
//! the Control-issued realization lease, and what is staged for it.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_session_mcp::SessionMcpState;

use super::FrozenProjectionReceipt;
use crate::environment_owner::SessionEnvironmentOwner;
use crate::frozen_inputs::ManagedContentDelivery;
use crate::memory_binding::MemoryBinding;
use crate::staged_resources::{EnvironmentResourceReconciliation, StagedResources};

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

/// The environment-realization layer: who owns the Session's Environment,
/// under which realization lease, and which Resources, memory, Skills and MCP
/// generations are staged for it. Built while realizing; revocation keeps
/// only what a retry of a retained Environment owner still needs.
#[derive(Default)]
pub(crate) struct SessionRealization {
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
    pub memory_bindings: HashMap<String, Arc<MemoryBinding>>,
    /// The one explicitly selected automatic recall/extraction binding. This is
    /// never the authority for which standard resources are mounted.
    pub memory: Option<Arc<MemoryBinding>>,
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
    pub skill_prompt: Option<crate::skill_delivery::FilesystemSkillPrompt>,
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

impl SessionRealization {
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
