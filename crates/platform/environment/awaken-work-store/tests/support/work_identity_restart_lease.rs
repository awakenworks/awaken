//! T09-C03: Work lease identity across a native process rebuild.
//!
//! Cause/effect graph: C1 process P1 claims Session Work W1 for (Session S,
//! owner O) and records the bearer's full lease coordinate; C2 P1 removes the
//! Environment and exits; C3 a fresh process P2 over the same durable store
//! enqueues the same logical Session and claims it as the same owner. Effects:
//! E1 W2 != W1 (identity never recycled across the rebuild); E2 the lease epoch
//! keeps its per-Work generation meaning (the replacement starts at the same
//! first grant, it neither continues nor reuses W1's lease); E3 every
//! request with W1's coordinate (bearer digest, ack, heartbeat, stop) is
//! refused as NotFound; E4 W2's own bearer is current and accepted.
//!
//! Negative control (executed): a queue-length-only comparison cannot tell a
//! recycled identity from a fresh one (both views hold exactly one item), while
//! [`identity_verdict`] rejects the mutant observation in which W2 reuses W1.
//! Runtime incarnation is not a Work-store coordinate (`WorkSessionAccess`
//! carries Work id, owner, epoch and expiry only); that half of the oracle is
//! owned by the Session realization lease.

use super::*;
use awaken_executable_environment_contract::{LeaseHeartbeat, WorkSessionAccess};
use awaken_lease_contract::fence::Epoch;

const ENV: &str = "restart-lease";
const SESSION: &str = "same-session";
const OWNER: &str = "same-worker";

fn record_line(access: &WorkSessionAccess) -> String {
    serde_json::json!({
        "work_id": access.work_id,
        "environment_id": access.environment_id,
        "session_id": access.session_id.as_str(),
        "lease_owner": access.lease_owner,
        "lease_epoch": access.lease_epoch.get(),
        "expires_at_unix_ms": access.expires_at_unix_ms,
    })
    .to_string()
}

fn parse_record(text: &str) -> WorkSessionAccess {
    let value: serde_json::Value = serde_json::from_str(text).expect("recorded lease coordinate");
    WorkSessionAccess {
        work_id: value["work_id"].as_str().unwrap().to_owned(),
        environment_id: value["environment_id"].as_str().unwrap().to_owned(),
        session_id: value["session_id"].as_str().unwrap().into(),
        lease_owner: value["lease_owner"].as_str().unwrap().to_owned(),
        lease_epoch: Epoch::from_persisted(value["lease_epoch"].as_u64().unwrap())
            .expect("recorded epoch is a grant"),
        expires_at_unix_ms: value["expires_at_unix_ms"].as_u64().unwrap(),
    }
}

/// The identity oracle: E1 + E2 + E3 over the two full coordinates.
fn identity_verdict(
    old: &WorkSessionAccess,
    new: &WorkSessionAccess,
    old_coordinate_refused: bool,
) -> Result<(), String> {
    if new.work_id == old.work_id {
        return Err(format!("E1 recycled Work identity {}", new.work_id));
    }
    if new.lease_owner != old.lease_owner || new.session_id != old.session_id {
        return Err("fixture drift: owner/session must be identical".into());
    }
    if new.lease_epoch != old.lease_epoch {
        return Err(format!(
            "E2 epoch meaning changed: replacement {} vs original {}",
            new.lease_epoch, old.lease_epoch
        ));
    }
    if !old_coordinate_refused {
        return Err("E3 the retired coordinate still acts".into());
    }
    Ok(())
}

pub(super) async fn seed<Q: WorkBackend>(queue: &Q, record: &str) {
    let digest = awaken_work_store::test_session_access_digest(9301);
    queue.enqueue_session(ENV, &SESSION.into()).await.unwrap();
    queue
        .claim_with_session_access(ENV, OWNER, "poller", None, &digest)
        .await
        .unwrap()
        .expect("C1 claim");
    let access = queue
        .authenticate_session_access(&digest)
        .await
        .unwrap()
        .expect("C1 bearer");
    assert_eq!(access.lease_epoch, Epoch::FIRST, "C1 first grant");
    std::fs::write(record, record_line(&access)).unwrap();
    queue.remove_env(ENV).await.unwrap();
}

pub(super) async fn assert_after_restart<Q: WorkBackend>(queue: &Q, record: &str) {
    let old = parse_record(&std::fs::read_to_string(record).unwrap());
    let old_digest = awaken_work_store::test_session_access_digest(9301);
    assert!(
        queue
            .authenticate_session_access(&old_digest)
            .await
            .unwrap()
            .is_none(),
        "E3 old bearer"
    );
    queue.enqueue_session(ENV, &SESSION.into()).await.unwrap();
    let new_digest = awaken_work_store::test_session_access_digest(9302);
    queue
        .claim_with_session_access(ENV, OWNER, "poller", None, &new_digest)
        .await
        .unwrap()
        .expect("C3 claim");
    let new = queue
        .authenticate_session_access(&new_digest)
        .await
        .unwrap()
        .expect("E4 current bearer");
    let ack_refused = queue
        .ack_with_session_access(&old)
        .await
        .unwrap()
        .is_not_found();
    let heartbeat_refused = queue
        .heartbeat_with_session_access(&old, LeaseHeartbeat::unconditional())
        .await
        .unwrap()
        .is_not_found();
    let stop_refused = queue
        .stop_with_session_access(&old)
        .await
        .unwrap()
        .is_not_found();
    identity_verdict(&old, &new, ack_refused && heartbeat_refused && stop_refused)
        .unwrap_or_else(|failure| panic!("{failure}"));

    // NC: the length-only view is indistinguishable, the identity view is not.
    assert_eq!(queue.list(ENV).await.unwrap().len(), 1, "NC length view");
    let recycled = WorkSessionAccess {
        work_id: old.work_id.clone(),
        ..new.clone()
    };
    assert!(
        identity_verdict(&old, &recycled, true).is_err(),
        "NC: a recycled identity must be rejected by the identity oracle"
    );
    assert!(
        identity_verdict(&old, &new, false).is_err(),
        "NC: a retired coordinate that still acts must be rejected"
    );

    assert_eq!(
        queue
            .authenticate_session_access(&new_digest)
            .await
            .unwrap(),
        Some(new.clone()),
        "E4 rejected old requests left the replacement intact"
    );
    assert!(
        queue
            .ack_with_session_access(&new)
            .await
            .unwrap()
            .is_accepted(),
        "E4"
    );
}

#[test]
fn sqlite_lease_identity_keeps_its_meaning_across_a_native_process_restart() {
    let directory = tempfile::tempdir().unwrap();
    run_phases(
        "sqlite",
        directory.path().join("lease-restart.db").to_str().unwrap(),
        &directory.path().join("retired-lease.json"),
        &["seed-lease", "assert-lease"],
    );
}

#[tokio::test]
async fn postgres_lease_identity_keeps_its_meaning_across_a_native_process_restart() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = WorkPostgresFixture::new(&url, 2).await;
    let endpoint = fixture.scoped_url.clone();
    let directory = tempfile::tempdir().unwrap();
    let record = directory.path().join("retired-lease.json");
    tokio::task::spawn_blocking(move || {
        run_phases(
            "postgres",
            &endpoint,
            &record,
            &["seed-lease", "assert-lease"],
        );
    })
    .await
    .unwrap();
    fixture.cleanup().await;
}

#[test]
fn postgres_lease_identity_keeps_its_meaning_across_a_native_process_restart_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "restart::lease::postgres_lease_identity_keeps_its_meaning_across_a_native_process_restart",
    );
}
