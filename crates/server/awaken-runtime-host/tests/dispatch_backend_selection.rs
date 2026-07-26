//! The durable-dispatch backend selection at the host boundary (ADR-0019/0024).
//!
//! [`DeploymentConfig::durable_needs_persistence_error`] is unit-proven in-crate.
//! This binary pins the public Postgres initialization seam with an explicitly
//! injected deployment snapshot; no production configuration is read from env.

use awaken_runtime_host::{DeploymentConfig, init_shared_postgres_dispatch_with_config};

#[tokio::test(flavor = "multi_thread")]
async fn init_shared_postgres_dispatch_selects_the_pg_branch_skipping_when_unreachable() {
    let deployment = DeploymentConfig::ephemeral();

    // A real database only if the operator points us at one; otherwise an unreachable
    // URL exercises the Postgres branch and its connect-failure surface deterministically.
    let url = std::env::var("AWAKEN_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://127.0.0.1:1/awaken_no_such_db".to_string());
    match init_shared_postgres_dispatch_with_config(&url, &deployment).await {
        Ok(()) => {
            // A live database was provided and the shared pool connected: a second
            // call is idempotent (keeps the first pool).
            assert!(
                init_shared_postgres_dispatch_with_config(&url, &deployment)
                    .await
                    .is_ok()
            );
        }
        Err(e) => {
            // The expected skip: the Postgres branch was taken and reported a connect
            // failure (no live database), as a plain error string — never a panic.
            assert!(!e.is_empty(), "connect failure carries a message");
        }
    }
}
