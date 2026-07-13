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
    /// Wall-clock ms after which the lease is reapable; `None` = never expires.
    pub expires_ms: Option<u64>,
}

impl LeaseGrant {
    /// An indefinite lease (never reaped on expiry).
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
    /// live run is never reaped mid-flight.
    #[must_use]
    pub fn liveness(&self, now_ms: u64, renew_margin_ms: u64) -> LeaseLiveness {
        match self.expires_ms {
            None => LeaseLiveness::Live,
            Some(exp) if now_ms >= exp => LeaseLiveness::Reapable,
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
    /// Past its deadline — reclaimable; reap unless still referenced.
    Reapable,
}

/// Why a sandbox is being reaped — recorded for observability and idempotent reap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapCause {
    /// The lease was explicitly revoked (admin, or a superseding placement).
    Revoked,
    /// The lease deadline passed without renewal (owner vanished).
    Expired,
    /// The owner's transport to the sandbox was lost (channel closed) while the lease
    /// was still within its deadline — a "hung but alive" reclaim.
    TransportLost,
    /// A newer sandbox superseded this one for the same binding.
    Superseded,
    /// The owning run settled and released it.
    Released,
}

/// Point-in-time liveness signals for a leased sandbox, collapsed into a single reap
/// decision by [`decide_reap`]. The caller supplies each from its own source (a
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

/// Cap a credential's expiry at the lease deadline: `min(now + default_ttl, lease
/// deadline)` (oversight ADR-0023 — an injected secret must never outlive its lease).
/// Pure and neutral: a credential minter calls this to bound a short-lived token's
/// `exp` so a revoked/expired lease can never leave a live credential behind. An
/// indefinite lease returns just `now + default_ttl_ms` (the token's own TTL still
/// applies).
#[must_use]
pub fn capped_expiry(default_ttl_ms: u64, now_ms: u64, grant: &LeaseGrant) -> u64 {
    let token_exp = now_ms.saturating_add(default_ttl_ms);
    match grant.expires_ms {
        Some(lease_exp) => token_exp.min(lease_exp),
        None => token_exp,
    }
}

/// Whether a side-effecting outbound (egress) call is still permitted for a leased
/// worker — the per-call fence (oversight ADR-0016): a **revoked** or **past-deadline**
/// lease denies egress, so a fenced worker's external effects are rejected just like
/// its callbacks. Pure; the egress chokepoint re-checks this on every call (it does
/// not renew the lease).
#[must_use]
pub fn egress_permitted(grant: &LeaseGrant, now_ms: u64, revoked: bool) -> bool {
    !revoked && !matches!(grant.liveness(now_ms, 0), LeaseLiveness::Reapable)
}

/// Decide whether to reap a leased sandbox, collapsing the signals with the fixed
/// priority **Revoked > Expired(deadline) > TransportLost** (awaken-next parity): a
/// revoke always wins; else a passed deadline; else a lost transport; else keep it
/// alive. Pure — heartbeat is not a signal here (it renews the deadline upstream), so
/// a within-deadline sandbox is only reaped on revoke or transport loss.
#[must_use]
pub fn decide_reap(grant: &LeaseGrant, signals: LivenessSignals) -> Option<ReapCause> {
    if signals.revoked {
        return Some(ReapCause::Revoked);
    }
    if matches!(grant.liveness(signals.now_ms, 0), LeaseLiveness::Reapable) {
        return Some(ReapCause::Expired);
    }
    if signals.transport_lost {
        return Some(ReapCause::TransportLost);
    }
    None
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

/// The identity of a sandbox for reconciliation: `(provider_kind, sandbox_id)`.
/// `extra` carries provider locators, not identity, so it is excluded.
fn key(h: &SandboxHandle) -> (&str, &str) {
    (h.provider_kind.as_str(), h.sandbox_id.as_str())
}

/// Reconcile live sandboxes against those still referenced by a live run.
///
/// - live ∩ referenced → **adopt** (reconnect; a run still needs it)
/// - live ∖ referenced → **reap** (nothing needs it)
/// - referenced ∖ live → **orphan** (the run's sandbox died; re-place it)
///
/// Pure and neutral: identity is `(provider_kind, sandbox_id)`; `referenced` is
/// supplied by the caller from live `Claimed` bindings.
#[must_use]
pub fn reconcile_adoption(live: &[SandboxHandle], referenced: &[SandboxHandle]) -> AdoptionPlan {
    let referenced_keys: Vec<(&str, &str)> = referenced.iter().map(key).collect();
    let live_keys: Vec<(&str, &str)> = live.iter().map(key).collect();

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
pub async fn apply_adoption_plan(
    provider: &dyn SandboxProvider,
    plan: &AdoptionPlan,
) -> ReconcileOutcome {
    let mut out = ReconcileOutcome {
        orphaned: plan.orphan.clone(),
        ..Default::default()
    };
    for h in &plan.adopt {
        match provider.adopt(h).await {
            Ok(_reconnected) => out.adopted.push(h.clone()),
            Err(_) => out.failed.push(h.clone()),
        }
    }
    for h in &plan.reap {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn h(id: &str) -> SandboxHandle {
        SandboxHandle::new("k8s", id)
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
        assert_eq!(g.liveness(1_000, 100), LeaseLiveness::Reapable);
        assert_eq!(g.liveness(1_200, 100), LeaseLiveness::Reapable);
    }

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
    fn identity_ignores_extra_locators() {
        let mut with_extra = SandboxHandle::new("k8s", "a");
        with_extra.extra = Some(serde_json::json!({"node": "n1"}));
        let plan = reconcile_adoption(&[with_extra], &[h("a")]);
        assert_eq!(
            plan.adopt.len(),
            1,
            "same (kind,id) reconciles regardless of extra"
        );
    }

    #[test]
    fn empty_inputs_yield_empty_plan() {
        assert_eq!(reconcile_adoption(&[], &[]), AdoptionPlan::default());
    }

    fn signals(now_ms: u64, revoked: bool, transport_lost: bool) -> LivenessSignals {
        LivenessSignals {
            now_ms,
            revoked,
            transport_lost,
        }
    }

    #[test]
    fn a_live_lease_with_no_faults_is_not_reaped() {
        let g = LeaseGrant::until(1_000);
        assert_eq!(decide_reap(&g, signals(500, false, false)), None);
    }

    #[test]
    fn revoke_beats_deadline_and_transport_loss() {
        let g = LeaseGrant::until(1_000);
        // Even when also expired AND transport-lost, revoke wins.
        assert_eq!(
            decide_reap(&g, signals(2_000, true, true)),
            Some(ReapCause::Revoked)
        );
        // And even while comfortably within the deadline.
        assert_eq!(
            decide_reap(&g, signals(100, true, false)),
            Some(ReapCause::Revoked)
        );
    }

    #[test]
    fn deadline_beats_transport_loss() {
        let g = LeaseGrant::until(1_000);
        // Past deadline + transport lost, not revoked → Expired (deadline wins).
        assert_eq!(
            decide_reap(&g, signals(1_500, false, true)),
            Some(ReapCause::Expired)
        );
    }

    #[test]
    fn transport_loss_reaps_a_within_deadline_lease() {
        let g = LeaseGrant::until(1_000);
        // Within the deadline, not revoked, but the transport is gone → hung-but-alive.
        assert_eq!(
            decide_reap(&g, signals(500, false, true)),
            Some(ReapCause::TransportLost)
        );
    }

    #[test]
    fn an_indefinite_lease_is_only_reaped_on_revoke_or_transport_loss() {
        let g = LeaseGrant::indefinite();
        assert_eq!(decide_reap(&g, signals(u64::MAX, false, false)), None);
        assert_eq!(
            decide_reap(&g, signals(u64::MAX, false, true)),
            Some(ReapCause::TransportLost)
        );
    }

    #[test]
    fn capped_expiry_never_outlives_the_lease() {
        // Lease ends at 1_000; a 500ms token from now=800 would reach 1_300 → capped.
        let g = LeaseGrant::until(1_000);
        assert_eq!(capped_expiry(500, 800, &g), 1_000);
        // A token that ends before the lease keeps its own (shorter) TTL.
        assert_eq!(capped_expiry(100, 800, &g), 900);
        // An indefinite lease → just the token's TTL.
        assert_eq!(capped_expiry(500, 800, &LeaseGrant::indefinite()), 1_300);
    }

    #[test]
    fn egress_is_denied_once_the_lease_is_revoked_or_expired() {
        let g = LeaseGrant::until(1_000);
        assert!(egress_permitted(&g, 500, false)); // live
        assert!(!egress_permitted(&g, 500, true)); // revoked mid-flight
        assert!(!egress_permitted(&g, 1_500, false)); // past the deadline
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
                secret_egress_substitution: false,
                resource_limits: true,
                custom_rootfs: true,
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
            network: crate::vocab::NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            limits: Default::default(),
            lease_ttl_secs: None,
            extra: None,
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
