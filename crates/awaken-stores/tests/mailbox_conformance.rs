//! Shared semantic conformance tests for [`MailboxStore`] implementations.
//!
//! Each `pub async fn` in this module exercises one semantic behavior of the
//! trait against a `&S: MailboxStore`. Backend-specific test binaries (e.g.
//! `memory_mailbox_conformance.rs`) include this file via `mod
//! mailbox_conformance;` and dispatch each test against a fresh store
//! instance.
//!
//! No `#[test]` / `#[tokio::test]` attributes live here on purpose: when cargo
//! compiles this file as its own integration test binary it simply produces a
//! binary with zero tests. `#![allow(dead_code)]` silences warnings in that
//! standalone compilation mode, since each backend binary only calls a subset
//! of these functions through its macro expansion.

#![allow(dead_code)]

use awaken_server_contract::contract::mailbox::{MailboxStore, RunDispatch, RunDispatchStatus};
use uuid::Uuid;

/// Construct a `RunDispatch` with sensible defaults for conformance tests.
pub fn make_dispatch(
    dispatch_id: &str,
    thread_id: &str,
    run_id: &str,
    priority: u8,
    available_at: u64,
) -> RunDispatch {
    RunDispatch {
        dispatch_id: dispatch_id.to_string(),
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        priority,
        dedupe_key: None,
        dispatch_epoch: 0,
        status: RunDispatchStatus::Queued,
        available_at,
        attempt_count: 0,
        max_attempts: 5,
        last_error: None,
        claim_token: None,
        claimed_by: None,
        lease_until: None,
        dispatch_instance_id: None,
        run_status: None,
        termination: None,
        run_response: None,
        run_error: None,
        completed_at: None,
        created_at: available_at,
        updated_at: available_at,
    }
}

fn assert_claim_fields_clear(dispatch: &RunDispatch) {
    assert!(dispatch.claim_token.is_none());
    assert!(dispatch.claimed_by.is_none());
    assert!(dispatch.lease_until.is_none());
}

/// Enqueue a single dispatch and verify it is returned by `list_dispatches`.
pub async fn enqueue_and_list<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-enqueue", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let listed = store
        .list_dispatches("t-enqueue", None, 100, 0)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].dispatch_id, "d1");
    assert_eq!(listed[0].status, RunDispatchStatus::Queued);
}

/// `claim()` returns a previously queued dispatch with status Claimed and a
/// populated `claim_token`.
pub async fn claim_returns_queued_dispatch<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-claim", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-claim", "consumer-1", 30_000, 1000, 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id, "d1");
    assert_eq!(claimed[0].status, RunDispatchStatus::Claimed);
    assert!(claimed[0].claim_token.is_some());
    assert_eq!(claimed[0].claimed_by.as_deref(), Some("consumer-1"));
}

/// `claim()` must skip dispatches whose `available_at` is in the future.
pub async fn claim_respects_available_at<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-avail", "r1", 128, 5000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-avail", "consumer-1", 30_000, 1000, 10)
        .await
        .unwrap();
    assert!(claimed.is_empty());

    // Advance beyond available_at: dispatch must now be claimable.
    let claimed = store
        .claim("t-avail", "consumer-1", 30_000, 5000, 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id, "d1");
}

/// Lower numeric priority wins before `created_at` ordering.
pub async fn claim_respects_priority_before_created_at<S: MailboxStore>(store: &S) {
    let mut older_low_priority = make_dispatch("d-low", "t-priority", "r-low", 200, 1000);
    older_low_priority.created_at = 10;
    store.enqueue(&older_low_priority).await.unwrap();

    let mut newer_high_priority = make_dispatch("d-high", "t-priority", "r-high", 10, 1000);
    newer_high_priority.created_at = 20;
    store.enqueue(&newer_high_priority).await.unwrap();

    let claimed = store
        .claim("t-priority", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id, "d-high");
    assert_eq!(claimed[0].priority, 10);
}

/// A second claim must not bypass same-thread execution ownership while a
/// dispatch is already active.
pub async fn claim_limit_preserves_thread_exclusivity<S: MailboxStore>(store: &S) {
    let older = make_dispatch("d-one", "t-claim-limit", "r-one", 10, 1000);
    store.enqueue(&older).await.unwrap();

    let mut newer = make_dispatch("d-two", "t-claim-limit", "r-two", 10, 1000);
    newer.created_at = 2000;
    store.enqueue(&newer).await.unwrap();

    let claimed = store
        .claim("t-claim-limit", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id, "d-one");

    let blocked = store
        .claim("t-claim-limit", "consumer-2", 30_000, 1100, 10)
        .await
        .unwrap();
    assert!(
        blocked.is_empty(),
        "same thread must not claim a second dispatch while one is active"
    );

    let token = claimed[0].claim_token.clone().unwrap();
    store.ack("d-one", &token, 1200).await.unwrap();

    let next = store
        .claim("t-claim-limit", "consumer-2", 30_000, 1300, 1)
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].dispatch_id, "d-two");
}

/// `list_dispatches()` must use the same priority/FIFO ordering as `claim()`.
pub async fn list_dispatches_orders_by_priority_then_created_at<S: MailboxStore>(store: &S) {
    let mut low = make_dispatch("d-low", "t-list-order", "r-low", 200, 1000);
    low.created_at = 10;
    store.enqueue(&low).await.unwrap();

    let mut high_newer = make_dispatch("d-high-newer", "t-list-order", "r-high-newer", 10, 1000);
    high_newer.created_at = 30;
    store.enqueue(&high_newer).await.unwrap();

    let mut high_older = make_dispatch("d-high-older", "t-list-order", "r-high-older", 10, 1000);
    high_older.created_at = 20;
    store.enqueue(&high_older).await.unwrap();

    let listed = store
        .list_dispatches("t-list-order", None, 10, 0)
        .await
        .unwrap();
    let ids = listed
        .iter()
        .map(|dispatch| dispatch.dispatch_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["d-high-older", "d-high-newer", "d-low"]);
}

/// `ack()` transitions a Claimed dispatch to Acked.
pub async fn ack_transitions_to_acked<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-ack", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-ack", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();

    store.ack("d1", &token, 2000).await.unwrap();

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Acked);
    assert_claim_fields_clear(&loaded);
}

/// `ack()` with a claim_token that does not match the active lease must fail.
pub async fn ack_rejects_wrong_claim_token<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-ack-wrong", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    store
        .claim("t-ack-wrong", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();

    let result = store.ack("d1", "wrong-token", 2000).await;
    assert!(result.is_err(), "ack with wrong token must error");

    // Dispatch stays Claimed.
    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Claimed);
}

/// `nack()` with attempts remaining returns the dispatch to Queued, bumping
/// `attempt_count` and rescheduling with `available_at == retry_at`.
pub async fn nack_returns_to_queued<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-nack", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-nack", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();

    store
        .nack("d1", &token, 3000, "transient error", 2000)
        .await
        .unwrap();

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Queued);
    assert_eq!(loaded.attempt_count, 1);
    assert_eq!(loaded.available_at, 3000);
    assert!(loaded.claim_token.is_none());
    assert!(loaded.claimed_by.is_none());
    assert!(loaded.lease_until.is_none());
    assert_eq!(loaded.last_error.as_deref(), Some("transient error"));
}

/// `nack()` dead-letters a dispatch once `attempt_count` reaches `max_attempts`.
pub async fn nack_dead_letters_after_max_attempts<S: MailboxStore>(store: &S) {
    let mut dispatch = make_dispatch("d1", "t-dead", "r1", 128, 1000);
    dispatch.max_attempts = 2;
    store.enqueue(&dispatch).await.unwrap();

    // First nack: attempt_count=1, back to Queued.
    let claimed = store
        .claim("t-dead", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();
    store.nack("d1", &token, 1500, "retry", 1100).await.unwrap();
    let after_first = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(after_first.status, RunDispatchStatus::Queued);
    assert_eq!(after_first.attempt_count, 1);

    // Second nack: attempt_count=2 == max_attempts → DeadLetter.
    let claimed = store
        .claim("t-dead", "consumer-1", 30_000, 1500, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();
    store
        .nack("d1", &token, 2500, "final error", 2000)
        .await
        .unwrap();

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::DeadLetter);
    assert_eq!(loaded.attempt_count, 2);
    assert_claim_fields_clear(&loaded);
}

/// `cancel()` on a Queued dispatch transitions it to Cancelled.
pub async fn cancel_queued_dispatch<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-cancel", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let cancelled = store.cancel("d1", 2000).await.unwrap();
    assert!(cancelled.is_some());
    assert_eq!(cancelled.unwrap().status, RunDispatchStatus::Cancelled);

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Cancelled);
    assert_claim_fields_clear(&loaded);
}

/// `cancel()` is only for Queued dispatches; Claimed dispatches remain owned by
/// the runtime cancellation path.
pub async fn cancel_claimed_dispatch_returns_none<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-cancel-claimed", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-cancel-claimed", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    let cancelled = store.cancel("d1", 2000).await.unwrap();
    assert!(cancelled.is_none());

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Claimed);
}

/// `extend_lease()` with the right claim_token updates `lease_until`.
pub async fn extend_lease_success<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-extend", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-extend", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();

    let ok = store
        .extend_lease("d1", &token, 60_000, 15_000)
        .await
        .unwrap();
    assert!(ok);

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.lease_until, Some(75_000));
}

/// `extend_lease()` with a mismatched claim_token must return `false` and
/// leave the dispatch untouched.
pub async fn extend_lease_wrong_token<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-extend-wrong", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-extend-wrong", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let original_lease = claimed[0].lease_until;

    let ok = store
        .extend_lease("d1", "wrong-token", 60_000, 15_000)
        .await
        .unwrap();
    assert!(!ok);

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.lease_until, original_lease);
}

/// `interrupt()` supersedes all Queued dispatches in the thread.
pub async fn interrupt_supersedes_queued<S: MailboxStore>(store: &S) {
    store
        .enqueue(&make_dispatch("d1", "t-interrupt", "r1", 128, 1000))
        .await
        .unwrap();
    store
        .enqueue(&make_dispatch("d2", "t-interrupt", "r2", 128, 1000))
        .await
        .unwrap();

    let result = store.interrupt_detailed("t-interrupt", 2000).await.unwrap();
    assert_eq!(result.superseded_count, 2);
    assert_eq!(result.superseded_dispatches.len(), 2);
    assert!(result.active_dispatch.is_none());
    assert!(result.new_dispatch_epoch >= 1);
    let mut returned_ids = result
        .superseded_dispatches
        .iter()
        .map(|dispatch| {
            assert_eq!(dispatch.status, RunDispatchStatus::Superseded);
            dispatch.dispatch_id.as_str()
        })
        .collect::<Vec<_>>();
    returned_ids.sort_unstable();
    assert_eq!(returned_ids, vec!["d1", "d2"]);

    let listed = store
        .list_dispatches(
            "t-interrupt",
            Some(&[RunDispatchStatus::Superseded]),
            100,
            0,
        )
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
}

/// `interrupt()` returns the currently Claimed dispatch as `active_dispatch`
/// and supersedes the remaining Queued dispatches.
pub async fn interrupt_returns_active_claimed<S: MailboxStore>(store: &S) {
    store
        .enqueue(&make_dispatch("d1", "t-interrupt-active", "r1", 128, 1000))
        .await
        .unwrap();

    // Claim the first dispatch → Claimed.
    let claimed = store
        .claim("t-interrupt-active", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    let claimed_id = claimed[0].dispatch_id.clone();

    // Enqueue another dispatch (will be Queued).
    store
        .enqueue(&make_dispatch("d2", "t-interrupt-active", "r2", 128, 1000))
        .await
        .unwrap();

    let result = store
        .interrupt_detailed("t-interrupt-active", 2000)
        .await
        .unwrap();
    let active = result
        .active_dispatch
        .expect("interrupt must return active claimed dispatch");
    assert_eq!(active.dispatch_id, claimed_id);
    assert_eq!(active.status, RunDispatchStatus::Claimed);
    assert_eq!(result.superseded_count, 1);
    assert_eq!(result.superseded_dispatches.len(), 1);
    assert_eq!(result.superseded_dispatches[0].dispatch_id, "d2");
    assert_eq!(
        result.superseded_dispatches[0].status,
        RunDispatchStatus::Superseded
    );
}

/// Dispatch epoch reads must reflect interrupt bumps.
pub async fn current_dispatch_epoch_tracks_interrupt<S: MailboxStore>(store: &S) {
    let initial = store.current_dispatch_epoch("t-epoch").await.unwrap();
    assert_eq!(initial, 0);

    let interrupt = store.interrupt("t-epoch", 2000).await.unwrap();
    let current = store.current_dispatch_epoch("t-epoch").await.unwrap();

    assert_eq!(current, interrupt.new_dispatch_epoch);
    assert!(
        current > initial,
        "interrupt should advance the authoritative thread epoch"
    );
}

/// A claimed dispatch can be terminalized as superseded without entering the
/// runtime, clearing claim ownership and lease fields.
pub async fn supersede_claimed_terminalizes_active_dispatch<S: MailboxStore>(store: &S) {
    store
        .enqueue(&make_dispatch("d1", "t-supersede-claimed", "r1", 128, 1000))
        .await
        .unwrap();
    let claimed = store
        .claim("t-supersede-claimed", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();

    let superseded = store
        .supersede_claimed("d1", &token, 2000, "test supersede")
        .await
        .unwrap()
        .expect("claimed dispatch should be superseded");

    assert_eq!(superseded.status, RunDispatchStatus::Superseded);
    assert!(superseded.claim_token.is_none());
    assert!(superseded.claimed_by.is_none());
    assert!(superseded.lease_until.is_none());
    assert_eq!(superseded.completed_at, Some(2000));

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Superseded);
    assert!(loaded.claim_token.is_none());
}

/// Once an interrupt bumps the thread epoch, a stale claimed dispatch must lose
/// lease renewal instead of extending indefinitely.
pub async fn extend_lease_rejects_stale_claim_after_interrupt<S: MailboxStore>(store: &S) {
    store
        .enqueue(&make_dispatch("d1", "t-stale-lease", "r1", 128, 1000))
        .await
        .unwrap();
    let claimed = store
        .claim("t-stale-lease", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();

    store.interrupt("t-stale-lease", 1500).await.unwrap();

    let renewed = store
        .extend_lease("d1", &token, 30_000, 2000)
        .await
        .unwrap();
    assert!(!renewed, "stale claimed dispatch must not renew lease");

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Superseded);
    assert!(loaded.claim_token.is_none());
}

/// Runtime-start projection must not resurrect a dispatch that became stale by
/// epoch after claim.
pub async fn record_dispatch_start_rejects_stale_claim_after_interrupt<S: MailboxStore>(store: &S) {
    store
        .enqueue(&make_dispatch("d1", "t-stale-start", "r1", 128, 1000))
        .await
        .unwrap();
    let claimed = store
        .claim("t-stale-start", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();

    store.interrupt("t-stale-start", 1500).await.unwrap();

    let result = store
        .record_dispatch_start("d1", &token, "attempt-1", 2000)
        .await;
    assert!(
        result.is_err(),
        "stale claimed dispatch must reject runtime start"
    );

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Superseded);
    assert!(loaded.dispatch_instance_id.is_none());
    assert!(loaded.run_status.is_none());
}

/// Expired leases that became stale because of an interrupt must be
/// terminalized instead of requeued for another runtime.
pub async fn reclaim_expired_stale_claim_supersedes<S: MailboxStore>(store: &S) {
    store
        .enqueue(&make_dispatch("d1", "t-stale-reclaim", "r1", 128, 1000))
        .await
        .unwrap();
    store
        .claim("t-stale-reclaim", "consumer-1", 100, 1000, 1)
        .await
        .unwrap();

    store.interrupt("t-stale-reclaim", 1050).await.unwrap();

    let reclaimed = store.reclaim_expired_leases(2000, 10).await.unwrap();
    assert!(
        reclaimed.is_empty(),
        "stale claimed dispatches should not be returned for execution"
    );

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Superseded);
    assert!(loaded.claim_token.is_none());
    assert!(loaded.lease_until.is_none());
}

/// `enqueue()` rejects a second dispatch that reuses a non-terminal
/// dispatch's `dedupe_key`.
pub async fn dedupe_key_rejects_duplicate<S: MailboxStore>(store: &S) {
    let mut first = make_dispatch("d1", "t-dedupe", "r1", 128, 1000);
    first.dedupe_key = Some("unique-key".to_string());
    store.enqueue(&first).await.unwrap();

    let mut second = make_dispatch("d2", "t-dedupe", "r2", 128, 1000);
    second.dedupe_key = Some("unique-key".to_string());
    let result = store.enqueue(&second).await;
    assert!(result.is_err(), "duplicate dedupe_key must be rejected");
}

/// `reclaim_expired_leases()` returns orphaned Claimed dispatches to Queued.
pub async fn reclaim_expired_leases_requeues<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-reclaim", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    // Claim with a short lease ending at 1100.
    store
        .claim("t-reclaim", "consumer-1", 100, 1000, 1)
        .await
        .unwrap();

    // Advance past lease expiry.
    let reclaimed = store.reclaim_expired_leases(2000, 10).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].dispatch_id, "d1");
    assert_eq!(reclaimed[0].status, RunDispatchStatus::Queued);
    assert_eq!(reclaimed[0].attempt_count, 1);

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Queued);
    assert!(loaded.claim_token.is_none());
}

/// Expired lease reclaim follows `max_attempts` and dead-letters instead of
/// retrying forever.
pub async fn reclaim_expired_leases_dead_letters_at_max_attempts<S: MailboxStore>(store: &S) {
    let mut dispatch = make_dispatch("d1", "t-reclaim-max", "r1", 128, 1000);
    dispatch.max_attempts = 1;
    store.enqueue(&dispatch).await.unwrap();

    store
        .claim("t-reclaim-max", "consumer-1", 100, 1000, 1)
        .await
        .unwrap();

    let reclaimed = store.reclaim_expired_leases(2000, 10).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].dispatch_id, "d1");
    assert_eq!(reclaimed[0].status, RunDispatchStatus::DeadLetter);
    assert_eq!(reclaimed[0].attempt_count, 1);

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::DeadLetter);
    assert_eq!(loaded.attempt_count, 1);
    assert_claim_fields_clear(&loaded);
}

/// Concurrent maintenance sweeps are idempotent: only one worker may
/// terminalize an expired exhausted dispatch, and later sweeps must not return
/// it for execution again.
pub async fn reclaim_expired_leases_dead_letter_is_idempotent_under_concurrency<S: MailboxStore>(
    store: &S,
) {
    let mut dispatch = make_dispatch("d1", "t-reclaim-idempotent", "r1", 128, 1000);
    dispatch.max_attempts = 1;
    store.enqueue(&dispatch).await.unwrap();

    store
        .claim("t-reclaim-idempotent", "consumer-1", 100, 1000, 1)
        .await
        .unwrap();

    let (left, right) = tokio::join!(
        store.reclaim_expired_leases(2000, 10),
        store.reclaim_expired_leases(2000, 10)
    );
    let mut reclaimed = Vec::new();
    reclaimed.extend(left.unwrap());
    reclaimed.extend(right.unwrap());

    assert_eq!(
        reclaimed.len(),
        1,
        "concurrent sweeps must observe a single terminal transition"
    );
    assert_eq!(reclaimed[0].dispatch_id, "d1");
    assert_eq!(reclaimed[0].status, RunDispatchStatus::DeadLetter);

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::DeadLetter);
    assert_eq!(loaded.attempt_count, 1);
    assert_claim_fields_clear(&loaded);

    let later = store.reclaim_expired_leases(3000, 10).await.unwrap();
    assert!(
        later.is_empty(),
        "dead-lettered dispatch must not be returned by later sweeps"
    );
}

/// `purge_terminal()` removes terminal dispatches older than the cutoff.
pub async fn purge_terminal_removes_old<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-purge", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    // Cancel → Cancelled (terminal) with updated_at=1500.
    store.cancel("d1", 1500).await.unwrap();
    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Cancelled);

    // Cutoff > 1500 must purge the terminal dispatch.
    let purged = store.purge_terminal(2000).await.unwrap();
    assert_eq!(purged, 1);

    let gone = store.load_dispatch("d1").await.unwrap();
    assert!(gone.is_none(), "purged dispatch must be removed");
}

/// `queued_thread_ids()` lists every thread that currently holds at least one
/// Queued dispatch.
pub async fn queued_thread_ids_returns_active_threads<S: MailboxStore>(store: &S) {
    store
        .enqueue(&make_dispatch("d1", "thread-a", "r1", 128, 1000))
        .await
        .unwrap();
    store
        .enqueue(&make_dispatch("d2", "thread-b", "r2", 128, 1000))
        .await
        .unwrap();

    let ids = store.queued_thread_ids().await.unwrap();
    assert!(ids.contains(&"thread-a".to_string()));
    assert!(ids.contains(&"thread-b".to_string()));
    assert_eq!(ids.len(), 2);
}

/// `count_dispatches_by_status()` tracks low-cardinality lifecycle gauges.
pub async fn count_dispatches_by_status_tracks_lifecycle<S: MailboxStore>(store: &S) {
    store
        .enqueue(&make_dispatch("d1", "t-count-a", "r1", 128, 1000))
        .await
        .unwrap();
    store
        .enqueue(&make_dispatch("d2", "t-count-b", "r2", 128, 1000))
        .await
        .unwrap();

    assert_eq!(
        store
            .count_dispatches_by_status(RunDispatchStatus::Queued)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .count_dispatches_by_status(RunDispatchStatus::Claimed)
            .await
            .unwrap(),
        0
    );

    let claimed = store
        .claim("t-count-a", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let claim_token = claimed[0].claim_token.as_deref().unwrap();

    assert_eq!(
        store
            .count_dispatches_by_status(RunDispatchStatus::Queued)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .count_dispatches_by_status(RunDispatchStatus::Claimed)
            .await
            .unwrap(),
        1
    );

    store.ack("d1", claim_token, 1100).await.unwrap();
    store.cancel("d2", 1200).await.unwrap();

    assert_eq!(
        store
            .count_dispatches_by_status(RunDispatchStatus::Queued)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .count_dispatches_by_status(RunDispatchStatus::Acked)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .count_dispatches_by_status(RunDispatchStatus::Cancelled)
            .await
            .unwrap(),
        1
    );
}

/// `list_terminal_dispatches()` scans terminal dispatches across threads for
/// run lifecycle reconciliation.
pub async fn list_terminal_dispatches_returns_all_terminal<S: MailboxStore>(store: &S) {
    store
        .enqueue(&make_dispatch("d-cancel", "t-terminal-a", "r1", 128, 1000))
        .await
        .unwrap();
    store
        .enqueue(&make_dispatch(
            "d-supersede",
            "t-terminal-b",
            "r2",
            128,
            1001,
        ))
        .await
        .unwrap();
    store
        .enqueue(&make_dispatch("d-queued", "t-terminal-c", "r3", 128, 1002))
        .await
        .unwrap();

    store.cancel("d-cancel", 2000).await.unwrap();
    store.interrupt("t-terminal-b", 3000).await.unwrap();

    let listed = store.list_terminal_dispatches(10, 0).await.unwrap();
    let mut terminal = listed
        .iter()
        .map(|dispatch| (dispatch.dispatch_id.as_str(), dispatch.status))
        .collect::<Vec<_>>();
    terminal.sort_unstable_by_key(|(dispatch_id, _)| *dispatch_id);
    assert_eq!(
        terminal,
        vec![
            ("d-cancel", RunDispatchStatus::Cancelled),
            ("d-supersede", RunDispatchStatus::Superseded),
        ]
    );

    let paged = store.list_terminal_dispatches(1, 1).await.unwrap();
    assert_eq!(paged.len(), 1);
    assert!(paged[0].status.is_terminal());
}

/// `claim_dispatch()` targets a specific dispatch_id and transitions it to
/// Claimed.
pub async fn claim_dispatch_by_id<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-claim-id", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim_dispatch("d1", "consumer-1", 30_000, 1000)
        .await
        .unwrap()
        .expect("claim_dispatch should return the dispatch");
    assert_eq!(claimed.dispatch_id, "d1");
    assert_eq!(claimed.status, RunDispatchStatus::Claimed);
    assert!(claimed.claim_token.is_some());
}

/// `claim_dispatch(id)` MUST ignore `available_at` — it is the by-ID
/// inline-claim path used by `Mailbox::submit()` to claim the dispatch
/// it just wrote, where a future `available_at` keeps the sweeper away.
/// The queue-scan path (`claim()`) must still respect the guard.
pub async fn claim_dispatch_ignores_available_at<S: MailboxStore>(store: &S) {
    // Submit with available_at far in the future.
    let mut dispatch = make_dispatch("d-guarded", "t-guarded", "r1", 128, 1000);
    dispatch.available_at = 1_000_000;
    store.enqueue(&dispatch).await.unwrap();

    // Queue scan at t=1000 must skip it (future available_at).
    let scanned = store
        .claim("t-guarded", "consumer-1", 30_000, 1000, 10)
        .await
        .unwrap();
    assert!(
        scanned.is_empty(),
        "claim() by thread must respect available_at"
    );

    // By-id claim at the same time must succeed regardless of available_at.
    let claimed = store
        .claim_dispatch("d-guarded", "consumer-1", 30_000, 1000)
        .await
        .unwrap()
        .expect("claim_dispatch must ignore available_at");
    assert_eq!(claimed.dispatch_id, "d-guarded");
    assert_eq!(claimed.status, RunDispatchStatus::Claimed);
}

/// `record_dispatch_start()` sets the active runtime projection and clears any
/// stale terminal result fields from previous attempts.
pub async fn record_dispatch_start_marks_running<S: MailboxStore>(store: &S) {
    let dispatch = make_dispatch("d1", "t-start", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-start", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();

    store
        .record_dispatch_start("d1", &token, "attempt-1", 1500)
        .await
        .unwrap();

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.dispatch_instance_id.as_deref(), Some("attempt-1"));
    assert_eq!(
        loaded.run_status,
        Some(awaken_server_contract::contract::lifecycle::RunStatus::Running)
    );
    assert!(loaded.termination.is_none());
    assert!(loaded.run_response.is_none());
    assert!(loaded.run_error.is_none());
    assert!(loaded.completed_at.is_none());
}

/// `record_run_result()` requires the active claim and persists the full runtime
/// result projection.
pub async fn record_run_result_sets_terminal_projection<S: MailboxStore>(store: &S) {
    use awaken_server_contract::contract::lifecycle::{RunStatus, TerminationReason};
    use awaken_server_contract::contract::mailbox::RunDispatchResult;

    let dispatch = make_dispatch("d1", "t-result", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("t-result", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().unwrap();
    let result = RunDispatchResult {
        run_id: "r1".to_string(),
        dispatch_instance_id: "attempt-1".to_string(),
        status: RunStatus::Done,
        termination: Some(TerminationReason::NaturalEnd),
        response: Some("ok".to_string()),
        error: None,
    };

    store
        .record_run_result("d1", &token, &result, 2000)
        .await
        .unwrap();

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.dispatch_instance_id.as_deref(), Some("attempt-1"));
    assert_eq!(loaded.run_status, Some(RunStatus::Done));
    assert_eq!(loaded.termination, Some(TerminationReason::NaturalEnd));
    assert_eq!(loaded.run_response.as_deref(), Some("ok"));
    assert!(loaded.run_error.is_none());
    assert_eq!(loaded.completed_at, Some(2000));
}

/// Runtime projection writes must require an existing claimed dispatch.
pub async fn record_projection_rejects_missing_or_unclaimed_dispatch<S: MailboxStore>(store: &S) {
    use awaken_server_contract::contract::lifecycle::RunStatus;
    use awaken_server_contract::contract::mailbox::RunDispatchResult;

    let result = RunDispatchResult {
        run_id: "r-missing".to_string(),
        dispatch_instance_id: "attempt-missing".to_string(),
        status: RunStatus::Done,
        termination: None,
        response: None,
        error: None,
    };

    assert!(
        store
            .record_dispatch_start("missing", "token", "attempt-missing", 1000)
            .await
            .is_err()
    );
    assert!(
        store
            .record_run_result("missing", "token", &result, 1000)
            .await
            .is_err()
    );

    let dispatch = make_dispatch("d1", "t-projection-reject", "r1", 128, 1000);
    store.enqueue(&dispatch).await.unwrap();

    assert!(
        store
            .record_dispatch_start("d1", "token", "attempt-1", 1100)
            .await
            .is_err()
    );
    assert!(
        store
            .record_run_result("d1", "token", &result, 1100)
            .await
            .is_err()
    );

    let loaded = store.load_dispatch("d1").await.unwrap().unwrap();
    assert_eq!(loaded.status, RunDispatchStatus::Queued);
    assert!(loaded.run_status.is_none());
    assert!(loaded.dispatch_instance_id.is_none());
}

/// For dispatches that share a priority, `claim()` must return them in
/// `created_at` ascending order (FIFO).
pub async fn fifo_ordering_within_same_priority<S: MailboxStore>(store: &S) {
    // Use unique dispatch IDs to survive any cross-test bleed (not expected,
    // but cheap insurance against non-isolated backends).
    let older_id = format!("older-{}", Uuid::now_v7());
    let newer_id = format!("newer-{}", Uuid::now_v7());

    let mut older = make_dispatch(&older_id, "t-fifo", "r1", 128, 1000);
    older.created_at = 100;
    store.enqueue(&older).await.unwrap();

    let mut newer = make_dispatch(&newer_id, "t-fifo", "r2", 128, 1000);
    newer.created_at = 200;
    store.enqueue(&newer).await.unwrap();

    // Claim them one at a time; the older dispatch must come first.
    let first = store
        .claim("t-fifo", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].dispatch_id, older_id);

    let token = first[0].claim_token.clone().unwrap();
    store
        .ack(&first[0].dispatch_id, &token, 1100)
        .await
        .unwrap();

    let second = store
        .claim("t-fifo", "consumer-1", 30_000, 1100, 1)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].dispatch_id, newer_id);
}
