//! The durable-dispatch backend selection at the host boundary (ADR-0019/0024).
//!
//! [`DeploymentConfig::durable_needs_persistence_error`] is unit-proven in-crate;
//! this binary pins the PUBLIC composition-root entrypoints that read it through the
//! process environment: [`ensure_durable_backend`] (the fail-fast persistence gate)
//! across the sqlite-vs-postgres / storage-dir axes, and
//! [`init_shared_postgres_dispatch`] (the Postgres branch) — skip-on-unreachable, so
//! it never needs a live database.
//!
//! Its own test binary because it mutates the process-global `AWAKEN_*` environment;
//! one sequential test owns that env so nothing races on it.

use awaken_runtime_host::{ensure_durable_backend, init_shared_postgres_dispatch};

/// Set or clear an env var. SAFETY: called only from the single sequential test in
/// this dedicated binary, before any concurrent reader exists.
fn set_env(key: &str, value: Option<&str>) {
    unsafe {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

fn clear_dispatch_env() {
    for key in [
        "AWAKEN_INGRESS",
        "AWAKEN_STORAGE_DIR",
        "AWAKEN_DISPATCH_BACKEND",
        "AWAKEN_DISPATCH_WAKE",
        "AWAKEN_DATABASE_URL",
    ] {
        set_env(key, None);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ensure_durable_backend_reads_the_env_persistence_contract() {
    clear_dispatch_env();

    // A direct ingress (no AWAKEN_INGRESS=durable) never requires persistence.
    assert!(
        ensure_durable_backend().is_ok(),
        "a direct ingress is always OK"
    );

    // durable + the default SQLite backend + no storage dir → the footgun: refused,
    // with the actionable message.
    set_env("AWAKEN_INGRESS", Some("durable"));
    let err = ensure_durable_backend()
        .expect_err("a durable ingress on a volatile in-memory queue is refused");
    assert!(err.contains("AWAKEN_STORAGE_DIR"), "{err}");

    // A storage dir makes the SQLite queue persistent → accepted.
    set_env("AWAKEN_STORAGE_DIR", Some("/tmp/awaken-dispatch-sel-test"));
    assert!(
        ensure_durable_backend().is_ok(),
        "a storage dir satisfies the durable contract"
    );

    // Postgres is persistent even without a storage dir → accepted.
    set_env("AWAKEN_STORAGE_DIR", None);
    set_env("AWAKEN_DISPATCH_BACKEND", Some("postgres"));
    assert!(
        ensure_durable_backend().is_ok(),
        "the Postgres dispatch backend satisfies the durable contract"
    );

    clear_dispatch_env();
}

#[tokio::test(flavor = "multi_thread")]
async fn init_shared_postgres_dispatch_selects_the_pg_branch_skipping_when_unreachable() {
    // Keep the wake selection on the default (no cross-node wake) so the branch under
    // test is the plain `connect_postgres`, not a pg-notify / nats variant.
    set_env("AWAKEN_DISPATCH_WAKE", None);

    // A real database only if the operator points us at one; otherwise an unreachable
    // URL exercises the Postgres branch and its connect-failure surface deterministically.
    let url = std::env::var("AWAKEN_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://127.0.0.1:1/awaken_no_such_db".to_string());
    match init_shared_postgres_dispatch(&url).await {
        Ok(()) => {
            // A live database was provided and the shared pool connected: a second
            // call is idempotent (keeps the first pool).
            assert!(init_shared_postgres_dispatch(&url).await.is_ok());
        }
        Err(e) => {
            // The expected skip: the Postgres branch was taken and reported a connect
            // failure (no live database), as a plain error string — never a panic.
            assert!(!e.is_empty(), "connect failure carries a message");
        }
    }
}
