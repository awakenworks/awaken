//! Backend-generic conformance for `SkillStore`, run against every backend
//! (in-memory + filesystem + sqlite, and — when reachable — postgres), so all
//! backends keep identical semantics: sanitized ids, sorted + workspace-scoped
//! listing, and idempotent delete.

use awaken_skill_store::{FsSkillStore, InMemorySkillStore, SkillStore};

async fn put_get_list_delete_and_scope_by_workspace(store: &dyn SkillStore) {
    // put returns the sanitized id it is addressable by.
    assert_eq!(store.put("ws1", "greet", "HELLO").await.unwrap(), "greet");
    assert_eq!(
        store.put("ws1", "../etc/passwd", "x").await.unwrap(),
        "etc-passwd"
    );
    store.put("ws1", "review", "REVIEW").await.unwrap();
    // A different workspace is isolated.
    store.put("ws2", "greet", "HALLO").await.unwrap();

    assert_eq!(
        store.get("ws1", "greet").await.unwrap().as_deref(),
        Some("HELLO")
    );
    assert_eq!(
        store.get("ws2", "greet").await.unwrap().as_deref(),
        Some("HALLO")
    );
    assert_eq!(store.get("ws1", "missing").await.unwrap(), None);

    // list is sorted by id and scoped to the workspace.
    let ws1: Vec<String> = store
        .list("ws1")
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ws1, vec!["etc-passwd", "greet", "review"]);
    assert_eq!(store.list("ws2").await.unwrap().len(), 1);
    assert!(store.list("ws_absent").await.unwrap().is_empty());

    // delete reports prior existence and is idempotent; other workspaces untouched.
    assert!(store.delete("ws1", "greet").await.unwrap());
    assert!(!store.delete("ws1", "greet").await.unwrap());
    assert_eq!(store.get("ws1", "greet").await.unwrap(), None);
    assert_eq!(
        store.get("ws2", "greet").await.unwrap().as_deref(),
        Some("HALLO")
    );
}

#[tokio::test]
async fn in_memory_conforms() {
    put_get_list_delete_and_scope_by_workspace(&InMemorySkillStore::new()).await;
}

#[tokio::test]
async fn fs_conforms_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    put_get_list_delete_and_scope_by_workspace(&FsSkillStore::open(&root).unwrap()).await;

    // A fresh handle over the same root (a restart) reads the committed catalog.
    let reopened = FsSkillStore::open(&root).unwrap();
    reopened.put("wsX", "persist", "BODY").await.unwrap();
    let again = FsSkillStore::open(&root).unwrap();
    assert_eq!(
        again.get("wsX", "persist").await.unwrap().as_deref(),
        Some("BODY")
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_conforms() {
    use awaken_skill_store::SqliteSkillStore;
    put_get_list_delete_and_scope_by_workspace(&SqliteSkillStore::open_in_memory().unwrap()).await;
}

/// Live Postgres conformance on a fresh schema. Skips when no Postgres is reachable
/// (`AWAKEN_TEST_DATABASE_URL`), proving the same portable bundle renders there too.
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_skill_store::PgSkillStore;
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
        let Some(pool) = schema_pool("t_skill").await else {
            return;
        };
        let store = PgSkillStore::with_pool(pool).await.unwrap();
        put_get_list_delete_and_scope_by_workspace(&store).await;
    }
}
