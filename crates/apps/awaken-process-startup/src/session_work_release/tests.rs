//! Real PostgreSQL evidence for the existing conditional Session Work effect.
//! Fixture admission/namespaces and the checkout policy remain with their owners.

use super::{ClaimConnectionUse, SessionWorkReleasePersistence, conditional_session_work_release};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch::{
    ConditionalSessionWorkRelease, DispatchAdministration as _, DispatchClaims as _,
    DispatchCommitAuthority as _, DispatchControlReads as _, DispatchEnqueue as _,
    DispatchExecution as _, PhysicalAttempt, PostgresDispatchStore, RunClaim, RunDispatch,
};
use awaken_session_application_contract::{
    SessionWorkAcquisition, SessionWorkGrant, SessionWorkOwnership,
    SessionWorkerLeaseAuthority as _,
};
use awaken_session_types::SessionId;
use sqlx::postgres::{PgConnectOptions, PgPool};
use sqlx::{Connection as _, Executor as _};
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

const OWNER: &str = "parking-request";
const ENV: &str = "env_local";
const PROGRESS: Duration = Duration::from_secs(10);
type WorkRow = (String, Option<String>, i64, Option<i64>, Option<i64>);

struct Fixture {
    d: PgPool,
    roots: PgPool,
    work: PgPool,
    observer: PgPool,
    d_url: String,
    tag: String,
    dispatch: Arc<PostgresDispatchStore>,
    assignments: Arc<awaken_session_application::SessionWorkAssignments>,
    managed: awaken_protocol_managed::ManagedState,
    clock: Arc<awaken_lease_testkit::ManualClock>,
    roots_shared: bool,
    work_shared: bool,
}

#[cfg(test)]
async fn owned_pool(capacity: u32, tag: &str) -> (PgPool, String) {
    let url = awaken_reliability_testkit::postgres_fixture_url()
        .expect("live conditional Work test requires the admitted PostgreSQL service");
    (pool_on(&url, capacity, tag).await, url)
}

#[cfg(test)]
async fn pool_on(url: &str, capacity: u32, tag: &str) -> PgPool {
    let options = PgConnectOptions::from_str(url)
        .expect("admitted PostgreSQL URL")
        .application_name(tag);
    pool_with_options(options, capacity).await
}

#[cfg(test)]
async fn pool_with_options(options: PgConnectOptions, capacity: u32) -> PgPool {
    // Do not replace after_connect/before_acquire: the canonical checkout
    // ROLLBACK policy must remain present for these regression tests.
    awaken_sql_storage::postgres_pool_options()
        .max_connections(capacity)
        .acquire_timeout(PROGRESS)
        .connect_with(options)
        .await
        .expect("owned fixture pool")
}

#[cfg(test)]
impl Fixture {
    async fn new(capacity: u32, roots_shared: bool, work_shared: bool) -> Self {
        let tag = format!("park_{}", uuid::Uuid::now_v7().simple());
        let (d, d_url) = owned_pool(capacity, &tag).await;
        let independent = if !roots_shared || !work_shared {
            Some(owned_pool(capacity, &format!("{tag}_other")).await.0)
        } else {
            None
        };
        let roots = if roots_shared {
            d.clone()
        } else {
            independent.as_ref().unwrap().clone()
        };
        let work = if work_shared {
            d.clone()
        } else {
            independent.as_ref().unwrap().clone()
        };
        Self::from_sources(d, d_url, roots, work, roots_shared, work_shared, tag).await
    }

    async fn from_sources(
        d: PgPool,
        d_url: String,
        roots: PgPool,
        work: PgPool,
        roots_shared: bool,
        work_shared: bool,
        tag: String,
    ) -> Self {
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100_000));
        let sessions = Arc::new(
            awaken_session_store::PostgresManagedSessionRepository::with_pool(roots.clone())
                .await
                .expect("real Session ledger"),
        );
        let queue = Arc::new(
            awaken_work_store::PostgresWorkQueue::with_pool(work.clone(), clock.clone())
                .await
                .expect("real Work ledger"),
        );
        let catalog =
            Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
        catalog
            .install_seed(awaken_environment_application::default_environment_registration())
            .expect("canonical local Environment");
        let environments = Arc::new(
            awaken_environment_application::execution::EnvironmentExecutionApplication::new(
                queue, catalog,
            ),
        );
        let runtime = Arc::new(awaken_protocol_managed_testkit::RehydrateFake::default());
        let managed = awaken_protocol_managed_testkit::managed_state_from_parts(
            runtime.clone(),
            Some(runtime),
            sessions,
            environments,
            clock.clone(),
            Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        );
        let assignments = managed.session_application().work_assignments();
        let dispatch = Arc::new(
            PostgresDispatchStore::with_pool(d.clone())
                .await
                .expect("real Dispatch ledger"),
        );
        // Explicit observation/competing-Coordinator connections are outside
        // the product pool cap and carry a different application tag.
        let observer = pool_on(&d_url, 2, &format!("{tag}_observer")).await;
        Self {
            d,
            roots,
            work,
            observer,
            d_url,
            tag,
            dispatch,
            assignments,
            managed,
            clock,
            roots_shared,
            work_shared,
        }
    }

    fn release(&self) -> Arc<dyn ConditionalSessionWorkRelease> {
        let persistence = if !self.roots_shared && !self.work_shared {
            SessionWorkReleasePersistence::Independent
        } else {
            SessionWorkReleasePersistence::Postgres {
                pool: self.d.clone(),
                roots: if self.roots_shared {
                    ClaimConnectionUse::Borrowed
                } else {
                    ClaimConnectionUse::Independent
                },
                work: if self.work_shared {
                    ClaimConnectionUse::Borrowed
                } else {
                    ClaimConnectionUse::Independent
                },
            }
        };
        conditional_session_work_release(
            persistence,
            self.dispatch.clone(),
            self.assignments.clone(),
            self.clock.clone(),
        )
    }

    async fn entry(&self) -> Entry {
        let session = self
            .managed
            .create_session(
                awaken_protocol_managed::types::SessionCreateParams::new("assistant", ENV),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .expect("canonical Session create");
        let id = SessionId::from(session.id);
        let grant = match self
            .assignments
            .acquire_session_work(&id, OWNER, SessionWorkAcquisition::ClaimedRun)
            .await
            .expect("Work acquisition")
        {
            SessionWorkOwnership::Leased(grant) => grant,
            _ => panic!("the self-hosted Session must lease its real Work"),
        };
        let run = RunId(format!("run-{}", id.as_str()));
        let request = RunDispatch::new(awaken_runtime_contract::RunActivation::new(
            run.clone(),
            ThreadId(id.as_str().into()),
            awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
                .model(
                    awaken_runtime_contract::ModelBinding::parse(
                        "default",
                        "parking-model",
                        "default",
                    )
                    .expect("valid canonical Native fixture binding"),
                )
                .build(),
            Vec::new(),
        ))
        .for_session(ThreadId(id.as_str().into()))
        .with_session_activity_epoch(1);
        self.dispatch
            .enqueue(request)
            .await
            .expect("Root Run dispatch");
        let claimed = self
            .dispatch
            .claim(OWNER, 60_000, &Default::default())
            .await
            .unwrap()
            .expect("claim");
        let claim = RunClaim::from(&claimed.lease);
        assert_eq!(
            self.dispatch
                .begin_attempt(&claim, None)
                .await
                .expect("physical slot"),
            awaken_dispatch::AttemptAdmission::Applied
        );
        assert_eq!(
            self.dispatch
                .close_attempt(&claim)
                .await
                .expect("irreversible local entry close"),
            awaken_dispatch::SettleOutcome::Applied
        );
        let attempt = self
            .dispatch
            .load_physical_attempt(&run)
            .await
            .unwrap()
            .expect("occupied closed slot");
        Entry {
            id,
            claim,
            attempt,
            grant,
        }
    }

    async fn work_row(&self, id: &SessionId) -> WorkRow {
        work_row_on(&self.work, id).await
    }

    async fn cleanup(self) {
        self.d.close().await;
        self.roots.close().await;
        self.work.close().await;
        self.observer.close().await;
    }
}

#[cfg(test)]
async fn work_row_on<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    id: &SessionId,
) -> WorkRow {
    sqlx::query_as("SELECT state,lease_owner,lease_epoch,lease_expires_ms,lease_refreshed_ms FROM work_queue_item WHERE data_type='session' AND data_id=$1")
        .bind(id.as_str()).fetch_one(executor).await.unwrap()
}

struct Entry {
    id: SessionId,
    claim: RunClaim,
    attempt: PhysicalAttempt,
    grant: SessionWorkGrant,
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly admitted PostgreSQL service fixture"]
async fn live_postgres_cap_one_and_partial_owner_matrix() {
    // Cause graph: C1 Dispatch cap=1; C2 Root borrows D; C3 Work borrows D;
    // C4 the fresh exact physical attempt is closed+model-only. Effects: E1
    // every valid topology releases its OWN Work without another D checkout;
    // E2 independent sources are read through their own ledger, never D tables.
    // | Rule | C2 | C3 | persistence                | result |
    // | P1   | T  | T  | one D transaction          | E1     |
    // | P2   | T  | F  | borrowed Root, own Work    | E1+E2  |
    // | P3   | F  | T  | own Root, borrowed Work    | E1+E2  |
    // | P4   | F  | F  | existing independent path  | E1+E2  |
    // Independent schemas deliberately contain different ledgers: a wrong-D
    // read cannot accidentally see the real Root/Work. No operation opens a pool.
    for (root_shared, work_shared) in [(true, true), (true, false), (false, true), (false, false)] {
        let f = Fixture::new(1, root_shared, work_shared).await;
        let e = f.entry().await;
        let before = f.work_row(&e.id).await;
        assert_eq!(before.0, "active");
        assert!(
            tokio::time::timeout(PROGRESS, f.release().release(&e.claim, &e.attempt, true))
                .await
                .expect("P1-P4 must progress with the original one-connection cap")
                .unwrap()
        );
        let after = f.work_row(&e.id).await;
        assert_eq!((after.0.as_str(), after.1.as_deref()), ("stopped", None));
        assert_eq!(after.2, before.2, "release cannot mint a new Work epoch");
        let backends: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name=$1")
                .bind(&f.tag)
                .fetch_one(&f.observer)
                .await
                .unwrap();
        assert_eq!(backends, 1, "one ordinary D backend, no hidden pool growth");
        f.cleanup().await;
    }
}

#[cfg(test)]
#[tokio::test]
#[ignore = "requires an explicitly admitted PostgreSQL service fixture"]
async fn live_postgres_borrowed_primitives_preserve_the_callers_transaction() {
    // B1 one checked-out D connection contains a sentinel and exact claim;
    // B2 Root read, Work acquire and Work release all borrow it. E1 txid stays
    // identical (no nested transaction/check-out ROLLBACK); E2 explicit caller
    // rollback removes the sentinel AND every Work mutation. These observations
    // distinguish the borrowed path from a second checkout even with spare caps.
    let f = Fixture::new(1, true, true).await;
    let e = f.entry().await;
    let before = f.work_row(&e.id).await;
    f.d.execute("CREATE TABLE parking_sentinel(id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    let mut tx = f.d.begin().await.unwrap();
    tx.execute("INSERT INTO parking_sentinel VALUES(1)")
        .await
        .unwrap();
    let txid: i64 = sqlx::query_scalar("SELECT txid_current()::bigint")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    awaken_dispatch::lock_running_claim_for_commit(&mut tx, &e.claim)
        .await
        .unwrap()
        .expect("held fresh claim");
    let root = awaken_session_store::postgres_session_root_on(&mut tx, &e.id)
        .await
        .unwrap();
    assert_eq!(root.session_id, e.id);
    f.clock.advance(1_000);
    let (grant, completed) = awaken_work_store::acquire_postgres_session_work_on(
        &mut tx,
        f.clock.as_ref(),
        ENV,
        &e.id,
        OWNER,
    )
    .await
    .unwrap();
    assert!(completed);
    assert!(
        awaken_work_store::release_postgres_session_work_on(
            &mut tx,
            f.clock.as_ref(),
            &grant.unwrap()
        )
        .await
        .unwrap()
    );
    let still: i64 = sqlx::query_scalar("SELECT txid_current()::bigint")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(still, txid, "B1+B2 retain one actual PG transaction");
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM parking_sentinel")
        .fetch_one(&f.d)
        .await
        .unwrap();
    assert_eq!(count, 0, "caller rollback still owns the sentinel");
    assert_eq!(
        f.work_row(&e.id).await,
        before,
        "caller rollback owns Work renewal and release"
    );
    f.cleanup().await;
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly admitted PostgreSQL service fixture"]
async fn live_postgres_cap_two_serializes_duplicate_release_and_claim_takeover() {
    // C1 cap2 and two duplicate requests; C2 first request is blocked INSIDE
    // the Work UPDATE, while its D row guard must still be held; C3 a second
    // Coordinator competes to cancel that claim. Effects: E1 both
    // ordinary D backends stay within cap2; E2 duplicate and cancellation wait on
    // the SAME fenced D row until Work commits; E3 exactly one release applies,
    // Work is Stopped before the competitor can acquire that row.
    // R1 C1+C2 => two D lock waiters (Work gate + D row), no extra checkout.
    // R2 R1+C3 => competing cancellation remains a real PG row-lock waiter.
    // R3 unlock => first release commits, duplicate fences/idempotently refuses,
    // then competing cancellation succeeds. No scheduler sleep is correctness evidence.
    let f = Fixture::new(2, true, true).await;
    let e = f.entry().await;
    let key = i64::from_be_bytes(uuid::Uuid::now_v7().as_bytes()[8..].try_into().unwrap());
    let mut gate = sqlx::PgConnection::connect(&f.d_url).await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(key)
        .execute(&mut gate)
        .await
        .unwrap();
    f.d.execute(format!("CREATE FUNCTION parking_stop_gate() RETURNS trigger LANGUAGE plpgsql AS $$         BEGIN PERFORM pg_advisory_xact_lock({key}); RETURN NEW; END $$").as_str()).await.unwrap();
    f.d.execute("CREATE TRIGGER parking_stop_gate BEFORE UPDATE ON work_queue_item         FOR EACH ROW WHEN (NEW.state='stopped' AND OLD.state='active') EXECUTE FUNCTION parking_stop_gate()")
        .await.unwrap();
    let port = f.release();
    let claim = e.claim.clone();
    let observed = e.attempt.clone();
    let first = tokio::spawn(async move { port.release(&claim, &observed, true).await });
    awaken_reliability_testkit::wait_for_postgres_lock_waiters(
        &f.observer,
        &f.tag,
        1,
        Some("UPDATE work_queue_item"),
        PROGRESS,
    )
    .await
    .unwrap();
    let port = f.release();
    let claim = e.claim.clone();
    let observed = e.attempt.clone();
    let duplicate = tokio::spawn(async move { port.release(&claim, &observed, true).await });
    awaken_reliability_testkit::wait_for_postgres_lock_waiters(
        &f.observer,
        &f.tag,
        2,
        None,
        PROGRESS,
    )
    .await
    .unwrap();
    let ordinary: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name=$1")
            .bind(&f.tag)
            .fetch_one(&f.observer)
            .await
            .unwrap();
    assert_eq!(
        ordinary, 2,
        "E1 no product pool was opened around the ceiling"
    );
    let rival = Arc::new(
        PostgresDispatchStore::with_existing_pool(f.observer.clone())
            .await
            .unwrap(),
    );
    let run = e.claim.run_id.clone();
    let cancellation = tokio::spawn(async move { rival.cancel(&run).await });
    awaken_reliability_testkit::wait_for_postgres_lock_waiters(
        &f.observer,
        &format!("{}_observer", f.tag),
        1,
        Some("runtime_dispatch"),
        PROGRESS,
    )
    .await
    .unwrap();
    assert!(
        !first.is_finished() && !duplicate.is_finished() && !cancellation.is_finished(),
        "E2 the actual database locks, not time, order the effects"
    );
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut gate)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(PROGRESS, first)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "R3 first applied"
    );
    assert!(
        !tokio::time::timeout(PROGRESS, duplicate)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "R3 no second release"
    );
    assert!(
        tokio::time::timeout(PROGRESS, cancellation)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_some(),
        "R3 competing cancellation after commit"
    );
    assert_eq!(
        f.work_row(&e.id).await.0,
        "stopped",
        "E3 Work commit precedes guard release"
    );
    gate.close().await.unwrap();
    f.cleanup().await;
}

#[cfg(test)]
#[tokio::test]
#[ignore = "requires an explicitly admitted PostgreSQL service fixture"]
async fn live_postgres_rejects_stale_claim_and_changed_physical_evidence() {
    // C1 caller claim owner/epoch drift; C2 observed physical owner/epoch/close
    // differ from the FRESH locked row; C3 non-model footprint. E1 every
    // mismatch refuses without Work mutation. R1 owner drift, R2 epoch drift,
    // R3 physical owner, R4 physical epoch, R5 open local entry, R6 non-model.
    // A final current request succeeds, proving failures were exact fences,
    // not a broken fixture. Other kernel shapes remain in its exhaustive tests.
    let f = Fixture::new(1, true, true).await;
    let e = f.entry().await;
    let before = f.work_row(&e.id).await;
    let port = f.release();
    for (rule, foreign_owner) in [("R1", true), ("R2", false)] {
        let mut claim = e.claim.clone();
        if foreign_owner {
            claim.owner = "foreign".into();
        } else {
            claim.epoch = awaken_lease_contract::fence::Epoch::from_persisted(claim.epoch)
                .unwrap()
                .next()
                .unwrap()
                .get();
        }
        assert!(
            !port.release(&claim, &e.attempt, true).await.unwrap(),
            "{rule}"
        );
        assert_eq!(f.work_row(&e.id).await, before, "{rule} no effect");
    }
    for rule in ["R3", "R4", "R5"] {
        let mut observed = e.attempt.clone();
        match rule {
            "R3" => observed.claim.owner = "foreign".into(),
            "R4" => {
                observed.claim.epoch =
                    awaken_lease_contract::fence::Epoch::from_persisted(observed.claim.epoch)
                        .unwrap()
                        .next()
                        .unwrap()
                        .get()
            }
            _ => observed.closing = false,
        }
        assert!(
            !port.release(&e.claim, &observed, true).await.unwrap(),
            "{rule}"
        );
        assert_eq!(f.work_row(&e.id).await, before, "{rule} no effect");
    }
    assert!(
        !port.release(&e.claim, &e.attempt, false).await.unwrap(),
        "R6"
    );
    // The caller kept the original closed observation. A concurrent, persisted
    // slot-owner change must be re-read under the claim fence, not replaced by
    // a second load while the one connection is already held. Only our fixture
    // row is fault-injected; no executing task or claimed stop is fabricated.
    sqlx::query("UPDATE runtime_dispatch SET active_attempt_owner=$1 WHERE run_id=$2")
        .bind("replacement-physical-owner")
        .bind(&e.claim.run_id.0)
        .execute(&f.d)
        .await
        .unwrap();
    assert!(
        !port.release(&e.claim, &e.attempt, true).await.unwrap(),
        "R7 fresh physical snapshot wins"
    );
    assert_eq!(f.work_row(&e.id).await, before, "R7 no Work effect");
    sqlx::query("UPDATE runtime_dispatch SET active_attempt_owner=$1 WHERE run_id=$2")
        .bind(&e.attempt.claim.owner)
        .bind(&e.claim.run_id.0)
        .execute(&f.d)
        .await
        .unwrap();
    // R7b additional parking evidence is unreadable, but the CURRENT ordinary
    // claim fence still owns commit/settle. Decode failure => guard admitted,
    // parking refused, original Work unchanged; SQL/claim faults remain errors.
    // Explicitly release the guard before reusing this capacity-one pool.
    assert!(e.attempt.binding.is_none(), "R7b original unbound fixture");
    sqlx::query("UPDATE runtime_dispatch SET active_attempt_binding='not-json' WHERE run_id=$1")
        .bind(&e.claim.run_id.0)
        .execute(&f.d)
        .await
        .unwrap();
    let guard = f
        .dispatch
        .lock_commit_epoch(&e.claim)
        .await
        .unwrap()
        .expect("R7b ordinary current claim remains guarded");
    guard.release().await.unwrap();
    assert!(
        !port.release(&e.claim, &e.attempt, true).await.unwrap(),
        "R7b unreadable physical details cannot authorize parking"
    );
    assert_eq!(f.work_row(&e.id).await, before, "R7b no Work effect");
    sqlx::query("UPDATE runtime_dispatch SET active_attempt_binding=NULL WHERE run_id=$1")
        .bind(&e.claim.run_id.0)
        .execute(&f.d)
        .await
        .unwrap();
    // An exact physical slot cannot itself be relinquished (the kernel fences
    // that). First a canonical cancel revokes the live lease while retaining
    // its closed physical predecessor; the same owner claims its successor.
    assert!(f.dispatch.cancel(&e.claim.run_id).await.unwrap().is_some());
    let successor = f
        .dispatch
        .claim(OWNER, 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let successor = RunClaim::from(&successor.lease);
    assert!(
        successor.epoch > e.claim.epoch,
        "R8 real same-owner successor"
    );
    assert_eq!(
        f.dispatch.retry_claim(&successor, 0, 8).await.unwrap(),
        awaken_dispatch::SettleOutcome::Applied
    );
    let current = f
        .dispatch
        .claim(OWNER, 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let current = RunClaim::from(&current.lease);
    assert!(
        current.epoch > successor.epoch,
        "R8 canonical retry/reclaim advanced the epoch"
    );
    assert!(
        !port.release(&e.claim, &e.attempt, true).await.unwrap(),
        "R8 old original lease"
    );
    assert!(
        !port.release(&successor, &e.attempt, true).await.unwrap(),
        "R8 old retried lease"
    );
    assert_eq!(
        f.work_row(&e.id).await,
        before,
        "R8 neither old Work release applied"
    );
    assert_eq!(
        f.dispatch
            .load_physical_attempt(&e.claim.run_id)
            .await
            .unwrap(),
        Some(e.attempt.clone()),
        "R8 logical claim transitions never clear the old physical owner"
    );
    assert!(
        port.release(&current, &e.attempt, true).await.unwrap(),
        "current claim and exact predecessor evidence"
    );
    f.cleanup().await;
}

#[cfg(test)]
#[tokio::test]
#[ignore = "requires an explicitly admitted PostgreSQL service fixture"]
async fn live_postgres_work_release_rollback_and_epoch_owner_fences() {
    // W1 release-side storage fault after renewal => the ORIGINAL Work state,
    // epoch and expiry all roll back; D guard/physical slot also survive. W2 a
    // foreign live Work owner => no release. W3 an old grant after a legitimate
    // stop+wake+new epoch => rejected by the SAME borrowed release primitive;
    // W4 the new exact grant releases. Neither fault is hidden by a retry.
    let f = Fixture::new(1, true, true).await;
    let e = f.entry().await;
    let before = f.work_row(&e.id).await;
    f.clock.advance(1_000);
    f.d.execute("CREATE FUNCTION parking_reject_stop() RETURNS trigger LANGUAGE plpgsql AS $$         BEGIN RAISE EXCEPTION 'owned parking stop failure'; END $$").await.unwrap();
    f.d.execute("CREATE TRIGGER parking_reject_stop BEFORE UPDATE ON work_queue_item         FOR EACH ROW WHEN (NEW.state='stopped') EXECUTE FUNCTION parking_reject_stop()")
        .await.unwrap();
    assert!(
        f.release()
            .release(&e.claim, &e.attempt, true)
            .await
            .is_err(),
        "W1 storage failure remains loud"
    );
    assert_eq!(
        f.work_row(&e.id).await,
        before,
        "W1 includes the preceding renewal rollback"
    );
    assert_eq!(
        f.dispatch
            .load_physical_attempt(&e.claim.run_id)
            .await
            .unwrap(),
        Some(e.attempt.clone()),
        "W1 no physical closure or replacement"
    );
    f.d.execute("DROP TRIGGER parking_reject_stop ON work_queue_item")
        .await
        .unwrap();
    sqlx::query(
        "UPDATE work_queue_item SET lease_owner='foreign' WHERE data_type='session' AND data_id=$1",
    )
    .bind(e.id.as_str())
    .execute(&f.work)
    .await
    .unwrap();
    let foreign = f.work_row(&e.id).await;
    assert!(
        !f.release()
            .release(&e.claim, &e.attempt, true)
            .await
            .unwrap(),
        "W2"
    );
    assert_eq!(
        f.work_row(&e.id).await,
        foreign,
        "W2 foreign grant untouched"
    );
    sqlx::query(
        "UPDATE work_queue_item SET lease_owner=$1 WHERE data_type='session' AND data_id=$2",
    )
    .bind(OWNER)
    .bind(e.id.as_str())
    .execute(&f.work)
    .await
    .unwrap();
    assert!(f.assignments.release_session_work(&e.grant).await.unwrap());
    let renewed = match f
        .assignments
        .acquire_session_work(&e.id, OWNER, SessionWorkAcquisition::ClaimedRun)
        .await
        .unwrap()
    {
        SessionWorkOwnership::Leased(grant) => grant,
        _ => panic!("new Work epoch"),
    };
    let newer = f.work_row(&e.id).await;
    assert!(
        newer.2 > before.2,
        "W3 a real canonical reacquisition advanced the epoch"
    );
    let mut tx = f.d.begin().await.unwrap();
    assert!(
        !awaken_work_store::release_postgres_session_work_on(
            &mut tx,
            f.clock.as_ref(),
            &e.grant.to_transfer()
        )
        .await
        .unwrap(),
        "W3 old Work epoch cannot release a same-owner successor"
    );
    tx.rollback().await.unwrap();
    assert_eq!(f.work_row(&e.id).await, newer, "W3 successor preserved");
    let mut tx = f.d.begin().await.unwrap();
    assert!(
        awaken_work_store::release_postgres_session_work_on(
            &mut tx,
            f.clock.as_ref(),
            &renewed.to_transfer()
        )
        .await
        .unwrap(),
        "W4 exact current grant"
    );
    tx.commit().await.unwrap();
    assert_eq!(f.work_row(&e.id).await.0, "stopped");
    f.cleanup().await;
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly admitted PostgreSQL service fixture"]
async fn live_postgres_shared_root_and_separate_work_database() {
    // Cause/effect rules for actual partial sharing, beyond the schema matrix:
    // DB1 Root shares capacity-one D, Work is in another DATABASE => read the
    // original Root on D and the original Work through its independent port.
    // DB2 foreign Work owner OR old same-owner epoch => refuse that Work CAS.
    // DB3 Work stop is inside its local transaction and blocked on our lock =>
    // the independent Work port is still running AND D's claim row remains
    // locked. DB4 release barrier => Work commits before competing cancellation
    // can acquire D. No cross-database atomic rollback is asserted: D protects
    // the claim until the original Work authority finishes its own fenced CAS.
    let tag = format!("park_{}", uuid::Uuid::now_v7().simple());
    let (d, d_url) = owned_pool(1, &tag).await;
    let namespace: String = sqlx::query_scalar("SELECT current_schema()")
        .fetch_one(&d)
        .await
        .unwrap();
    assert!(
        namespace.starts_with("awaken_fixture_"),
        "DB1 admitted fixture namespace"
    );
    // The database name derives from the reliability fixture's unique owned
    // namespace. This case creates/drops only that name; no shared DB is reset.
    let database = format!("{namespace}_work");
    assert!(database.len() <= 63);
    assert!(
        database
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    );
    eprintln!("owned split-Work database: {database}; retained on test failure");
    d.execute(format!("CREATE DATABASE {database}").as_str())
        .await
        .unwrap();
    let work_tag = format!("{tag}_work");
    let work_options = PgConnectOptions::from_str(&d_url)
        .unwrap()
        .database(&database)
        .options([("search_path", "public")])
        .application_name(&work_tag);
    let work = pool_with_options(work_options.clone(), 1).await;
    let f = Fixture::from_sources(d.clone(), d_url, d, work, true, false, tag).await;
    let e = f.entry().await;
    let original = f.work_row(&e.id).await;
    let actual_work_database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&f.work)
        .await
        .unwrap();
    let dispatch_database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&f.d)
        .await
        .unwrap();
    assert_eq!(
        actual_work_database, database,
        "DB1 actual separate database"
    );
    assert_ne!(actual_work_database, dispatch_database);
    let wrong_work: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('work_queue_item')::text")
            .fetch_one(&f.d)
            .await
            .unwrap();
    assert!(
        wrong_work.is_none(),
        "DB1 D cannot accidentally supply Work"
    );
    sqlx::query(
        "UPDATE work_queue_item SET lease_owner='foreign' WHERE data_type='session' AND data_id=$1",
    )
    .bind(e.id.as_str())
    .execute(&f.work)
    .await
    .unwrap();
    let foreign = f.work_row(&e.id).await;
    assert!(
        !f.release()
            .release(&e.claim, &e.attempt, true)
            .await
            .unwrap(),
        "DB2 foreign independent Work owner"
    );
    assert_eq!(
        f.work_row(&e.id).await,
        foreign,
        "DB2 no foreign Work mutation"
    );
    sqlx::query(
        "UPDATE work_queue_item SET lease_owner=$1 WHERE data_type='session' AND data_id=$2",
    )
    .bind(OWNER)
    .bind(e.id.as_str())
    .execute(&f.work)
    .await
    .unwrap();
    assert!(f.assignments.release_session_work(&e.grant).await.unwrap());
    let current = f
        .assignments
        .acquire_session_work(&e.id, OWNER, SessionWorkAcquisition::ClaimedRun)
        .await
        .unwrap();
    assert!(matches!(current, SessionWorkOwnership::Leased(_)));
    let newer = f.work_row(&e.id).await;
    assert!(
        newer.2 > original.2,
        "DB2 canonical same-owner Work successor"
    );
    assert!(
        !f.assignments.release_session_work(&e.grant).await.unwrap(),
        "DB2 old independent Work epoch"
    );
    assert_eq!(f.work_row(&e.id).await, newer, "DB2 new Work preserved");
    let work_observer = pool_with_options(
        work_options
            .clone()
            .application_name(&format!("{}_observer", work_tag)),
        2,
    )
    .await;
    let mut gate = sqlx::PgConnection::connect_with(
        &work_options.application_name(&format!("{}_gate", work_tag)),
    )
    .await
    .unwrap();
    let key = i64::from_be_bytes(uuid::Uuid::now_v7().as_bytes()[8..].try_into().unwrap());
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(key)
        .execute(&mut gate)
        .await
        .unwrap();
    f.work.execute(format!("CREATE FUNCTION parking_stop_gate() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({key}); RETURN NEW; END $$").as_str()).await.unwrap();
    f.work.execute("CREATE TRIGGER parking_stop_gate BEFORE UPDATE ON work_queue_item FOR EACH ROW WHEN (NEW.state='stopped' AND OLD.state='active') EXECUTE FUNCTION parking_stop_gate()")
        .await.unwrap();
    let port = f.release();
    let claim = e.claim.clone();
    let observed = e.attempt.clone();
    let parking = tokio::spawn(async move { port.release(&claim, &observed, true).await });
    awaken_reliability_testkit::wait_for_postgres_lock_waiters(
        &work_observer,
        &work_tag,
        1,
        Some("UPDATE work_queue_item"),
        PROGRESS,
    )
    .await
    .unwrap();
    let rival = PostgresDispatchStore::with_existing_pool(f.observer.clone())
        .await
        .unwrap();
    let run = e.claim.run_id.clone();
    let competing = tokio::spawn(async move { rival.cancel(&run).await });
    awaken_reliability_testkit::wait_for_postgres_lock_waiters(
        &f.observer,
        &format!("{}_observer", f.tag),
        1,
        Some("runtime_dispatch"),
        PROGRESS,
    )
    .await
    .unwrap();
    assert!(
        !parking.is_finished() && !competing.is_finished(),
        "DB3 Work barrier retains D's exact claim fence"
    );
    let visible = work_row_on(&work_observer, &e.id).await;
    assert_eq!(
        visible, newer,
        "DB3 no Work stop before its own CAS finishes"
    );
    let d_connections: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name=$1")
            .bind(&f.tag)
            .fetch_one(&f.observer)
            .await
            .unwrap();
    let work_connections: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name=$1")
            .bind(&work_tag)
            .fetch_one(&work_observer)
            .await
            .unwrap();
    assert_eq!(
        (d_connections, work_connections),
        (1, 1),
        "DB3 original product pools retain their capacity-one ceilings"
    );
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(key)
        .execute(&mut gate)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(PROGRESS, parking)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "DB4 exact current independent Work release"
    );
    assert!(
        tokio::time::timeout(PROGRESS, competing)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_some(),
        "DB4 competing cancellation only after Work CAS"
    );
    // Cleanup K1: the final pool-backed SELECT's automatic Drop can still be
    // returning through SQLx 0.8.6's asynchronous ping when Pool::close starts.
    // A real failed run left closed=true/size=1/idle=1 and one idle backend
    // holding exactly that SELECT. Own the final lease explicitly, reuse the
    // same reader, then close the lease before closing the pool. K2 requires
    // size=0 and the ordinary DROP to succeed; no retry/force/longer deadline
    // may hide an active effect or an unclosed fixture connection.
    let mut final_work_read = f.work.acquire().await.unwrap();
    let after = work_row_on(&mut *final_work_read, &e.id).await;
    assert_eq!(
        (after.0.as_str(), after.1.as_deref(), after.2),
        ("stopped", None, newer.2)
    );
    final_work_read.close().await.unwrap();
    assert_eq!(f.work.size(), 0, "K1 final Work read is explicitly closed");
    gate.close().await.unwrap();
    work_observer.close().await;
    f.work.close().await;
    sqlx::query(&format!("DROP DATABASE {database}"))
        .execute(&f.observer)
        .await
        .unwrap();
    f.cleanup().await;
}
