//! Draft evidence: real transactions, durable reopen and separate-process restart.
#![cfg(feature = "test-support")]
use awaken_environment_contract::EnvironmentRevision;
use awaken_executable_environment_contract::{
    EnvironmentWorkLifecycle, EnvironmentWorkRevisionError,
    EnvironmentWorkRevisionOperation as Operation, EnvironmentWorkRevisionOutcome as Outcome,
    SessionAccessAuthenticator, SessionWorkStore, WorkConsumer, WorkQueries,
};
use awaken_work_store::{PostgresWorkQueue, SqliteWorkQueue, test_session_access_digest};
#[path = "support/work_postgres.rs"]
mod work_postgres_fixture;

#[tokio::test]
async fn sqlite_registration_watermark_and_queue_rollback_together() {
    // F1 healthcheck insertion fails; F2 watermark insertion fails after queue
    // insert; F3/F4 watermark update fails after Cloud/Withdraw deletes rows.
    // No prefix may survive. Reopen and exact retry must return Applied, never
    // AlreadyApplied for an uncommitted watermark. Session bearer survives F3.
    for row in [
        "healthcheck_insert",
        "watermark_insert",
        "cloud_update",
        "withdraw_update",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("work.db");
        let q = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        let session = q
            .enqueue_session("rollback", &awaken_session_types::SessionId::from("frozen"))
            .await
            .unwrap();
        let digest = test_session_access_digest(709);
        q.claim_with_session_access("rollback", "owner", "poller", None, &digest)
            .await
            .unwrap()
            .unwrap();
        let proof = q
            .authenticate_session_access(&digest)
            .await
            .unwrap()
            .unwrap();
        let existing = row.ends_with("update");
        if existing {
            q.apply_environment_revision(
                "rollback",
                EnvironmentRevision(1),
                Operation::Register { healthcheck: true },
            )
            .await
            .unwrap();
        }
        let before: Vec<_> = q
            .list("rollback")
            .await
            .unwrap()
            .iter()
            .map(|item| format!("{item:?}"))
            .collect();
        let before_watermark: Option<(String,String)> = sqlx::query_as("SELECT revision, operation FROM work_queue_environment_revision WHERE environment_id='rollback'")
            .fetch_optional(&pool).await.unwrap();
        let event = if row == "healthcheck_insert" {
            "INSERT ON work_queue_item WHEN NEW.data_type='healthcheck'"
        } else if row == "watermark_insert" {
            "INSERT ON work_queue_environment_revision"
        } else {
            "UPDATE ON work_queue_environment_revision"
        };
        sqlx::raw_sql(&format!("CREATE TRIGGER revision_failure BEFORE {event} BEGIN SELECT RAISE(ABORT,'injected revision rollback'); END;"))
            .execute(&pool).await.unwrap();
        let operation = match row {
            "cloud_update" => Operation::Register { healthcheck: false },
            "withdraw_update" => Operation::Withdraw,
            _ => Operation::Register { healthcheck: true },
        };
        let revision = EnvironmentRevision(if existing { 2 } else { 1 });
        let failure = q
            .apply_environment_revision("rollback", revision, operation)
            .await
            .unwrap_err();
        assert!(
            matches!(&failure, EnvironmentWorkRevisionError::Storage(_)),
            "{failure:?}"
        );
        assert!(failure.to_string().contains("injected revision rollback"));
        assert_eq!(
            q.list("rollback")
                .await
                .unwrap()
                .iter()
                .map(|item| format!("{item:?}"))
                .collect::<Vec<_>>(),
            before,
            "{row} queue rollback"
        );
        assert_eq!(
            q.authenticate_session_access(&digest).await.unwrap(),
            Some(proof.clone()),
            "{row} bearer rollback"
        );
        let after_watermark: Option<(String,String)> = sqlx::query_as("SELECT revision, operation FROM work_queue_environment_revision WHERE environment_id='rollback'")
            .fetch_optional(&pool).await.unwrap();
        assert_eq!(
            after_watermark, before_watermark,
            "{row} watermark rollback"
        );
        sqlx::raw_sql("DROP TRIGGER revision_failure")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        drop(q);
        let q = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap();
        assert_eq!(
            q.apply_environment_revision("rollback", revision, operation)
                .await
                .unwrap(),
            Outcome::Applied,
            "{row} exact retry"
        );
        assert_eq!(
            q.apply_environment_revision("rollback", revision, operation)
                .await
                .unwrap(),
            Outcome::AlreadyApplied
        );
        match operation {
            Operation::Withdraw | Operation::Register { healthcheck: false } => {
                assert_eq!(
                    q.list("rollback")
                        .await
                        .unwrap()
                        .iter()
                        .map(|item| item.id.as_str())
                        .collect::<Vec<_>>(),
                    [session.as_str()]
                );
                assert_eq!(
                    q.authenticate_session_access(&digest).await.unwrap(),
                    Some(proof)
                );
            }
            _ => assert_eq!(q.list("rollback").await.unwrap().len(), 2),
        }
        drop(q);
    }
}

#[tokio::test]
async fn postgres_registration_watermark_and_queue_rollback_together() {
    // F1 healthcheck insertion fails; F2 watermark insertion fails after queue
    // insert; F3/F4 watermark update fails after Cloud/Withdraw deletes rows.
    // No prefix may survive. Reopen and exact retry must return Applied, never
    // AlreadyApplied for an uncommitted watermark. Session bearer survives F3.
    for row in [
        "healthcheck_insert",
        "watermark_insert",
        "cloud_update",
        "withdraw_update",
    ] {
        let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
            return;
        };
        let fixture = work_postgres_fixture::WorkPostgresFixture::new(&url, 4).await;
        let pool = fixture.pool.clone();
        let q = PostgresWorkQueue::with_pool(pool.clone(), awaken_time::system_clock())
            .await
            .unwrap();
        let session = q
            .enqueue_session("rollback", &awaken_session_types::SessionId::from("frozen"))
            .await
            .unwrap();
        let digest = test_session_access_digest(709);
        q.claim_with_session_access("rollback", "owner", "poller", None, &digest)
            .await
            .unwrap()
            .unwrap();
        let proof = q
            .authenticate_session_access(&digest)
            .await
            .unwrap()
            .unwrap();
        let existing = row.ends_with("update");
        if existing {
            q.apply_environment_revision(
                "rollback",
                EnvironmentRevision(1),
                Operation::Register { healthcheck: true },
            )
            .await
            .unwrap();
        }
        let before: Vec<_> = q
            .list("rollback")
            .await
            .unwrap()
            .iter()
            .map(|item| format!("{item:?}"))
            .collect();
        let before_watermark: Option<(String,String)> = sqlx::query_as("SELECT revision, operation FROM work_queue_environment_revision WHERE environment_id='rollback'")
            .fetch_optional(&pool).await.unwrap();
        let table = if row == "healthcheck_insert" {
            "work_queue_item"
        } else {
            "work_queue_environment_revision"
        };
        let event = if row.ends_with("insert") {
            "INSERT"
        } else {
            "UPDATE"
        };
        let condition = if row == "healthcheck_insert" {
            "WHEN (NEW.data_type='healthcheck')"
        } else {
            ""
        };
        sqlx::raw_sql(&format!("CREATE FUNCTION fail_revision() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected revision rollback'; END; $$; CREATE TRIGGER revision_failure BEFORE {event} ON {table} FOR EACH ROW {condition} EXECUTE FUNCTION fail_revision();"))
            .execute(&pool).await.unwrap();
        let operation = match row {
            "cloud_update" => Operation::Register { healthcheck: false },
            "withdraw_update" => Operation::Withdraw,
            _ => Operation::Register { healthcheck: true },
        };
        let revision = EnvironmentRevision(if existing { 2 } else { 1 });
        let failure = q
            .apply_environment_revision("rollback", revision, operation)
            .await
            .unwrap_err();
        assert!(
            matches!(&failure, EnvironmentWorkRevisionError::Storage(_)),
            "{failure:?}"
        );
        assert!(failure.to_string().contains("injected revision rollback"));
        assert_eq!(
            q.list("rollback")
                .await
                .unwrap()
                .iter()
                .map(|item| format!("{item:?}"))
                .collect::<Vec<_>>(),
            before,
            "{row} queue rollback"
        );
        assert_eq!(
            q.authenticate_session_access(&digest).await.unwrap(),
            Some(proof.clone()),
            "{row} bearer rollback"
        );
        let after_watermark: Option<(String,String)> = sqlx::query_as("SELECT revision, operation FROM work_queue_environment_revision WHERE environment_id='rollback'")
            .fetch_optional(&pool).await.unwrap();
        assert_eq!(
            after_watermark, before_watermark,
            "{row} watermark rollback"
        );
        sqlx::raw_sql(&format!(
            "DROP TRIGGER revision_failure ON {table}; DROP FUNCTION fail_revision();"
        ))
        .execute(&pool)
        .await
        .unwrap();
        drop(q);
        let q = PostgresWorkQueue::with_pool(pool.clone(), awaken_time::system_clock())
            .await
            .unwrap();
        assert_eq!(
            q.apply_environment_revision("rollback", revision, operation)
                .await
                .unwrap(),
            Outcome::Applied,
            "{row} exact retry"
        );
        assert_eq!(
            q.apply_environment_revision("rollback", revision, operation)
                .await
                .unwrap(),
            Outcome::AlreadyApplied
        );
        match operation {
            Operation::Withdraw | Operation::Register { healthcheck: false } => {
                assert_eq!(
                    q.list("rollback")
                        .await
                        .unwrap()
                        .iter()
                        .map(|item| item.id.as_str())
                        .collect::<Vec<_>>(),
                    [session.as_str()]
                );
                assert_eq!(
                    q.authenticate_session_access(&digest).await.unwrap(),
                    Some(proof)
                );
            }
            _ => assert_eq!(q.list("rollback").await.unwrap().len(), 2),
        }
        drop(q);
        fixture.cleanup().await;
    }
}

async fn restart_phase<Q: awaken_store_conformance::work_queue::WorkBackend>(q: &Q, phase: &str) {
    match phase {
        "seed" => {
            q.apply_environment_revision(
                "restart",
                EnvironmentRevision(6),
                Operation::Register { healthcheck: true },
            )
            .await
            .unwrap();
            q.apply_environment_revision("restart", EnvironmentRevision(8), Operation::Withdraw)
                .await
                .unwrap();
        }
        "assert" => {
            assert_eq!(
                q.apply_environment_revision(
                    "restart",
                    EnvironmentRevision(7),
                    Operation::Register { healthcheck: true }
                )
                .await
                .unwrap(),
                Outcome::Superseded
            );
            assert_eq!(
                q.apply_environment_revision(
                    "restart",
                    EnvironmentRevision(8),
                    Operation::Withdraw
                )
                .await
                .unwrap(),
                Outcome::AlreadyApplied
            );
            assert!(q.ensure_healthcheck("restart").await.is_err());
            assert!(q.list("restart").await.unwrap().is_empty());
        }
        _ => panic!("invalid private restart phase"),
    }
}

#[tokio::test]
#[ignore = "requires the parent-owned restart fixture"]
async fn environment_revision_restart_child() {
    let kind = std::env::var("AWAKEN_WORK_REVISION_RESTART_KIND")
        .expect("restart child requires its parent-owned fixture");
    let endpoint = std::env::var("AWAKEN_WORK_REVISION_RESTART_ENDPOINT").unwrap();
    let phase = std::env::var("AWAKEN_WORK_REVISION_RESTART_PHASE").unwrap();
    match kind.as_str() {
        "sqlite" => {
            restart_phase(
                &SqliteWorkQueue::open(&endpoint, awaken_time::system_clock())
                    .await
                    .unwrap(),
                &phase,
            )
            .await
        }
        "postgres" => {
            restart_phase(
                &PostgresWorkQueue::connect(&endpoint, awaken_time::system_clock())
                    .await
                    .unwrap(),
                &phase,
            )
            .await
        }
        _ => panic!("invalid private restart backend"),
    }
    eprintln!("environment revision restart {kind}/{phase} completed");
}

fn run_restart_children(kind: &str, endpoint: &str) {
    for phase in ["seed", "assert"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "environment_revision_restart_child",
                "--ignored",
                "--nocapture",
            ])
            .env("AWAKEN_WORK_REVISION_RESTART_KIND", kind)
            .env("AWAKEN_WORK_REVISION_RESTART_ENDPOINT", endpoint)
            .env("AWAKEN_WORK_REVISION_RESTART_PHASE", phase);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            child.creation_flags(0x08000000);
        }
        let output = child.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
        let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
        assert!(
            stdout
                .lines()
                .any(|line| line == "test environment_revision_restart_child ... ok"),
            "{kind}/{phase}: exact restart child must actually run: {stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
            "{kind}/{phase}: zero-selected/ignored child cannot prove restart: {stdout}\n{stderr}"
        );
        assert!(
            stderr.lines().any(
                |line| line == format!("environment revision restart {kind}/{phase} completed")
            ),
            "{kind}/{phase}: selected child must finish its actual storage assertions: {stdout}\n{stderr}"
        );
        assert!(
            output.status.success(),
            "{kind}/{phase}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn sqlite_withdrawal_watermark_survives_a_process_restart() {
    let directory = tempfile::tempdir().unwrap();
    run_restart_children(
        "sqlite",
        directory.path().join("restart.db").to_str().unwrap(),
    );
}

#[tokio::test]
async fn postgres_withdrawal_watermark_survives_a_process_restart() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = work_postgres_fixture::WorkPostgresFixture::new(&url, 4).await;
    let endpoint = fixture.scoped_url.clone();
    tokio::task::spawn_blocking(move || run_restart_children("postgres", &endpoint))
        .await
        .unwrap();
    fixture.cleanup().await;
}

#[tokio::test]
async fn sqlite_separate_queue_instances_share_the_registration_commit_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("multi-client.db");
    let a = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let b = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    a.apply_environment_revision(
        "multi-client",
        EnvironmentRevision(1),
        Operation::Register { healthcheck: true },
    )
    .await
    .unwrap();
    let (helper, withdrawal) = tokio::join!(
        a.ensure_healthcheck("multi-client"),
        b.apply_environment_revision("multi-client", EnvironmentRevision(2), Operation::Withdraw),
    );
    let _ = helper;
    assert_eq!(withdrawal.unwrap(), Outcome::Applied);
    assert!(a.list("multi-client").await.unwrap().is_empty());
    assert!(b.ensure_healthcheck("multi-client").await.is_err());
    assert_eq!(
        a.apply_environment_revision(
            "multi-client",
            EnvironmentRevision(1),
            Operation::Register { healthcheck: true }
        )
        .await
        .unwrap(),
        Outcome::Superseded
    );
    drop((a, b));
}

#[tokio::test]
async fn postgres_separate_queue_instances_share_the_registration_commit_boundary() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = work_postgres_fixture::WorkPostgresFixture::new(&url, 4).await;
    let a = PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
        .await
        .unwrap();
    let b = PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
        .await
        .unwrap();
    a.apply_environment_revision(
        "multi-client",
        EnvironmentRevision(1),
        Operation::Register { healthcheck: true },
    )
    .await
    .unwrap();
    let (helper, withdrawal) = tokio::join!(
        a.ensure_healthcheck("multi-client"),
        b.apply_environment_revision("multi-client", EnvironmentRevision(2), Operation::Withdraw),
    );
    let _ = helper;
    assert_eq!(withdrawal.unwrap(), Outcome::Applied);
    assert!(a.list("multi-client").await.unwrap().is_empty());
    assert!(b.ensure_healthcheck("multi-client").await.is_err());
    assert_eq!(
        a.apply_environment_revision(
            "multi-client",
            EnvironmentRevision(1),
            Operation::Register { healthcheck: true }
        )
        .await
        .unwrap(),
        Outcome::Superseded
    );
    drop((a, b));
    fixture.cleanup().await;
}
