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
    /// The lease deadline passed without renewal (owner vanished).
    Expired,
    /// A newer sandbox superseded this one for the same binding.
    Superseded,
    /// The owning run settled and released it.
    Released,
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
}
