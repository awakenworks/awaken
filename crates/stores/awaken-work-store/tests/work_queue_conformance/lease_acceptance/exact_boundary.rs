//! T09-C01: the Work lease reclaim boundary is the exact contract instant on
//! every real backend, observed through independent handles and a reopen.
//!
//! Cause/effect graph: C1 a Session Work is claimed at `t0`, yielding the
//! authenticated expiry `E`; C2 a second client claims at `E - 1`; C3 a third
//! client on a *reopened* handle (file SQLite: a new connection to the same
//! file; PostgreSQL: a new pool) claims at `E`. Effects: E1 C2 is refused (the
//! lease is still live); E2 C3 reclaims the same Work under a higher epoch;
//! E3 the first bearer is revoked at `E`. The decision is derived from the
//! store's own expiry, never from wall-clock sleeps.
//!
//! | Rule | probe instant | expected                              |
//! |------|---------------|---------------------------------------|
//! | B1   | `E - 1`       | claim refused                          |
//! | B2   | `E`           | same Work, higher epoch, old bearer gone |
//!
//! Oracle 1 (cross-backend agreement) compares every backend's observation
//! with the one exact reference row. Negative control (executed): the same
//! checker driven by a probe that overshoots by one millisecond -- what a
//! sleep-based test observes -- must NOT produce the exact row.

use super::*;

/// What one backend shows at the reclaim boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundaryObservation {
    held_until_the_last_millisecond: bool,
    reclaimed_at_expiry: bool,
    same_work_reclaimed: bool,
    epoch_advanced: bool,
    first_bearer_revoked: bool,
}

const EXACT: BoundaryObservation = BoundaryObservation {
    held_until_the_last_millisecond: true,
    reclaimed_at_expiry: true,
    same_work_reclaimed: true,
    epoch_advanced: true,
    first_bearer_revoked: true,
};

/// A further independent handle over the pair's durable authority.
async fn reopened(pair: &Pair) -> Handle {
    if let Some(directory) = pair._dir.as_ref() {
        let path = directory.path().join("work.db");
        return Arc::new(
            SqliteWorkQueue::open(path.to_str().expect("utf-8 path"), pair.clock.clone())
                .await
                .expect("reopened SQLite handle"),
        );
    }
    if let Some(fixture) = pair.postgres.as_ref() {
        return Arc::new(
            PostgresWorkQueue::connect_existing(&fixture.scoped_url, pair.clock.clone())
                .await
                .expect("reopened PostgreSQL pool"),
        );
    }
    // Volatile has one logical model; another handle is the same model.
    pair.a.clone()
}

/// Run B1/B2 with every probe instant shifted by `probe_skew_ms`.
async fn observe_boundary(pair: &Pair, probe_skew_ms: u64) -> BoundaryObservation {
    let env = "boundary-env";
    let work = pair
        .a
        .enqueue_session(env, &SessionId::from("boundary-session"))
        .await
        .unwrap();
    assert!(
        claim_as(&pair.a, env, "owner-first", 5, session_access_digest(21)).await,
        "{} C1 first claim",
        pair.label
    );
    let first = access(&pair.a, &session_access_digest(21), 5)
        .await
        .expect("C1 first bearer");
    let expiry = first.expires_at_unix_ms;
    assert_eq!(expiry, 5 + LEASE_TTL_MS, "{} contract expiry", pair.label);
    let early = claim_as(
        &pair.b,
        env,
        "owner-early",
        expiry - 1 + probe_skew_ms,
        session_access_digest(22),
    )
    .await;
    let reopened = reopened(pair).await;
    let at_expiry = claim_as(
        &reopened,
        env,
        "owner-boundary",
        expiry + probe_skew_ms,
        session_access_digest(23),
    )
    .await;
    let winner = access(
        &reopened,
        &session_access_digest(23),
        expiry + probe_skew_ms,
    )
    .await;
    BoundaryObservation {
        held_until_the_last_millisecond: !early,
        reclaimed_at_expiry: at_expiry,
        same_work_reclaimed: winner.as_ref().is_some_and(|w| w.work_id == work),
        epoch_advanced: winner
            .as_ref()
            .is_some_and(|w| w.lease_epoch > first.lease_epoch),
        first_bearer_revoked: access(&pair.b, &session_access_digest(21), expiry)
            .await
            .is_none(),
    }
}

/// Oracle 0 plus the executed negative control on one backend.
async fn exact_boundary_on(exact: Pair, skewed: Pair) -> BoundaryObservation {
    let label = exact.label;
    let observed = observe_boundary(&exact, 0).await;
    assert_eq!(observed, EXACT, "{label} B1/B2 exact contract boundary");
    exact.cleanup().await;
    let overshoot = observe_boundary(&skewed, 1).await;
    assert_ne!(
        overshoot, EXACT,
        "{label} NC: a probe that misses the boundary by 1ms must not sign off"
    );
    assert!(
        !overshoot.held_until_the_last_millisecond,
        "{label} NC: the overshooting probe is detected at B1"
    );
    skewed.cleanup().await;
    observed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_reclaim_boundary_is_identical_on_volatile_and_file_sqlite() {
    let volatile = exact_boundary_on(Pair::volatile(), Pair::volatile()).await;
    let sqlite = exact_boundary_on(Pair::sqlite_file().await, Pair::sqlite_file().await).await;
    assert_eq!(volatile, sqlite, "oracle 1: backends agree on the boundary");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_exact_reclaim_boundary_matches_the_reference() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let reference = exact_boundary_on(Pair::volatile(), Pair::volatile()).await;
    let postgres = exact_boundary_on(Pair::postgres(&url).await, Pair::postgres(&url).await).await;
    assert_eq!(
        postgres, reference,
        "oracle 1: PostgreSQL agrees with the reference"
    );
}

#[test]
fn postgres_exact_reclaim_boundary_matches_the_reference_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "lease_acceptance::exact_boundary::postgres_exact_reclaim_boundary_matches_the_reference",
    );
}
