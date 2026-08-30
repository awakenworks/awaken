//! Sandbox-lease liveness and crash-adoption reconciliation (ADR-0021 §6).
//!
//! A realized sandbox outlives the host that placed a run on it, so the fleet
//! keeps it alive with a **dead-man switch** ([`LeaseGrant`], renewed within the
//! spec's `lease_ttl_secs`) and, after a host restart, decides per sandbox whether
//! to **adopt** (a run still needs it), **reap** (nothing references it), or treat
//! it as **orphaned** (a run points at a sandbox that is already gone).
//!
//! This module is neutral: it names no dispatch/run type. The caller supplies two
//! sets of [`SandboxHandle`]s — the sandboxes a driver reports *live*, and the
//! sandboxes still *referenced* by a live run — and [`reconcile_adoption`] is a
//! pure function over them. The run↔sandbox binding itself lives in the dispatch
//! aggregate (`Claimed`), never here.

use crate::sandbox::{SandboxHandle, SandboxProvider};

/// A lease deadline over a realized sandbox — the dead-man switch the owner renews
/// within `lease_ttl_secs`. `None` is an indefinite lease (dev/trusted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseGrant {
    /// Wall-clock ms after which the owner is fenced; `None` = never expires.
    pub expires_ms: Option<u64>,
}

impl LeaseGrant {
    /// An indefinite lease (no deadline-based fencing).
    #[must_use]
    pub fn indefinite() -> Self {
        Self { expires_ms: None }
    }

    /// A lease expiring at `expires_ms`.
    #[must_use]
    pub fn until(expires_ms: u64) -> Self {
        Self {
            expires_ms: Some(expires_ms),
        }
    }

    /// Classify the lease at `now_ms`. `renew_margin_ms` is how close to expiry an
    /// owner should already be renewing — the band the pool heartbeat targets so a
    /// live run is fenced only after multiple missed renewal opportunities.
    #[must_use]
    pub fn liveness(&self, now_ms: u64, renew_margin_ms: u64) -> LeaseLiveness {
        match self.expires_ms {
            None => LeaseLiveness::Live,
            Some(exp) if now_ms >= exp => LeaseLiveness::Expired,
            Some(exp) if exp.saturating_sub(now_ms) <= renew_margin_ms => LeaseLiveness::Expiring,
            Some(_) => LeaseLiveness::Live,
        }
    }
}

/// Where a sandbox lease sits in its life: healthy, due for renewal, or dead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseLiveness {
    /// Comfortably within its deadline — no action.
    Live,
    /// Within the renew margin — the owner should renew now.
    Expiring,
    /// Past its deadline — the current owner must be fenced. Disposal remains a
    /// separate referenced-set reconciliation decision.
    Expired,
}

/// Why the current owner is fenced from further Sandbox effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseFenceCause {
    /// The lease was explicitly revoked by an authoritative control decision.
    Revoked,
    /// The lease deadline passed without renewal (owner vanished).
    Expired,
    /// The owner's transport to the sandbox was lost (channel closed) while the lease
    /// was still within its deadline — a "hung but alive" reclaim.
    TransportLost,
}

/// A lease decision can preserve the current owner or fence it. It deliberately
/// has no disposal variant: destruction is authorized only by the durable
/// referenced-set reconciliation in [`reconcile_adoption`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseAction {
    Keep,
    Fence(LeaseFenceCause),
}

/// Validated timing for a finite Sandbox lease.
///
/// Idle retention is intentionally absent. It is Session policy, while this
/// value object only proves that liveness has multiple renewal opportunities and
/// that reference reconciliation cannot run out of recovery grace immediately
/// after the lease deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseTimingPolicy {
    renew_interval_ms: u64,
    lease_ttl_ms: u64,
    reconciliation_interval_ms: u64,
    recovery_grace_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LeaseTimingError {
    #[error("lease timing values must all be non-zero")]
    Zero,
    #[error("lease TTL must provide at least three renewal intervals")]
    InsufficientRenewalWindow,
    #[error("recovery grace must cover the lease TTL and one reconciliation interval")]
    InsufficientRecoveryGrace,
}

impl LeaseTimingPolicy {
    pub fn new(
        renew_interval_ms: u64,
        lease_ttl_ms: u64,
        reconciliation_interval_ms: u64,
        recovery_grace_ms: u64,
    ) -> Result<Self, LeaseTimingError> {
        if renew_interval_ms == 0
            || lease_ttl_ms == 0
            || reconciliation_interval_ms == 0
            || recovery_grace_ms == 0
        {
            return Err(LeaseTimingError::Zero);
        }
        if renew_interval_ms > lease_ttl_ms / 3 {
            return Err(LeaseTimingError::InsufficientRenewalWindow);
        }
        if recovery_grace_ms < reconciliation_interval_ms
            || lease_ttl_ms > recovery_grace_ms - reconciliation_interval_ms
        {
            return Err(LeaseTimingError::InsufficientRecoveryGrace);
        }
        Ok(Self {
            renew_interval_ms,
            lease_ttl_ms,
            reconciliation_interval_ms,
            recovery_grace_ms,
        })
    }

    #[must_use]
    pub fn renew_interval_ms(self) -> u64 {
        self.renew_interval_ms
    }

    #[must_use]
    pub fn lease_ttl_ms(self) -> u64 {
        self.lease_ttl_ms
    }

    #[must_use]
    pub fn reconciliation_interval_ms(self) -> u64 {
        self.reconciliation_interval_ms
    }

    #[must_use]
    pub fn recovery_grace_ms(self) -> u64 {
        self.recovery_grace_ms
    }
}

/// Point-in-time liveness signals for a leased sandbox, collapsed into a single
/// ownership decision by [`decide_lease_action`]. The caller supplies each from its own source (a
/// revoke API, an `AgentChannel` close, the wall clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LivenessSignals {
    /// Wall-clock now, for the deadline check.
    pub now_ms: u64,
    /// The lease was explicitly revoked.
    pub revoked: bool,
    /// The owner's transport to the sandbox was lost.
    pub transport_lost: bool,
}

/// Cap credential expiry at both its own TTL and the owning Sandbox lease.
/// An expired lease therefore produces an already-expired credential instead
/// of extending authority beyond the lease boundary.
#[must_use]
pub fn capped_expiry(default_ttl_ms: u64, now_ms: u64, grant: &LeaseGrant) -> u64 {
    let credential_expiry = now_ms.saturating_add(default_ttl_ms);
    grant.expires_ms.map_or(credential_expiry, |lease_expiry| {
        credential_expiry.min(lease_expiry)
    })
}

/// Re-check the lease at the outbound side-effect boundary. Revocation and an
/// elapsed deadline both fail closed; this check never renews the lease.
#[must_use]
pub fn egress_permitted(grant: &LeaseGrant, now_ms: u64, revoked: bool) -> bool {
    !revoked && !matches!(grant.liveness(now_ms, 0), LeaseLiveness::Expired)
}

/// Decide whether the current owner remains usable, collapsing the signals with
/// the fixed priority **Revoked > Expired(deadline) > TransportLost**. Lease loss
/// only fences effects; it never authorizes destruction of the mutable Sandbox.
/// Heartbeat is not a signal here because it renews the deadline upstream.
#[must_use]
pub fn decide_lease_action(grant: &LeaseGrant, signals: LivenessSignals) -> LeaseAction {
    if signals.revoked {
        return LeaseAction::Fence(LeaseFenceCause::Revoked);
    }
    if matches!(grant.liveness(signals.now_ms, 0), LeaseLiveness::Expired) {
        return LeaseAction::Fence(LeaseFenceCause::Expired);
    }
    if signals.transport_lost {
        return LeaseAction::Fence(LeaseFenceCause::TransportLost);
    }
    LeaseAction::Keep
}

/// The reconciliation outcome: what to do with each live sandbox after a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdoptionPlan {
    /// Live sandboxes a run still references — reconnect (`SandboxProvider::adopt`).
    pub adopt: Vec<SandboxHandle>,
    /// Live sandboxes nothing references — reap.
    pub reap: Vec<SandboxHandle>,
    /// Sandboxes a run references but that are no longer live — the run's env is
    /// gone; it must be re-placed onto a fresh sandbox.
    pub orphan: Vec<SandboxHandle>,
}

/// The identity of a sandbox for reconciliation. Ordinary Phase-A handles
/// remain `(provider_kind, sandbox_id, None)` and ignore provider payload.
/// Phase-B restored handles additionally require exact restoration evidence.
fn key(h: &SandboxHandle) -> (&str, &str, Option<&crate::SandboxRestorationEvidence>) {
    (h.provider_kind(), h.sandbox_id.as_str(), h.restoration())
}

/// Reconcile live sandboxes against those still referenced by a live run.
///
/// - live ∩ referenced → **adopt** (reconnect; a run still needs it)
/// - live ∖ referenced → **reap** (nothing needs it)
/// - referenced ∖ live → **orphan** (the run's sandbox died; re-place it)
///
/// Pure and neutral: identity is `(provider_kind, sandbox_id, restoration)`;
/// `referenced` is supplied by the caller from live `Claimed` bindings.
#[must_use]
pub fn reconcile_adoption(live: &[SandboxHandle], referenced: &[SandboxHandle]) -> AdoptionPlan {
    let referenced_keys: Vec<_> = referenced.iter().map(key).collect();
    let live_keys: Vec<_> = live.iter().map(key).collect();

    let mut plan = AdoptionPlan::default();
    for h in live {
        if referenced_keys.contains(&key(h)) {
            plan.adopt.push(h.clone());
        } else {
            plan.reap.push(h.clone());
        }
    }
    for h in referenced {
        if !live_keys.contains(&key(h)) {
            plan.orphan.push(h.clone());
        }
    }
    plan
}

/// The result of actuating an [`AdoptionPlan`] against a provider — the node-agent
/// restart reconcile step atop the pure [`reconcile_adoption`] planner. Best-effort
/// and idempotent: a per-handle actuation error lands in `failed` (a later tick
/// retries) rather than aborting the whole reconcile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// Reconnected sandboxes a live run still references.
    pub adopted: Vec<SandboxHandle>,
    /// Sandboxes reaped because nothing referenced them.
    pub reaped: Vec<SandboxHandle>,
    /// Referenced-but-dead sandboxes the caller must re-place onto fresh sandboxes.
    pub orphaned: Vec<SandboxHandle>,
    /// Handles whose adopt/dispose actuation errored this tick (retried next tick).
    pub failed: Vec<SandboxHandle>,
}

/// Actuate a reconciliation plan against `provider`: reconnect (`adopt`) the
/// sandboxes a run still references, reap (reconnect → `dispose`) the ones nothing
/// references, and pass orphans through for the caller to re-place. Best-effort per
/// handle so a single failure never strands the rest of the fleet.
///
/// This generic actuator has neither the frozen [`crate::spec::SandboxSpec`] nor
/// the authoritative [`crate::sandbox::SandboxRestoreRequest`] required to prove
/// an exact restored target. Handles carrying restoration evidence therefore fail
/// closed without provider effects. Exact restored adoption/disposal belongs to
/// the dedicated R1 [`SandboxProvider::restore`] /
/// [`SandboxProvider::dispose_restored`] port owner; it must not be recreated as a
/// second cleanup path here.
pub async fn apply_adoption_plan(
    provider: &dyn SandboxProvider,
    plan: &AdoptionPlan,
) -> ReconcileOutcome {
    let mut out = ReconcileOutcome {
        orphaned: plan.orphan.clone(),
        ..Default::default()
    };
    for h in &plan.adopt {
        if h.restoration().is_some() {
            out.failed.push(h.clone());
            continue;
        }
        match provider.adopt(h).await {
            Ok(_reconnected) => out.adopted.push(h.clone()),
            Err(_) => out.failed.push(h.clone()),
        }
    }
    for h in &plan.reap {
        if h.restoration().is_some() {
            out.failed.push(h.clone());
            continue;
        }
        match provider.adopt(h).await {
            Ok(sandbox) => match sandbox.dispose().await {
                Ok(()) => out.reaped.push(h.clone()),
                Err(_) => out.failed.push(h.clone()),
            },
            Err(_) => out.failed.push(h.clone()),
        }
    }
    out
}

/// Reconcile the driver-live set against the referenced set and actuate the plan in
/// one call — the node-agent restart reconcile entry point.
pub async fn reconcile_and_apply(
    provider: &dyn SandboxProvider,
    live: &[SandboxHandle],
    referenced: &[SandboxHandle],
) -> ReconcileOutcome {
    apply_adoption_plan(provider, &reconcile_adoption(live, referenced)).await
}

#[cfg(kani)]
mod verification {
    //! Machine-checked cause/effect design:
    //! - P1 revoked=true => Fence(Revoked), masking deadline/transport;
    //! - P2 !revoked && now>=expiry => Fence(Expired), masking transport;
    //! - P3 live && transport_lost => Fence(TransportLost); otherwise Keep;
    //! - T1 accepted timing => renew*3<=ttl;
    //! - T2 accepted timing => ttl+reconcile<=recovery without overflow.
    //! `LeaseAction` has no disposal member, so every P rule proves lease loss
    //! cannot become destructive authority.
    use super::*;

    #[kani::proof]
    fn credential_expiry_never_exceeds_lease_or_own_ttl() {
        let now = kani::any::<u64>();
        let ttl = kani::any::<u64>();
        let lease_expiry = kani::any::<u64>();
        let grant = LeaseGrant::until(lease_expiry);
        let expiry = capped_expiry(ttl, now, &grant);
        assert!(expiry <= lease_expiry);
        assert!(expiry <= now.saturating_add(ttl));
    }

    #[kani::proof]
    fn revoked_or_expired_lease_always_denies_egress() {
        let now = kani::any::<u64>();
        let expiry = kani::any::<u64>();
        let revoked = kani::any::<bool>();
        let grant = LeaseGrant::until(expiry);
        if revoked || now >= expiry {
            assert!(!egress_permitted(&grant, now, revoked));
        }
    }

    /// P1-P3: explore every timestamp/boolean combination and prove the fixed
    /// masking order while the output remains in the Keep/Fence domain.
    #[kani::proof]
    fn lease_loss_obeys_fixed_fail_closed_fence_priority() {
        let now = kani::any::<u64>();
        let expiry = kani::any::<u64>();
        let revoked = kani::any::<bool>();
        let transport_lost = kani::any::<bool>();
        let grant = LeaseGrant::until(expiry);
        let got = decide_lease_action(
            &grant,
            LivenessSignals {
                now_ms: now,
                revoked,
                transport_lost,
            },
        );
        let expected = if revoked {
            LeaseAction::Fence(LeaseFenceCause::Revoked)
        } else if now >= expiry {
            LeaseAction::Fence(LeaseFenceCause::Expired)
        } else if transport_lost {
            LeaseAction::Fence(LeaseFenceCause::TransportLost)
        } else {
            LeaseAction::Keep
        };
        assert_eq!(got, expected);
    }

    /// T1: arbitrary accepted values preserve three complete renewal windows.
    #[kani::proof]
    fn valid_lease_timing_always_has_three_renewal_opportunities() {
        let renew = kani::any::<u64>();
        let ttl = kani::any::<u64>();
        let reconcile = kani::any::<u64>();
        let recovery = kani::any::<u64>();
        if let Ok(policy) = LeaseTimingPolicy::new(renew, ttl, reconcile, recovery) {
            assert!(policy.renew_interval_ms() <= policy.lease_ttl_ms() / 3);
            assert!(policy.renew_interval_ms().saturating_mul(3) <= policy.lease_ttl_ms());
        }
    }

    /// T2: arbitrary accepted values preserve one post-TTL reconciliation tick.
    #[kani::proof]
    fn valid_recovery_grace_covers_lease_and_reconciliation() {
        let renew = kani::any::<u64>();
        let ttl = kani::any::<u64>();
        let reconcile = kani::any::<u64>();
        let recovery = kani::any::<u64>();
        if let Ok(policy) = LeaseTimingPolicy::new(renew, ttl, reconcile, recovery) {
            assert!(policy.lease_ttl_ms() <= policy.recovery_grace_ms());
            assert!(
                policy.lease_ttl_ms() + policy.reconciliation_interval_ms()
                    <= policy.recovery_grace_ms()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(id: &str) -> SandboxHandle {
        SandboxHandle::new("k8s", id)
    }

    pub(super) fn restored_h(id: &str, effect_coordinate: &str) -> SandboxHandle {
        let mut encoded = serde_json::to_value(h(id)).unwrap();
        encoded.as_object_mut().unwrap().insert(
            "restoration".into(),
            serde_json::json!({
                "effect_id": awaken_agent_contract::collision_resistant_fingerprint(
                "awaken-test-restoration-effect-v1",
                &[effect_coordinate.as_bytes()],
                ),
                "generation_id": "generation-a",
                "checkpoint_id": "checkpoint-a",
                "checkpoint_digest": "digest-a",
                "sandbox_spec_fingerprint": "spec-a",
                "checkpoint_exclusions_fingerprint": "exclusions-a"
            }),
        );
        serde_json::from_value(encoded).unwrap()
    }

    #[test]
    fn indefinite_lease_is_always_live() {
        assert_eq!(
            LeaseGrant::indefinite().liveness(u64::MAX, 100),
            LeaseLiveness::Live
        );
    }

    #[test]
    fn lease_liveness_bands() {
        let g = LeaseGrant::until(1_000);
        assert_eq!(g.liveness(500, 100), LeaseLiveness::Live);
        assert_eq!(g.liveness(950, 100), LeaseLiveness::Expiring);
        assert_eq!(g.liveness(1_000, 100), LeaseLiveness::Expired);
        assert_eq!(g.liveness(1_200, 100), LeaseLiveness::Expired);
    }

    #[test]
    fn credential_expiry_is_bounded_by_ttl_and_lease() {
        let grant = LeaseGrant::until(1_000);
        assert_eq!(capped_expiry(500, 800, &grant), 1_000);
        assert_eq!(capped_expiry(100, 800, &grant), 900);
        assert_eq!(capped_expiry(500, 800, &LeaseGrant::indefinite()), 1_300);
        assert_eq!(
            capped_expiry(u64::MAX, u64::MAX, &LeaseGrant::until(1_000)),
            1_000
        );
        assert_eq!(capped_expiry(500, 2_000, &grant), 1_000);
    }

    #[test]
    fn revoked_or_expired_lease_denies_egress() {
        let grant = LeaseGrant::until(1_000);
        assert!(egress_permitted(&grant, 500, false));
        assert!(!egress_permitted(&grant, 500, true));
        assert!(!egress_permitted(&grant, 1_500, false));
        assert!(egress_permitted(&LeaseGrant::indefinite(), u64::MAX, false));
        assert!(!egress_permitted(&LeaseGrant::indefinite(), u64::MAX, true));
    }

    // Cause/effect decision table:
    // R1 all values non-zero + renew <= ttl/3 + ttl+reconcile <= recovery => valid;
    // R2 any zero => Zero; R3 too few renewal windows => InsufficientRenewalWindow;
    // R4 insufficient recovery window => InsufficientRecoveryGrace.
    #[test]
    fn lease_timing_policy_enforces_every_safety_boundary() {
        let policy = LeaseTimingPolicy::new(20_000, 90_000, 60_000, 600_000).unwrap();
        assert_eq!(policy.renew_interval_ms(), 20_000, "R1");
        assert_eq!(policy.lease_ttl_ms(), 90_000, "R1");
        assert_eq!(
            LeaseTimingPolicy::new(0, 90_000, 60_000, 600_000),
            Err(LeaseTimingError::Zero),
            "R2"
        );
        assert_eq!(
            LeaseTimingPolicy::new(31_000, 90_000, 60_000, 600_000),
            Err(LeaseTimingError::InsufficientRenewalWindow),
            "R3"
        );
        assert_eq!(
            LeaseTimingPolicy::new(20_000, 90_000, 60_000, 149_999),
            Err(LeaseTimingError::InsufficientRecoveryGrace),
            "R4"
        );
        assert!(
            LeaseTimingPolicy::new(30_000, 90_000, 60_000, 150_000).is_ok(),
            "boundary"
        );
    }

    // Referenced-set decision table (the sole disposal authority):
    // A1 live∩referenced => adopt; A2 live∖referenced => reap;
    // A3 referenced∖live => orphan. Sets are mutually classified by identity.
    #[test]
    fn reconcile_adopts_referenced_and_reaps_the_rest() {
        let live = vec![h("a"), h("b"), h("c")];
        let referenced = vec![h("b")];
        let plan = reconcile_adoption(&live, &referenced);
        assert_eq!(plan.adopt, vec![h("b")]);
        assert_eq!(plan.reap, vec![h("a"), h("c")]);
        assert!(plan.orphan.is_empty());
    }

    #[test]
    fn reconcile_flags_referenced_but_dead_as_orphan() {
        let live = vec![h("a")];
        let referenced = vec![h("a"), h("gone")];
        let plan = reconcile_adoption(&live, &referenced);
        assert_eq!(plan.adopt, vec![h("a")]);
        assert!(plan.reap.is_empty());
        assert_eq!(plan.orphan, vec![h("gone")]);
    }

    #[test]
    fn phase_a_identity_ignores_provider_payload() {
        let with_payload = SandboxHandle::local(
            "a",
            crate::LocalSandboxHandleV1 {
                outputs_path: "/outputs".into(),
                base_env: Vec::new(),
                continuation_excluded_paths: Vec::new(),
                deny_tool_egress: false,
            },
        );
        let plan = reconcile_adoption(&[with_payload], &[SandboxHandle::new("local", "a")]);
        assert_eq!(
            plan.adopt.len(),
            1,
            "same (kind,id) reconciles regardless of provider payload"
        );
    }

    // Restoration reconciliation table: C1 both handles have None; C2 both
    // carry the same complete Some; C3 one has None; C4 both have Some but one
    // exact field differs. Effects: R1/R2 adopt, while R3/R4 reap the live
    // identity and orphan the durable reference. Provider payload remains
    // excluded in R1; exact restoration evidence is included in R2-R4.
    #[test]
    fn phase_b_identity_requires_exact_restoration_evidence() {
        let exact = restored_h("restored", "effect-a");
        let same = exact.clone();
        let mismatch = restored_h("restored", "effect-b");
        let ordinary = h("restored");

        assert_eq!(
            reconcile_adoption(std::slice::from_ref(&exact), &[same])
                .adopt
                .len(),
            1,
            "R2"
        );
        for (rule, referenced) in [("R3", ordinary), ("R4", mismatch)] {
            let plan = reconcile_adoption(
                std::slice::from_ref(&exact),
                std::slice::from_ref(&referenced),
            );
            assert_eq!(plan.reap, vec![exact.clone()], "{rule}");
            assert_eq!(plan.orphan, vec![referenced], "{rule}");
            assert!(plan.adopt.is_empty(), "{rule}");
        }
    }

    #[test]
    fn empty_inputs_yield_empty_plan() {
        assert_eq!(reconcile_adoption(&[], &[]), AdoptionPlan::default());
    }

    // Cross-decision rule C1: lease expired + sandbox live + durable reference
    // present => Fence(Expired) AND adopt, never reap. Cause: owner liveness and
    // durable reachability disagree. Effects: effects are fenced, mutable state
    // survives for recovery, and only a later reference removal can authorize
    // disposal. This locks the lease/reclamation boundary rather than relying on
    // enum shape alone.
    #[test]
    fn expired_lease_does_not_make_a_referenced_sandbox_reapable() {
        let sandbox = h("still-referenced");
        let action = decide_lease_action(&LeaseGrant::until(1_000), signals(1_000, false, false));
        let plan = reconcile_adoption(
            std::slice::from_ref(&sandbox),
            std::slice::from_ref(&sandbox),
        );

        assert_eq!(action, LeaseAction::Fence(LeaseFenceCause::Expired), "C1");
        assert_eq!(plan.adopt, vec![sandbox], "C1");
        assert!(
            plan.reap.is_empty(),
            "C1: lease loss is not disposal authority"
        );
    }

    fn signals(now_ms: u64, revoked: bool, transport_lost: bool) -> LivenessSignals {
        LivenessSignals {
            now_ms,
            revoked,
            transport_lost,
        }
    }

    // Lease-action decision table:
    // F1 no fault/live => Keep; F2 revoked masks expired+transport;
    // F3 expired masks transport; F4 transport-only => Fence(TransportLost);
    // F5 indefinite/no fault => Keep. Effects never include disposal.
    #[test]
    fn a_live_lease_with_no_faults_keeps_the_owner() {
        let g = LeaseGrant::until(1_000);
        assert_eq!(
            decide_lease_action(&g, signals(500, false, false)),
            LeaseAction::Keep
        );
    }

    #[test]
    fn revoke_beats_deadline_and_transport_loss() {
        let g = LeaseGrant::until(1_000);
        // Even when also expired AND transport-lost, revoke wins.
        assert_eq!(
            decide_lease_action(&g, signals(2_000, true, true)),
            LeaseAction::Fence(LeaseFenceCause::Revoked)
        );
        // And even while comfortably within the deadline.
        assert_eq!(
            decide_lease_action(&g, signals(100, true, false)),
            LeaseAction::Fence(LeaseFenceCause::Revoked)
        );
    }

    #[test]
    fn deadline_beats_transport_loss() {
        let g = LeaseGrant::until(1_000);
        // Past deadline + transport lost, not revoked → Expired (deadline wins).
        assert_eq!(
            decide_lease_action(&g, signals(1_500, false, true)),
            LeaseAction::Fence(LeaseFenceCause::Expired)
        );
    }

    #[test]
    fn transport_loss_fences_a_within_deadline_lease() {
        let g = LeaseGrant::until(1_000);
        // Within the deadline, not revoked, but the transport is gone → hung-but-alive.
        assert_eq!(
            decide_lease_action(&g, signals(500, false, true)),
            LeaseAction::Fence(LeaseFenceCause::TransportLost)
        );
    }

    #[test]
    fn an_indefinite_lease_is_only_fenced_on_revoke_or_transport_loss() {
        let g = LeaseGrant::indefinite();
        assert_eq!(
            decide_lease_action(&g, signals(u64::MAX, false, false)),
            LeaseAction::Keep
        );
        assert_eq!(
            decide_lease_action(&g, signals(u64::MAX, false, true)),
            LeaseAction::Fence(LeaseFenceCause::TransportLost)
        );
    }
}

#[cfg(test)]
mod actuator_tests {
    //! The node-agent restart reconcile actuation over a fake provider that counts
    //! adopt/dispose and can be told to fail either.
    use super::*;
    use crate::sandbox::{
        IsolationClass, ProcessHandle, Sandbox, SandboxCapabilities, SandboxError, SandboxStatus,
    };
    use crate::spec::{Command, SandboxSpec};
    use crate::vocab::{Artifact, MountRequirement, RealizedMount};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FakeSandbox {
        id: String,
        dispose_fails: bool,
        disposes: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Sandbox for FakeSandbox {
        fn id(&self) -> &str {
            &self.id
        }
        fn handle(&self) -> SandboxHandle {
            SandboxHandle::new("fake", &self.id)
        }
        async fn spawn(&self, _c: Command) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Err(SandboxError::new("unused"))
        }
        async fn attach(&self, _r: MountRequirement) -> Result<RealizedMount, SandboxError> {
            Err(SandboxError::new("unused"))
        }
        async fn artifacts(&self) -> Result<Vec<Artifact>, SandboxError> {
            Ok(Vec::new())
        }
        async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, SandboxError> {
            Ok(Vec::new())
        }
        fn realized(&self) -> &[RealizedMount] {
            &[]
        }
        async fn process(&self, _p: &str) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Err(SandboxError::new("unused"))
        }
        async fn status(&self) -> Result<SandboxStatus, SandboxError> {
            Ok(SandboxStatus::Ready)
        }
        async fn renew_lease(&self) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn dispose(&self) -> Result<(), SandboxError> {
            self.disposes.fetch_add(1, Ordering::SeqCst);
            if self.dispose_fails {
                Err(SandboxError::new("dispose failed"))
            } else {
                Ok(())
            }
        }
    }

    struct FakeProvider {
        adopts: Arc<AtomicU32>,
        disposes: Arc<AtomicU32>,
        fail_adopt_ids: Vec<String>,
        dispose_fails: bool,
    }

    #[async_trait]
    impl SandboxProvider for FakeProvider {
        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities {
                isolation: IsolationClass::Container,
                tool_transparent: true,
                path_fidelity: true,
                enforced_readonly: true,
                network_isolation: true,
                enforced_network_allowlist: false,
                secret_egress_substitution: false,
                resource_limits: true,
                custom_rootfs: true,
                package_provisioning: false,
                control_services: Default::default(),
            }
        }
        async fn create(&self, _s: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
            Err(SandboxError::new("unused"))
        }
        async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
            self.adopts.fetch_add(1, Ordering::SeqCst);
            if self.fail_adopt_ids.contains(&handle.sandbox_id) {
                return Err(SandboxError::new("adopt failed"));
            }
            Ok(Box::new(FakeSandbox {
                id: handle.sandbox_id.clone(),
                dispose_fails: self.dispose_fails,
                disposes: self.disposes.clone(),
            }))
        }
    }

    fn h(id: &str) -> SandboxHandle {
        SandboxHandle::new("k8s", id)
    }

    // Actuation rules derived from A1-A3:
    // X1 adopt target succeeds => adopted; X2 unreferenced adopt+dispose succeeds
    // => reaped; X3 referenced but absent => orphaned; X4 any provider failure
    // => failed and independent targets continue. Only X2 invokes dispose.
    #[tokio::test]
    async fn reconcile_and_apply_adopts_reaps_and_reports_orphans() {
        let adopts = Arc::new(AtomicU32::new(0));
        let disposes = Arc::new(AtomicU32::new(0));
        let provider = FakeProvider {
            adopts: adopts.clone(),
            disposes: disposes.clone(),
            fail_adopt_ids: Vec::new(),
            dispose_fails: false,
        };
        let live = vec![h("keep"), h("reap")];
        let referenced = vec![h("keep"), h("gone")];

        let out = reconcile_and_apply(&provider, &live, &referenced).await;
        assert_eq!(out.adopted, vec![h("keep")]);
        assert_eq!(out.reaped, vec![h("reap")]);
        assert_eq!(out.orphaned, vec![h("gone")]);
        assert!(out.failed.is_empty());
        // Only the unreferenced sandbox is disposed; adopt runs for both the adopt set
        // and the reap set (reconnect-to-reap).
        assert_eq!(disposes.load(Ordering::SeqCst), 1);
        assert_eq!(adopts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn actuation_errors_are_recorded_not_fatal() {
        let provider = FakeProvider {
            adopts: Arc::new(AtomicU32::new(0)),
            disposes: Arc::new(AtomicU32::new(0)),
            fail_adopt_ids: vec!["keep".into()],
            dispose_fails: true,
        };
        let plan = AdoptionPlan {
            adopt: vec![h("keep")],
            reap: vec![h("reap")],
            orphan: Vec::new(),
        };
        let out = apply_adoption_plan(&provider, &plan).await;
        // `keep` fails to adopt; `reap` adopts then fails to dispose — both land in
        // `failed`, and neither aborts the other.
        assert!(out.adopted.is_empty());
        assert!(out.reaped.is_empty());
        assert_eq!(out.failed.len(), 2);
    }

    // Restored-handle actuator decision table:
    // C1 restoration Some in adopt; C2 restoration Some in reap; C3 ordinary
    // restoration None follows either. R1 C1/C2 => failed with zero provider
    // adopt/dispose effects because this generic port lacks the frozen spec and
    // restore request. R2 C3 => ordinary work still succeeds after either
    // fail-closed classification, preserving per-handle best effort.
    #[tokio::test]
    async fn restored_handles_fail_closed_without_blocking_ordinary_work() {
        let restored_adopt = super::tests::restored_h("restored-adopt", "effect-adopt");
        let restored_reap = super::tests::restored_h("restored-reap", "effect-reap");
        let exact_adopts = Arc::new(AtomicU32::new(0));
        let exact_disposes = Arc::new(AtomicU32::new(0));
        let exact_provider = FakeProvider {
            adopts: exact_adopts.clone(),
            disposes: exact_disposes.clone(),
            fail_adopt_ids: Vec::new(),
            dispose_fails: false,
        };

        let exact = apply_adoption_plan(
            &exact_provider,
            &AdoptionPlan {
                adopt: vec![restored_adopt.clone()],
                reap: vec![restored_reap.clone()],
                orphan: Vec::new(),
            },
        )
        .await;
        assert_eq!(
            exact.failed,
            vec![restored_adopt.clone(), restored_reap.clone()]
        );
        assert!(exact.adopted.is_empty());
        assert!(exact.reaped.is_empty());
        assert_eq!(exact_adopts.load(Ordering::SeqCst), 0, "R1 adopt");
        assert_eq!(exact_disposes.load(Ordering::SeqCst), 0, "R1 dispose");

        let ordinary = h("ordinary");
        let mixed_adopts = Arc::new(AtomicU32::new(0));
        let mixed_disposes = Arc::new(AtomicU32::new(0));
        let mixed_provider = FakeProvider {
            adopts: mixed_adopts.clone(),
            disposes: mixed_disposes.clone(),
            fail_adopt_ids: Vec::new(),
            dispose_fails: false,
        };
        let mixed = apply_adoption_plan(
            &mixed_provider,
            &AdoptionPlan {
                adopt: vec![restored_adopt.clone(), ordinary.clone()],
                reap: vec![restored_reap.clone()],
                orphan: Vec::new(),
            },
        )
        .await;
        assert_eq!(mixed.adopted, vec![ordinary], "R2");
        assert_eq!(mixed.failed, vec![restored_adopt, restored_reap], "R2");
        assert_eq!(mixed_adopts.load(Ordering::SeqCst), 1, "ordinary only");
        assert_eq!(
            mixed_disposes.load(Ordering::SeqCst),
            0,
            "restored reap skipped"
        );
    }

    #[tokio::test]
    async fn fixtures_conform_to_the_sandbox_ports() {
        // The actuator only drives adopt/dispose; assert the fake is otherwise a
        // well-formed `Sandbox`/`SandboxProvider` so what it *does* drive is sound.
        let provider = FakeProvider {
            adopts: Arc::new(AtomicU32::new(0)),
            disposes: Arc::new(AtomicU32::new(0)),
            fail_adopt_ids: Vec::new(),
            dispose_fails: false,
        };
        assert_eq!(provider.capabilities().isolation, IsolationClass::Container);
        assert!(provider.create(&spec_min()).await.is_err());

        let sb = provider.adopt(&h("s")).await.unwrap();
        assert_eq!(sb.id(), "s");
        assert_eq!(sb.handle().sandbox_id, "s");
        assert!(matches!(sb.status().await.unwrap(), SandboxStatus::Ready));
        assert!(sb.artifacts().await.unwrap().is_empty());
        assert!(sb.read_artifact("a").await.unwrap().is_empty());
        assert!(sb.realized().is_empty());
        sb.renew_lease().await.unwrap();
        assert!(sb.spawn(Command::new(["x"])).await.is_err());
        assert!(sb.attach(a_mount()).await.is_err());
        assert!(sb.process("p").await.is_err());
    }

    fn spec_min() -> SandboxSpec {
        SandboxSpec {
            scope: "s".into(),
            isolation: IsolationClass::Container,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: crate::vocab::NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: crate::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            control_services: Default::default(),
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
        }
    }

    fn a_mount() -> MountRequirement {
        MountRequirement {
            mount_id: "m".into(),
            source: crate::vocab::MountSource::File {
                file_id: "f".into(),
                content_hash: None,
            },
            mount_path: "/workspace/x".into(),
            access: crate::vocab::MountAccess::ReadOnly,
            lifetime: crate::vocab::MountLifetime::PerRun,
            required: false,
        }
    }
}
