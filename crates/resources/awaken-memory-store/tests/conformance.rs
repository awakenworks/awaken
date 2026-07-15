//! Backend-generic conformance for `MemoryBlobStore`, run against every backend
//! (in-memory + filesystem + sqlite, and — when reachable — postgres), so all
//! backends keep identical semantics: dense unique-id minting, byte round-trips,
//! workspace scoping, and empty-on-create.

use awaken_memory_store::{FsMemoryBlobStore, InMemoryBlobStore, MemoryBlobStore};

async fn create_put_get_exists_and_scope_by_workspace(store: &dyn MemoryBlobStore) {
    // create mints a distinct id per call and resolves empty.
    let a = store.create("ws1").await.unwrap();
    let b = store.create("ws1").await.unwrap();
    assert_ne!(a, b, "ids are distinct");
    assert_eq!(store.get("ws1", &a).await.unwrap(), Some(Vec::new()));
    assert!(store.exists("ws1", &a).await.unwrap());
    assert!(!store.exists("ws1", "memstore_absent").await.unwrap());

    // put overwrites; bytes round-trip.
    store.put("ws1", &a, b"hello bytes").await.unwrap();
    assert_eq!(
        store.get("ws1", &a).await.unwrap().as_deref(),
        Some(&b"hello bytes"[..])
    );

    // A different workspace is isolated: the same id is absent there.
    assert_eq!(store.get("ws2", &a).await.unwrap(), None);
    assert!(!store.exists("ws2", &a).await.unwrap());
    store.put("ws2", "memstore_x", b"other").await.unwrap();
    assert_eq!(
        store.get("ws2", "memstore_x").await.unwrap().as_deref(),
        Some(&b"other"[..])
    );
    assert_eq!(store.get("ws1", "memstore_x").await.unwrap(), None);

    // A crafted `../` id addresses one safe blob: put and get sanitize identically, so
    // it round-trips and can never escape the store root. A very long id is bounded to
    // a stem, not a panic or an over-NAME_MAX filename.
    store.put("wsC", "../../etc/passwd", b"safe").await.unwrap();
    assert_eq!(
        store
            .get("wsC", "../../etc/passwd")
            .await
            .unwrap()
            .as_deref(),
        Some(&b"safe"[..])
    );
    let long = "x".repeat(500);
    store.put("wsC", &long, b"bounded").await.unwrap();
    assert_eq!(
        store.get("wsC", &long).await.unwrap().as_deref(),
        Some(&b"bounded"[..])
    );
}

#[tokio::test]
async fn in_memory_conforms() {
    create_put_get_exists_and_scope_by_workspace(&InMemoryBlobStore::new()).await;
}

#[tokio::test]
async fn fs_conforms_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    create_put_get_exists_and_scope_by_workspace(&FsMemoryBlobStore::open(&root).unwrap()).await;

    // A restart re-seeds the counter past what's on disk (no id re-mint) and reads
    // the committed bytes back.
    let store = FsMemoryBlobStore::open(&root).unwrap();
    let id = store.create("wsR").await.unwrap();
    store.put("wsR", &id, b"persist").await.unwrap();
    let reopened = FsMemoryBlobStore::open(&root).unwrap();
    assert_eq!(
        reopened.get("wsR", &id).await.unwrap().as_deref(),
        Some(&b"persist"[..])
    );
    // The reopened store does not re-mint the existing id.
    assert_ne!(reopened.create("wsR").await.unwrap(), id);
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_conforms() {
    use awaken_memory_store::SqliteMemoryBlobStore;
    create_put_get_exists_and_scope_by_workspace(&SqliteMemoryBlobStore::open_in_memory().unwrap())
        .await;
}

/// Live Postgres conformance on a fresh schema. Skips when no Postgres is reachable
/// (`AWAKEN_TEST_DATABASE_URL`), proving the same portable bundle renders there too.
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_memory_store::PgMemoryBlobStore;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    fn database_url() -> String {
        std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        })
    }

    async fn schema_pool(schema: &'static str) -> Option<PgPool> {
        let admin = match PgPool::connect(&database_url()).await {
            Ok(pool) => pool,
            Err(err) => {
                println!("[skip] no Postgres reachable: {err}");
                return None;
            }
        };
        let _ = admin
            .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
            .await;
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        admin.close().await;
        PgPoolOptions::new()
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    conn.execute(format!("SET search_path = {schema}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url())
            .await
            .ok()
    }

    #[tokio::test]
    async fn postgres_conforms() {
        let Some(pool) = schema_pool("t_memory").await else {
            return;
        };
        let store = PgMemoryBlobStore::with_pool(pool);
        store.ensure_schema().await.unwrap();
        create_put_get_exists_and_scope_by_workspace(&store).await;
    }
}
