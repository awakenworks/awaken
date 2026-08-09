//! Property verification of `LeaseBook`: the in-memory backend's lease/reclaim
//! kernel and every backend's process-local poll-liveness bookkeeping. Durable
//! SQLite/PostgreSQL lease safety is covered by backend conformance, restart and
//! contention tests because its authority lives in database rows.
//!
//! The cause-effect unit test pins the TTL boundary at one instant; these properties
//! assert the lease/poll predicates hold for ALL `(t0, now)` pairs the generator
//! produces — the universal statement of "a lease is live iff now is before its expiry"
//! and "a worker counts as polling iff now is within the window", including the exact
//! `>`-vs-`>=` boundary a hand-written case can only sample.

use awaken_work_store::{LEASE_TTL_MS, LeaseBook, POLLER_WINDOW_MS};
use proptest::prelude::*;

// Bound the clock so `t0 + TTL` never overflows u64; realistic ms timestamps fit easily.
const MAX_T: u64 = 1u64 << 50;

proptest! {
    /// LEASE LIVENESS: after `lease(wid, t0)`, the item is leased at `now` iff `now` is
    /// strictly before the expiry `t0 + LEASE_TTL_MS`. This is the exact reclaim boundary
    /// — a lapsed worker's item becomes reclaimable at `now == expiry`, not one ms later.
    #[test]
    fn a_lease_is_live_exactly_until_its_expiry(
        t0 in 0u64..MAX_T,
        delta in 0u64..(3 * LEASE_TTL_MS),
    ) {
        let book = LeaseBook::default();
        book.lease("w", t0);
        let now = t0.saturating_add(delta);
        let expiry = t0 + LEASE_TTL_MS;
        prop_assert_eq!(book.is_leased("w", now), now < expiry,
            "is_leased disagreed with `now < expiry` at now={}, expiry={}", now, expiry);
    }

    /// RELEASE INVALIDATES: once released, an item is not leased at ANY instant.
    #[test]
    fn a_released_lease_is_never_live(t0 in 0u64..MAX_T, now in 0u64..MAX_T) {
        let book = LeaseBook::default();
        book.lease("w", t0);
        book.release("w");
        prop_assert!(!book.is_leased("w", now));
    }

    /// RE-LEASE EXTENDS: re-leasing at a later `t1` moves the expiry to `t1 + TTL`, so an
    /// instant that would have expired under the first lease is live again (a heartbeat).
    #[test]
    fn re_leasing_extends_to_the_new_expiry(
        t0 in 0u64..MAX_T,
        gap in 1u64..LEASE_TTL_MS,
    ) {
        let book = LeaseBook::default();
        book.lease("w", t0);
        let t1 = t0 + gap;             // a heartbeat before the first lease lapses
        book.lease("w", t1);
        // Just before the SECOND expiry it is still live, even past the FIRST expiry.
        let past_first = t0 + LEASE_TTL_MS;      // first expiry instant
        prop_assert!(book.is_leased("w", past_first), "heartbeat did not extend past the first expiry");
        prop_assert!(!book.is_leased("w", t1 + LEASE_TTL_MS), "live past the second expiry");
    }

    /// AN UNKNOWN ITEM IS NEVER LEASED (no phantom lease blocks a claim).
    #[test]
    fn an_unknown_item_is_never_leased(now in 0u64..MAX_T) {
        let book = LeaseBook::default();
        prop_assert!(!book.is_leased("never-leased", now));
    }

    /// POLL WINDOW: a worker that polled at `t` counts as polling at `now` iff `now` is
    /// within `POLLER_WINDOW_MS` of it (`t + WINDOW > now`); distinct workers count once
    /// each, a re-poll by the same worker does not double-count.
    #[test]
    fn a_worker_counts_as_polling_only_within_the_window(
        t in 0u64..MAX_T,
        delta in 0u64..(3 * POLLER_WINDOW_MS),
    ) {
        let book = LeaseBook::default();
        book.record_poll("env", "w1", t);
        let now = t.saturating_add(delta);
        let within = t + POLLER_WINDOW_MS > now;
        prop_assert_eq!(book.workers_polling("env", now), i64::from(within));
        // A second poll by the SAME worker within the window is still one worker.
        book.record_poll("env", "w1", now);
        prop_assert!(book.workers_polling("env", now) <= 1, "same worker double-counted");
    }

    /// DISTINCT WORKERS ACCUMULATE: n distinct workers all polling at `now` count as n.
    #[test]
    fn distinct_workers_within_the_window_all_count(n in 1u64..8) {
        let book = LeaseBook::default();
        for i in 0..n {
            book.record_poll("env", &format!("w{i}"), 1_000);
        }
        prop_assert_eq!(book.workers_polling("env", 1_000), n as i64);
    }
}

#[test]
fn poll_window_is_exact_and_overflow_safe() {
    // Cause/effect graph: C1 observation is before/equal/after the poll; C2 elapsed
    // time is below/equal to the window; C3 the timestamp is near u64::MAX.
    // Effects: E1 worker counts as live; E2 worker expires; E3 no arithmetic panic.
    // Constraint: elapsed time is meaningful only when observation >= poll.
    //
    // | Rule | observation       | elapsed       | near MAX | result |
    // | T1   | before poll       | n/a           | no       | live   |
    // | T2   | after/equal       | WINDOW - 1    | no       | live   |
    // | T3   | after/equal       | WINDOW        | no       | expired|
    // | T4   | after/equal       | 1             | yes      | live   |
    let book = LeaseBook::default();
    book.record_poll("clock", "worker", 100);
    assert_eq!(book.workers_polling("clock", 99), 1, "T1/E1");
    assert_eq!(
        book.workers_polling("clock", 100 + POLLER_WINDOW_MS - 1),
        1,
        "T2/E1"
    );
    assert_eq!(
        book.workers_polling("clock", 100 + POLLER_WINDOW_MS),
        0,
        "T3/E2"
    );

    let near_limit = LeaseBook::default();
    near_limit.record_poll("clock", "worker", u64::MAX - 1);
    assert_eq!(near_limit.workers_polling("clock", u64::MAX), 1, "T4/E1/E3");
}
