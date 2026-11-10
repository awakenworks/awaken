//! Live Postgres conformance for the Resources-owned Registry repository.
#![cfg(feature = "postgres")]

use std::time::{SystemTime, UNIX_EPOCH};

use awaken_resource_contract::{
    ResourceKind, ResourcePurgeRepository, ResourcePurgeStatus, ResourceReferenceIndex,
    ResourceRegistryRepository,
};
use awaken_resource_store::PostgresResourceStore;
use sqlx::Executor;
use sqlx::postgres::PgPool;

async fn isolated_url(prefix: &str) -> Option<String> {
    assert!(
        !prefix.is_empty()
            && !prefix.as_bytes()[0].is_ascii_digit()
            && prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    );
    let base_url = awaken_reliability_testkit::postgres_fixture_url()?;
    let admin = PgPool::connect(&base_url)
        .await
        .expect("configured Postgres must connect");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let schema = format!("{prefix}_{}_{}", std::process::id(), nonce);
    // Only the checked ASCII fixture prefix and integer coordinates form DDL.
    admin
        .execute(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .await
        .expect("create isolated schema");
    admin.close().await;
    let separator = if base_url.contains('?') { '&' } else { '?' };
    Some(format!(
        "{}{}options=-c%20search_path%3D{}",
        base_url, separator, schema
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn resource_registry_repository_conforms_and_survives_restart() {
    // Fixture rule: absent configuration -> explicit local skip; configured
    // database -> connection/schema failure is a failed test, then the shared
    // CAS suite and fresh-connection readback must both pass.
    let Some(url) = isolated_url("t_resource_registry").await else {
        return;
    };
    // The shared suite owns the CAS, duplicate-insert and partitioning laws;
    // this backend adds the medium law: a fresh connection to the same
    // database still shows what the suite committed.
    awaken_store_conformance::resource_registry::run_suite(|| async {
        PostgresResourceStore::connect(&url)
            .await
            .expect("open Resources store")
    })
    .await;
    let reopened = PostgresResourceStore::connect_existing(&url)
        .await
        .expect("verify and reopen");
    let listed = reopened
        .list_memory_stores("ws-a")
        .await
        .expect("durable list");
    assert_eq!(
        listed.len(),
        2,
        "reopen shows the suite's committed aggregates"
    );
    assert!(
        reopened
            .load_repository("repository-1")
            .await
            .expect("durable load")
            .is_some(),
        "reopen shows the suite's repository"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resource_lifecycle_repository_conforms_and_survives_restart() {
    // Causes: L1 no configured fixture vs an explicit configured database;
    // L2 fresh lifecycle state; L3 a new connection after the shared suite.
    // Effects: L1 skips only when absent and fails on bad configuration; L2
    // applies every purge/CAS/reference/fence rule; L3 sees the exact claimed
    // intent and remaining reference. Rules: absent->named skip;
    // configured+L2->suite; configured+L2+L3->durable readback.
    let Some(url) = isolated_url("t_resource_lifecycle").await else {
        return;
    };
    let store = PostgresResourceStore::connect(&url)
        .await
        .expect("open Resources lifecycle store");
    awaken_store_conformance::resource_lifecycle::run_suite(&store).await;
    drop(store);
    let reopened = PostgresResourceStore::connect_existing(&url)
        .await
        .expect("verify and reopen lifecycle store");
    assert_eq!(
        reopened
            .get("purge-1")
            .await
            .expect("durable intent")
            .expect("suite inserted intent")
            .status,
        ResourcePurgeStatus::Claimed
    );
    assert_eq!(
        reopened
            .references_for_resource(ResourceKind::File, "hash-1")
            .await
            .expect("durable reverse references")
            .len(),
        1
    );
}
