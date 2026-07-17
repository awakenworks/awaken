//! Formal (property-based) verification of the in-memory `WorkQueue` — the open-tier
//! single-worker dispatch semantics (ADR-0059 verification pass). The safety invariant is
//! **at most one item is leased-active per environment at a time**: it is what makes a
//! self-hosted environment hand a run to exactly one worker. These properties assert it
//! over ALL enqueue/claim counts the generator produces, not just the pinned unit case.

use awaken_session_contract::work_queue::{WorkQueue, WorkState};
use awaken_work_store::{InMemoryWorkQueue, LEASE_TTL_MS};
use proptest::prelude::*;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

proptest! {
    /// SINGLE ACTIVE CAP: with `n` items queued, repeated claims at the SAME instant hand
    /// out exactly ONE lease — the rest are capped (None) while that one is live. This is
    /// the exactly-once guarantee for a single-worker environment.
    #[test]
    fn at_most_one_lease_is_handed_out_while_one_is_live(n in 1usize..12, claims in 2usize..8) {
        let q = InMemoryWorkQueue::new();
        for i in 0..n {
            block(q.enqueue_session("env", &format!("s{i}")));
        }
        let handed: usize = (0..claims)
            .filter(|_| block(q.claim("env", "w", 0)).is_some())
            .count();
        prop_assert_eq!(handed, 1, "more than one lease was live at once");
        // Exactly one item is Active; the rest stay Queued.
        let active = block(q.list("env")).into_iter().filter(|w| w.state == WorkState::Active).count();
        prop_assert_eq!(active, 1);
    }

    /// RECLAIM AFTER EXPIRY: once the live lease lapses (now >= expiry), the next claim
    /// succeeds again — a dead worker never wedges the environment forever.
    #[test]
    fn an_expired_lease_is_reclaimable(n in 1usize..8) {
        let q = InMemoryWorkQueue::new();
        for i in 0..n {
            block(q.enqueue_session("env", &format!("s{i}")));
        }
        prop_assert!(block(q.claim("env", "a", 0)).is_some(), "first claim leases");
        prop_assert!(block(q.claim("env", "b", 1)).is_none(), "a live lease caps the env");
        // At exactly the TTL boundary the lapsed lease is reclaimable.
        prop_assert!(block(q.claim("env", "b", LEASE_TTL_MS)).is_some(), "expired lease not reclaimed");
    }

    /// ENVIRONMENT ISOLATION: a claim in one environment never leases another's work; each
    /// env's single-active cap is independent.
    #[test]
    fn environments_lease_independently(a in 1usize..5, b in 1usize..5) {
        let q = InMemoryWorkQueue::new();
        for i in 0..a { block(q.enqueue_session("env_a", &format!("a{i}"))); }
        for i in 0..b { block(q.enqueue_session("env_b", &format!("b{i}"))); }
        let la = block(q.claim("env_a", "w", 0)).expect("env_a leases");
        let lb = block(q.claim("env_b", "w", 0)).expect("env_b leases independently");
        prop_assert_eq!(block(q.get("env_a", &la.id)).map(|w| w.environment_id), Some("env_a".to_string()));
        prop_assert_eq!(block(q.get("env_b", &lb.id)).map(|w| w.environment_id), Some("env_b".to_string()));
    }

    /// STOP FREES THE LEASE: stopping the active item lets the next queued item be claimed
    /// (progress), and the total item count is conserved across the claim→stop→claim cycle.
    #[test]
    fn stopping_the_active_item_frees_the_next(n in 2usize..8) {
        let q = InMemoryWorkQueue::new();
        for i in 0..n { block(q.enqueue_session("env", &format!("s{i}"))); }
        let first = block(q.claim("env", "w", 0)).expect("first");
        prop_assert!(block(q.claim("env", "w", 0)).is_none(), "capped while active");
        block(q.stop("env", &first.id));
        prop_assert!(block(q.claim("env", "w", 0)).is_some(), "next claimable after stop");
        // No item was lost or duplicated.
        prop_assert_eq!(block(q.list("env")).len(), n);
    }

    /// REMOVE_ENV PURGES: after remove_env, the environment has no items and nothing to claim.
    #[test]
    fn remove_env_purges_everything(n in 1usize..8) {
        let q = InMemoryWorkQueue::new();
        for i in 0..n { block(q.enqueue_session("env", &format!("s{i}"))); }
        block(q.remove_env("env"));
        prop_assert!(block(q.list("env")).is_empty());
        prop_assert!(block(q.claim("env", "w", 0)).is_none());
    }
}
