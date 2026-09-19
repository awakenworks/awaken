//! Live Postgres conformance for the Resources-owned Registry repository.
#![cfg(feature = "postgres")]

use std::time::{SystemTime, UNIX_EPOCH};

use awaken_resource_contract::ResourceRegistryRepository;
use awaken_resource_store::PostgresResourceStore;
use sqlx::Executor;
use sqlx::postgres::PgPool;

fn base_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_owned()
    })
}

async fn isolated_url() -> Option<String> {
    let admin = match PgPool::connect(&base_url()).await {
        Ok(pool) => pool,
        Err(error) => {
            println!("[skip] no Postgres reachable: {error}");
            return None;
        }
    };
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let schema = format!("t_resource_registry_{}_{}", std::process::id(), nonce);
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create isolated schema");
    admin.close().await;
    let separator = if base_url().contains('?') { '&' } else { '?' };
    Some(format!(
        "{}{}options=-c%20search_path%3D{}",
        base_url(),
        separator,
        schema
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn resource_registry_repository_conforms_and_survives_restart() {
    let Some(url) = isolated_url().await else {
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
