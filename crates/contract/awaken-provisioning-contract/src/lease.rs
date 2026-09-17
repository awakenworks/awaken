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

use crate::sandbox::SandboxHandle;
#[cfg(test)]
use crate::sandbox::SandboxProvider;

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
        let with_payload = crate::test_support::fixture_legacy_handle(
            "a",
            &crate::test_support::FixtureHandleLocator {
                outputs_path: "/outputs".into(),
                physical_incarnation: "physical-a".into(),
            },
        );
        let plan = reconcile_adoption(
            &[with_payload],
            &[SandboxHandle::new(
                crate::test_support::FIXTURE_HANDLE_PROVIDER,
                "a",
            )],
        );
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
        async fn dispose_unattached(&self) -> Result<(), SandboxError> {
            self.dispose().await
        }

        fn id(&self) -> &str {
            &self.id
        }
        fn handle(&self) -> SandboxHandle {
            SandboxHandle::new("fake", &self.id)
        }
        async fn spawn(&self, _c: Command) -> Result<Box<dyn ProcessHandle>, SandboxError> {
            Err(SandboxError::unsupported("unused"))
        }
        async fn attach(&self, _r: MountRequirement) -> Result<RealizedMount, SandboxError> {
            Err(SandboxError::unsupported("unused"))
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
            Err(SandboxError::unsupported("unused"))
        }
        async fn status(&self) -> Result<SandboxStatus, SandboxError> {
            Ok(SandboxStatus::Ready)
        }
        async fn renew_lease(&self) -> Result<(), SandboxError> {
            Ok(())
        }

        async fn dispose_for_effect(
            &self,
            authorization: &crate::SandboxDisposalAuthorization,
            authority: &dyn crate::CurrentExecutionAuthority,
        ) -> Result<(), SandboxError> {
            crate::with_current_sandbox_effect(authorization.effect_fence(), authority, || Ok(()))?;
            self.dispose().await
        }
    }

    impl FakeSandbox {
        // Relocated from the Sandbox/SandboxProvider trait: the unfenced ports
        // are no longer a trait obligation or a reachable escape hatch. They stay
        // as this type's own implementation detail, used by its fenced edges.
        async fn dispose(&self) -> Result<(), SandboxError> {
            self.disposes.fetch_add(1, Ordering::SeqCst);
            if self.dispose_fails {
                Err(SandboxError::backend("dispose", "dispose failed"))
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
                process_tree_supervision: false,
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

        fn claim_for_effect<'a>(
            &'a self,
            spec: &'a SandboxSpec,
            effect_fence: &'a crate::SandboxEffectFence,
            authority: &'a dyn crate::CurrentExecutionAuthority,
        ) -> crate::ClaimedEffect<'a, Box<dyn Sandbox>> {
            let claim = crate::EffectClaim::in_process(
                crate::test_support::retain_abandoned_effect(spec.scope.clone()),
            );
            crate::ClaimedEffect::new(claim, async move {
                crate::with_current_sandbox_effect(effect_fence, authority, || Ok(()))?;
                self.create(spec).await
            })
        }
        async fn adopt_for_effect(
            &self,
            _spec: &SandboxSpec,
            handle: &SandboxHandle,
            effect_fence: &crate::SandboxEffectFence,
            authority: &dyn crate::CurrentExecutionAuthority,
        ) -> Result<Box<dyn Sandbox>, SandboxError> {
            crate::with_current_sandbox_effect(effect_fence, authority, || Ok(()))?;
            self.adopt(handle).await
        }
    }

    impl FakeProvider {
        // Relocated from the Sandbox/SandboxProvider trait: the unfenced ports
        // are no longer a trait obligation or a reachable escape hatch. They stay
        // as this type's own implementation detail, used by its fenced edges.
        async fn create(&self, _s: &SandboxSpec) -> Result<Box<dyn Sandbox>, SandboxError> {
            Err(SandboxError::unsupported("unused"))
        }
        async fn adopt(&self, handle: &SandboxHandle) -> Result<Box<dyn Sandbox>, SandboxError> {
            self.adopts.fetch_add(1, Ordering::SeqCst);
            if self.fail_adopt_ids.contains(&handle.sandbox_id) {
                return Err(SandboxError::backend("adopt", "adopt failed"));
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

    // The generic adoption actuator is gone: it drove `dispose` with no current
    // authority at all. This fixture keeps proving the fake is a well-formed
    // `Sandbox`/`SandboxProvider`, which is what the surviving planner assumes.
    #[tokio::test]
    async fn fixtures_conform_to_the_sandbox_ports() {
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
            process_tree_supervision: false,
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
