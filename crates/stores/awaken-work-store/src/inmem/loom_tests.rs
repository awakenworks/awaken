// @formal evidence session_work.release_requires_exact_owner_and_epoch
// @formal evidence work.lease_authority_snapshot_and_owner_mutations_are_atomic
//! Whole reference-queue authority interleavings, using the actual public port.
use super::*;
use crate::test_session_access_digest;
use awaken_session_types::SessionId;
use loom::{sync::Arc, thread};
use std::{
    future::Future,
    task::{Context, Poll, Waker},
};

fn complete<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("the in-memory port must complete without external I/O"),
    }
}

loom::thread_local! {
    static READING_MS: std::cell::Cell<u64> = std::cell::Cell::new(0);
}

/// The store clock as each competing thread samples it. The queue still owns
/// every reading; a thread only fixes the instant at which its own operations
/// sample, so the model can order a commit at one instant against a commit at
/// another, including the order a monotone clock would forbid.
struct ThreadReadings;

impl ThreadReadings {
    fn queue() -> Arc<InMemoryWorkQueue> {
        Arc::new(InMemoryWorkQueue::new(std::sync::Arc::new(Self)))
    }

    /// This thread's operations sample the store clock at `now_ms`.
    fn at(now_ms: u64) {
        READING_MS.with(|reading| reading.set(now_ms));
    }
}

impl WallClock for ThreadReadings {
    fn now(&self) -> awaken_time::UtcTime {
        awaken_time::UtcTime::from_epoch_ms(READING_MS.with(std::cell::Cell::get))
    }
}

fn check_interleavings(run: impl Fn() + Send + Sync + 'static) {
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(2);
    model.max_branches = 10_000;
    // H1 inherited duration/permutation/checkpoint settings must not turn an
    // exhaustive bounded run into a successful prefix or resumed suffix.
    model.max_permutations = None;
    model.max_duration = None;
    model.checkpoint_file = None;
    model.checkpoint_interval = 20_000;
    let explored = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&explored);
    model.check(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        run();
    });
    // H2 every scenario has two competing threads: executing only its first
    // schedule cannot qualify coverage, even when that schedule is correct.
    let count = explored.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        count > 1,
        "the bounded scheduler must explore competing orders"
    );
    eprintln!("completed {count} schedules within the two-preemption bound");
}

#[test]
fn release_and_reacquire_preserve_one_complete_authority() {
    // C1 epoch one is active; C2 exact release races explicit wake+acquire.
    // R1 wake before release -> stopped; R2 release before wake -> epoch two
    // active. No schedule may expose active Work with absent lease authority,
    // nor may the old release clear a successor installed after its check.
    check_interleavings(|| {
        let queue = ThreadReadings::queue();
        ThreadReadings::at(0);
        let lease = complete(queue.acquire_session("env", &SessionId::from("session"), "owner"))
            .unwrap()
            .unwrap();
        let releasing = Arc::clone(&queue);
        let release = thread::spawn(move || complete(releasing.release_session(&lease)).unwrap());
        let waking = Arc::clone(&queue);
        let wake = thread::spawn(move || {
            ThreadReadings::at(1);
            complete(waking.wake_session("env", &SessionId::from("session"))).unwrap();
            complete(waking.acquire_session("env", &SessionId::from("session"), "owner")).unwrap();
        });
        assert!(release.join().unwrap());
        wake.join().unwrap();
        let works = complete(queue.list("env")).unwrap();
        if works[0].state == WorkState::Active {
            assert!(
                queue
                    .authority
                    .lock()
                    .current_session_lease("env", &SessionId::from("session"), 1)
                    .is_some(),
                "an old release cannot clear the successor authority"
            );
        }
    });
}

#[test]
fn token_authentication_never_borrows_a_replacement_epoch() {
    // C1 token T1 was issued with owner A/epoch one; C2 authentication races
    // expiry/reclaim minting T2 for B/epoch two. R1 auth before reclaim -> A/1;
    // R2 auth after reclaim -> None. Mixed T1 + B/2 is never a valid snapshot.
    check_interleavings(|| {
        let queue = ThreadReadings::queue();
        complete(queue.enqueue_session("env", &SessionId::from("session"))).unwrap();
        ThreadReadings::at(0);
        let claim = complete(queue.claim_with_session_access(
            "env",
            "owner-a",
            "poller",
            None,
            &test_session_access_digest(1),
        ))
        .unwrap()
        .unwrap();
        let token = claim.session_access.unwrap();
        let authenticating = Arc::clone(&queue);
        let auth = thread::spawn(move || {
            ThreadReadings::at(1);
            complete(authenticating.authenticate_session_access(&token)).unwrap()
        });
        let claiming = Arc::clone(&queue);
        let replacement = thread::spawn(move || {
            ThreadReadings::at(LEASE_TTL_MS);
            complete(claiming.claim_with_session_access(
                "env",
                "owner-b",
                "poller",
                None,
                &test_session_access_digest(2),
            ))
            .unwrap()
            .unwrap();
        });
        let observed = auth.join().unwrap();
        replacement.join().unwrap();
        if let Some(access) = observed {
            assert_eq!(
                access.lease_owner, "owner-a",
                "token and owner must come from one claim"
            );
            assert_eq!(
                access.lease_epoch.get(),
                1,
                "token and epoch must come from one claim"
            );
        }
    });
}

#[test]
fn owner_teardown_cannot_clear_an_intervening_reclaim() {
    // C1 A owns epoch one; C2 teardown(A) races B reclaiming at exact expiry.
    // R1 teardown first -> queued then B/2; R2 reclaim first -> teardown(A)
    // affects zero rows. Both orders leave B/2 active with its fresh token.
    check_interleavings(|| {
        let queue = ThreadReadings::queue();
        complete(queue.enqueue_session("env", &SessionId::from("session"))).unwrap();
        ThreadReadings::at(0);
        complete(queue.claim_with_session_access(
            "env",
            "owner-a",
            "poller",
            None,
            &test_session_access_digest(3),
        ))
        .unwrap()
        .unwrap();
        let retiring = Arc::clone(&queue);
        let release = thread::spawn(move || complete(retiring.release_owner("owner-a")).unwrap());
        let claiming = Arc::clone(&queue);
        let replacement = thread::spawn(move || {
            ThreadReadings::at(LEASE_TTL_MS);
            complete(claiming.claim_with_session_access(
                "env",
                "owner-b",
                "poller",
                None,
                &test_session_access_digest(4),
            ))
            .unwrap()
            .unwrap()
        });
        release.join().unwrap();
        let token = replacement.join().unwrap().session_access.unwrap();
        ThreadReadings::at(LEASE_TTL_MS);
        let access = complete(queue.authenticate_session_access(&token))
            .unwrap()
            .expect("teardown of A cannot revoke B's claim");
        assert_eq!(
            (access.lease_owner.as_str(), access.lease_epoch.get()),
            ("owner-b", 2)
        );
    });
}

#[test]
fn renewal_and_reclaim_return_complete_lease_snapshots() {
    // C1 A/1 expires at TTL; C2 A renews at time one while B polls at TTL.
    // R1 renewal first -> A/1 with TTL+1, so B cannot reclaim yet;
    // R2 reclaim first -> B/2 with 2*TTL, and A observes B without extending it.
    // A stale snapshot must not overwrite B's expiry with the shorter A lease.
    check_interleavings(|| {
        let queue = ThreadReadings::queue();
        ThreadReadings::at(0);
        complete(queue.acquire_session("env", &SessionId::from("session"), "owner-a"))
            .unwrap()
            .unwrap();
        let renewing = Arc::clone(&queue);
        let renewal = thread::spawn(move || {
            ThreadReadings::at(1);
            complete(renewing.acquire_session("env", &SessionId::from("session"), "owner-a"))
                .unwrap()
        });
        let claiming = Arc::clone(&queue);
        let replacement = thread::spawn(move || {
            ThreadReadings::at(LEASE_TTL_MS);
            complete(claiming.claim("env", "owner-b")).unwrap()
        });
        let renewed = renewal
            .join()
            .unwrap()
            .expect("one coherent lease is always available");
        let replaced = replacement.join().unwrap().is_some();
        let authority = queue.authority.lock();
        let current = authority
            .current_session_lease("env", &SessionId::from("session"), 1)
            .unwrap();
        let expected = if replaced {
            ("owner-b", 2, LEASE_TTL_MS * 2)
        } else {
            ("owner-a", 1, LEASE_TTL_MS + 1)
        };
        assert_eq!(
            (
                current.owner(),
                current.epoch(),
                current.expires_at_unix_ms()
            ),
            expected
        );
        assert!(
            (
                renewed.owner(),
                renewed.epoch(),
                renewed.expires_at_unix_ms()
            ) == ("owner-a", 1, LEASE_TTL_MS + 1)
                || (
                    renewed.owner(),
                    renewed.epoch(),
                    renewed.expires_at_unix_ms()
                ) == ("owner-b", 2, LEASE_TTL_MS * 2),
            "renewal must return one complete claim snapshot"
        );
    });
}
