//! Exercise the actual Coordinator, SQLite transaction, router and HTTP client.
#![cfg(test)]

use super::*;
use awaken_environment_contract::{EnvItem, EnvironmentRevision};
use awaken_executable_environment_catalog::{
    ExecutableEnvironmentCatalog, HttpExecutableEnvironmentRegistrar,
    LocalExecutableEnvironmentRegistrar, executable_environment_registration_router,
};
use awaken_executable_environment_contract::{
    EnvironmentWorkLifecycle, EnvironmentWorkRevisionOperation as Operation,
    EnvironmentWorkRevisionOutcome as Outcome, WorkQueries,
};
use awaken_work_store::SqliteWorkQueue;
use std::sync::Mutex;

fn command(id: &str) -> ExecutableEnvironmentRegistration {
    ExecutableEnvironmentRegistration::new(
        EnvItem {
            id: id.into(),
            revision: EnvironmentRevision(2),
            name: id.into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: EnvironmentConfig::SelfHosted,
            sandbox_policy: None,
            archived_at: None,
            created_at: Some("2026-02-03T04:05:06.007Z".into()),
            updated_at: Some("2026-02-03T04:05:06.007Z".into()),
        },
        None,
    )
}

async fn serve(
    registrar: CoordinatorEnvironmentRegistrar,
) -> (
    HttpExecutableEnvironmentRegistrar,
    Arc<Mutex<Vec<u16>>>,
    tokio::task::JoinHandle<()>,
) {
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let observed = statuses.clone();
    let router = executable_environment_registration_router(Arc::new(registrar), "revision-test")
        .unwrap()
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let observed = observed.clone();
                async move {
                    let response = next.run(request).await;
                    observed.lock().unwrap().push(response.status().as_u16());
                    response
                }
            },
        ));
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (
        HttpExecutableEnvironmentRegistrar::new(format!("http://{address}"), "revision-test")
            .unwrap(),
        statuses,
        task,
    )
}

#[cfg(test)]
#[tokio::test]
async fn queue_revision_conflict_is_http409_without_immediate_retry() {
    // The queue's exact Withdraw fact and an incoming same-revision Register
    // disagree. This is a legitimate low-level queue port state, not an illegal
    // Control update of an archived Environment. Reach the real admission rule
    // through Coordinator and the authenticated HTTP adapter, not a fake error.
    let queue = Arc::new(
        SqliteWorkQueue::open_in_memory(awaken_time::system_clock())
            .await
            .unwrap(),
    );
    queue
        .apply_environment_revision("conflict", EnvironmentRevision(2), Operation::Withdraw)
        .await
        .unwrap();
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let registrar = CoordinatorEnvironmentRegistrar::new(
        Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog)),
        queue.clone(),
    );
    let (client, statuses, server) = serve(registrar).await;
    let result = client.register(command("conflict")).await;
    server.abort();
    assert!(
        matches!(
            result,
            Err(ExecutableEnvironmentRegistrationError::Conflict(_))
        ),
        "same-revision queue conflict must remain a permanent HTTP conflict: {result:?}"
    );
    assert_eq!(
        *statuses.lock().unwrap(),
        [409],
        "Conflict must not be retried within this request"
    );
    assert!(queue.list("conflict").await.unwrap().is_empty());
    assert_eq!(
        queue
            .apply_environment_revision("conflict", EnvironmentRevision(2), Operation::Withdraw)
            .await
            .unwrap(),
        Outcome::AlreadyApplied,
        "conflict changed the original watermark"
    );
}

#[cfg(test)]
#[tokio::test]
async fn queue_transaction_failure_is_http503_and_retryable() {
    // Fail the real SQLite watermark INSERT after healthcheck insertion. Each
    // attempt must roll back both facts; HTTP retries storage faults exactly
    // three times, then exact replay succeeds after the storage fault clears.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("work.db");
    let queue = Arc::new(
        SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER revision_failure BEFORE INSERT ON work_queue_environment_revision BEGIN SELECT RAISE(ABORT,'injected revision storage failure'); END;")
        .execute(&pool).await.unwrap();
    let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
    let registrar = CoordinatorEnvironmentRegistrar::new(
        Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog)),
        queue.clone(),
    );
    let (client, statuses, server) = serve(registrar).await;
    let failed = client.register(command("storage")).await;
    let failed_statuses = statuses.lock().unwrap().clone();
    let rows = queue.list("storage").await.unwrap();
    let watermarks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM work_queue_environment_revision")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::raw_sql("DROP TRIGGER revision_failure")
        .execute(&pool)
        .await
        .unwrap();
    let replay = client.register(command("storage")).await;
    server.abort();
    pool.close().await;
    assert!(
        matches!(
            failed,
            Err(ExecutableEnvironmentRegistrationError::Unavailable(_))
        ),
        "storage failure was misclassified: {failed:?}"
    );
    assert_eq!(failed_statuses, [503, 503, 503]);
    assert!(rows.is_empty(), "failed attempts leaked healthcheck rows");
    assert_eq!(watermarks, 0, "failed attempts advanced the watermark");
    assert!(replay.is_ok(), "exact replay did not recover: {replay:?}");
    assert_eq!(*statuses.lock().unwrap(), [503, 503, 503, 200]);
    assert_eq!(queue.list("storage").await.unwrap().len(), 1);
}

#[tokio::test]
async fn historical_catalog_success_never_advances_a_fresh_queue_and_current_retries_converge() {
    // H1 current Active / Archived x H2 old Register / Withdraw x H3 first / replay.
    // The real authenticated HTTP path must retain history but leave a fresh
    // SQLite queue untouched. A later current retry must still converge after
    // the catalog-side fact was committed before the queue existed.
    for archived in [false, true] {
        for registration in [false, true] {
            let id = format!("historical-fresh-{archived}-{registration}");
            let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
            let delegate = Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone()));
            let mut definition = command(&id).definition;
            definition.revision = EnvironmentRevision(5);
            let current = ExecutableEnvironmentRegistration::new(definition, None);
            delegate.register(current.clone()).await.unwrap();
            let withdrawal = ExecutableEnvironmentWithdrawal {
                environment_id: id.clone(),
                lifecycle_revision: EnvironmentRevision(6),
            };
            if archived {
                delegate.withdraw(withdrawal.clone()).await.unwrap();
            }
            let queue = Arc::new(
                SqliteWorkQueue::open_in_memory(awaken_time::system_clock())
                    .await
                    .unwrap(),
            );
            let (client, statuses, server) = serve(CoordinatorEnvironmentRegistrar::new(
                delegate,
                queue.clone(),
            ))
            .await;
            for delivery in ["first", "replay"] {
                if registration {
                    let historical = command(&id);
                    assert_eq!(
                        client.register(historical.clone()).await.unwrap(),
                        ExecutableEnvironmentRegistrationOutcome::RegisteredHistorical,
                        "{delivery}: immutable duplicate is still historical"
                    );
                    assert_eq!(
                        catalog.at_revision(&id, EnvironmentRevision(2)),
                        Some(historical),
                        "exact old history retained"
                    );
                } else {
                    assert_eq!(
                        client
                            .withdraw(ExecutableEnvironmentWithdrawal {
                                environment_id: id.clone(),
                                lifecycle_revision: EnvironmentRevision(3)
                            })
                            .await
                            .unwrap(),
                        ExecutableEnvironmentWithdrawalOutcome::HistoricalNoop
                    );
                }
                assert!(
                    queue.list(&id).await.unwrap().is_empty(),
                    "{delivery}: history cannot seed a healthcheck"
                );
            }
            assert_eq!(
                catalog.current(&id),
                (!archived).then_some(current.clone()),
                "current catalog truth is unchanged"
            );
            // Probe the actual queue decision, rather than infer no watermark
            // from an empty item list: a hidden historical Withdraw would make
            // this lower first write Superseded, exposing the unintended effect.
            assert_eq!(
                queue
                    .apply_environment_revision(
                        &id,
                        EnvironmentRevision(1),
                        Operation::Register { healthcheck: false }
                    )
                    .await
                    .unwrap(),
                Outcome::Applied,
                "historical success wrote no queue revision"
            );
            if archived {
                assert_eq!(
                    client.withdraw(withdrawal).await.unwrap(),
                    ExecutableEnvironmentWithdrawalOutcome::AlreadyWithdrawn
                );
                assert!(queue.list(&id).await.unwrap().is_empty());
            } else {
                assert_eq!(
                    client.register(current).await.unwrap(),
                    ExecutableEnvironmentRegistrationOutcome::AlreadyRegistered
                );
                assert_eq!(
                    queue.list(&id).await.unwrap().len(),
                    1,
                    "current duplicate repairs the interrupted queue side"
                );
            }
            assert_eq!(
                *statuses.lock().unwrap(),
                vec![200, 200, 200],
                "actual HTTP calls completed without a retry loop"
            );
            server.abort();
        }
    }
}
