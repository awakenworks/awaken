//! T37-C01 (Work half) on a real file database: both published histories are
//! upgraded by the PRODUCTION open path, then observed through independent
//! handles and a reopen.
//!
//! Cause/effect graph: C1 a file written by a published release holds a live
//! Session lease (owner `legacy-owner`, epoch 7, expiry 60000) and had a higher
//! numeric id deleted; C2 `SqliteWorkQueue::open` upgrades it; C3 a second
//! independent handle and a later reopen observe it. Effects: E1 the live
//! bearer keeps its exact identity, owner, epoch and expiry; E2 a request with a
//! stale epoch is refused and changes nothing, the exact bearer renews; E3 the
//! migration ledger and allocator are applied once (reopen replays nothing);
//! E4 no new id reuses the live or the deleted numeric identity.
//!
//! Negative control (executed): a database created empty by the current
//! release has neither the legacy row nor its bearer, so "the schema opens" is
//! not evidence of an upgrade -- the same E1 check fails on it.

use super::*;
use crate::{SqliteWorkQueue, test_session_access_digest};
use awaken_executable_environment_contract::{
    HeartbeatResult, LeaseHeartbeat, SessionAccessAuthenticator, SessionWorkLeaseControl,
    SessionWorkStore, WorkQueries, WorkSessionAccess,
};
use awaken_lease_contract::fence::Epoch;
use awaken_sqlite_pool::EmbeddedSqlite;

const LIVE: &str = "work_0000000000000000";
const DELETED: &str = "work_0000000000009000";
const LIVE_ROW: &str = "INSERT INTO work_queue_item(work_id,seq,environment_id,data_type,data_id,metadata_json,state,created_at,lease_owner,lease_epoch,lease_expires_ms,lease_refreshed_ms,session_token_sha256) VALUES('work_0000000000000000',0,'legacy','session','live','{}','active','2026-01-01T00:00:00Z','legacy-owner',7,60000,0,'000000000000000000000000000000000000000000000000000000000000003b')";
const RETIRED_ROW: &str = "INSERT INTO work_queue_item(work_id,seq,environment_id,data_type,data_id,metadata_json,state,created_at) VALUES('work_0000000000009000',9000,'deleted','session','retired','{}','queued','2026-01-01T00:00:00Z')";

async fn write_published_history(path: &str, legacy: bool) {
    let pool = EmbeddedSqlite::file(path).open().await.unwrap();
    let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).unwrap();
    let published = if legacy {
        legacy_work_bundle().unwrap()
    } else {
        work_bundle().unwrap()
    };
    runner.run_bundle(&published).await.unwrap();
    runner
        .run_bundle(
            &MigrationBundle::new(
                CONVERGED_BUNDLE_ID,
                converged_work_bundle().unwrap().migrations()[..2].to_vec(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    sqlx::query(LIVE_ROW).execute(pool.sqlx()).await.unwrap();
    sqlx::query(RETIRED_ROW).execute(pool.sqlx()).await.unwrap();
    sqlx::query("DELETE FROM work_queue_item WHERE environment_id='deleted'")
        .execute(pool.sqlx())
        .await
        .unwrap();
    pool.close().await;
}

async fn ledger_and_allocator(path: &str) -> (i64, (String, i64)) {
    let pool = EmbeddedSqlite::file(path).open().await.unwrap();
    let ledger: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {NS}_schema_migrations"))
        .fetch_one(pool.sqlx())
        .await
        .unwrap();
    let allocator: (String, i64) =
        sqlx::query_as("SELECT generation_id,next_sequence FROM work_queue_identity_allocator")
            .fetch_one(pool.sqlx())
            .await
            .unwrap();
    pool.close().await;
    (ledger, allocator)
}

/// E1: the exact legacy bearer, or `None` when the store has no such lease.
async fn legacy_bearer(queue: &SqliteWorkQueue) -> Option<WorkSessionAccess> {
    let access = queue
        .authenticate_session_access(&test_session_access_digest(59), 0)
        .await
        .unwrap()?;
    (access.work_id == LIVE
        && access.lease_owner == "legacy-owner"
        && access.lease_epoch.get() == 7
        && access.expires_at_unix_ms == 60_000)
        .then_some(access)
}

#[tokio::test]
async fn file_upgrade_of_both_published_histories_preserves_the_live_lease_across_reopen() {
    for legacy in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("published.db");
        let path = path.to_str().unwrap();
        write_published_history(path, legacy).await;

        let upgraded = SqliteWorkQueue::open(path, awaken_time::system_clock())
            .await
            .expect("C2 production open upgrades the published file");
        let peer = SqliteWorkQueue::open(path, awaken_time::system_clock())
            .await
            .expect("C3 independent handle");
        let (ledger, allocator) = ledger_and_allocator(path).await;
        assert_eq!(
            allocator.1, 0,
            "{legacy}: no remaining row seeds the counter"
        );

        let access = legacy_bearer(&peer)
            .await
            .unwrap_or_else(|| panic!("{legacy} E1 exact legacy bearer"));
        let live = upgraded.get("legacy", LIVE).await.unwrap().unwrap();
        assert_eq!(live.created_at, "2026-01-01T00:00:00Z", "{legacy} E1");

        let stale = WorkSessionAccess {
            lease_epoch: Epoch::from_persisted(6).unwrap(),
            ..access.clone()
        };
        assert!(
            matches!(
                upgraded
                    .heartbeat_with_session_access(&stale, 1_000, LeaseHeartbeat::unconditional())
                    .await
                    .unwrap(),
                HeartbeatResult::PreconditionFailed | HeartbeatResult::NotFound
            ),
            "{legacy} E2 stale epoch refused"
        );
        assert_eq!(
            legacy_bearer(&peer).await,
            Some(access.clone()),
            "{legacy} E2 refusal changed nothing"
        );
        assert!(
            matches!(
                peer.heartbeat_with_session_access(&access, 1_000, LeaseHeartbeat::unconditional())
                    .await
                    .unwrap(),
                HeartbeatResult::Accepted(_)
            ),
            "{legacy} E2 the exact upgraded bearer renews"
        );

        let next = upgraded
            .enqueue_session("new", &"next".into())
            .await
            .unwrap();
        assert_eq!(next, format!("work_g{}{:016x}", allocator.0, 1), "{legacy}");
        assert_ne!(next, LIVE, "{legacy} E4");
        assert_ne!(next, DELETED, "{legacy} E4");
        assert!(upgraded.get("deleted", DELETED).await.unwrap().is_none());
        drop((upgraded, peer));

        let reopened = SqliteWorkQueue::open(path, awaken_time::system_clock())
            .await
            .expect("reopen");
        let (ledger_after, allocator_after) = ledger_and_allocator(path).await;
        assert_eq!(ledger_after, ledger, "{legacy} E3 migrations applied once");
        assert_eq!(
            allocator_after,
            (allocator.0.clone(), 2),
            "{legacy} E3 reopen never replaces generation or counter"
        );
        let renewed = reopened
            .authenticate_session_access(&test_session_access_digest(59), 1_000)
            .await
            .unwrap()
            .expect("E1 bearer survives reopen");
        assert_eq!(
            (renewed.work_id.as_str(), renewed.lease_epoch.get()),
            (LIVE, 7),
            "{legacy} E1 identity and epoch after reopen"
        );
        let after_reopen = reopened
            .enqueue_session("new", &"after-reopen".into())
            .await
            .unwrap();
        assert_ne!(after_reopen, next, "{legacy} E4 no reuse after reopen");
        assert_ne!(after_reopen, DELETED, "{legacy} E4");
    }

    // NC: an empty current-release file "opens" but carries no upgraded fact.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fresh.db");
    let fresh = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    assert!(
        legacy_bearer(&fresh).await.is_none(),
        "NC: creating an empty schema cannot satisfy the upgrade oracle"
    );
}
