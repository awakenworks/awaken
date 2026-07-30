//! Backend-generic conformance for the versioned, binary-safe Skill repository.

use std::sync::Arc;

use awaken_skill_store::{
    FsSkillStore, InMemorySkillStore, SkillBundleFile, SkillDefinition, SkillStore,
    SkillStoreError, SkillVersion, bundle_sha256,
};

fn version(id: &str, ordinal: u64, marker: &[u8]) -> SkillVersion {
    let files = vec![
        SkillBundleFile {
            path: "SKILL.md".into(),
            content: format!("---\ndescription: version {ordinal}\n---\nBODY").into_bytes(),
            executable: false,
        },
        SkillBundleFile {
            path: "assets/data.bin".into(),
            content: marker.to_vec(),
            executable: false,
        },
    ];
    SkillVersion {
        id: format!("skver_{id}_{ordinal}").into(),
        skill_id: id.into(),
        version: ordinal,
        name: id.into(),
        description: format!("version {ordinal}"),
        directory: format!("/skills/{id}"),
        bundle_sha256: bundle_sha256(&files),
        files,
        created_unix_nanos: ordinal,
    }
}

fn definition(workspace: &str, id: &str) -> SkillDefinition {
    SkillDefinition {
        id: id.into(),
        workspace_id: workspace.into(),
        display_title: Some(id.into()),
        latest_version: 1,
        last_version: 1,
        timestamps: Default::default(),
    }
}

// Hash decision table: identical path/bytes with C1 executable=false and C2
// executable=true must produce different pins (E1), while the default false case
// keeps the legacy byte hash algorithm (E2, exercised by every persisted fixture).
// This prevents permission escalation without a Session bundle-hash change.
#[test]
fn executable_metadata_is_bound_into_new_bundle_hashes() {
    let ordinary = SkillBundleFile {
        path: "scripts/run.sh".into(),
        content: b"#!/bin/sh\n".to_vec(),
        executable: false,
    };
    let executable = SkillBundleFile {
        executable: true,
        ..ordinary.clone()
    };
    assert_ne!(bundle_sha256(&[ordinary]), bundle_sha256(&[executable]));
}

async fn aggregate_lifecycle(store: &dyn SkillStore) {
    let binary = [0, 159, 146, 150, 255];
    store
        .create(
            definition("ws-a", "skill-a"),
            version("skill-a", 1, &binary),
        )
        .await
        .unwrap();
    store
        .create(definition("ws-b", "skill-a"), version("skill-a", 1, b"B"))
        .await
        .unwrap();

    assert!(matches!(
        store
            .create(definition("ws-a", "skill-a"), version("skill-a", 1, b"x"))
            .await,
        Err(SkillStoreError::AlreadyExists(_))
    ));
    assert_eq!(store.list_definitions("ws-a").await.unwrap().len(), 1);
    assert_eq!(
        store
            .version("ws-a", "skill-a", 1)
            .await
            .unwrap()
            .unwrap()
            .files[1]
            .content,
        binary
    );
    assert!(store.definition("ws-c", "skill-a").await.unwrap().is_none());

    // Latest-snapshot cause/effect table:
    // S1 visible v1 -> one v1 head; S2 append v2 -> one v2 head;
    // S3 retire v2 -> one v1 head; S4 another Workspace -> excluded.
    // Each rule is one store call, so no list/read interleaving can mix heads.
    let latest = store.snapshot_latest_versions("ws-a").await.unwrap();
    assert_eq!((latest.len(), latest[0].version), (1, 1), "S1/S4");

    store
        .append_version("ws-a", "skill-a", version("skill-a", 2, b"v2"))
        .await
        .unwrap();
    let definition = store.definition("ws-a", "skill-a").await.unwrap().unwrap();
    assert_eq!(definition.latest_version, 2);
    assert_eq!(
        store.snapshot_latest_versions("ws-a").await.unwrap()[0].version,
        2,
        "S2"
    );
    assert_eq!(
        store.list_versions("ws-a", "skill-a").await.unwrap().len(),
        2
    );
    assert!(store.delete_version("ws-a", "skill-a", 2).await.unwrap());
    assert_eq!(
        store.snapshot_latest_versions("ws-a").await.unwrap()[0].version,
        1,
        "S3"
    );
    assert_eq!(
        store
            .definition("ws-a", "skill-a")
            .await
            .unwrap()
            .unwrap()
            .latest_version,
        1
    );
    assert_eq!(
        store
            .version("ws-a", "skill-a", 2)
            .await
            .unwrap()
            .unwrap()
            .files[1]
            .content,
        b"v2"
    );
    store
        .append_version("ws-a", "skill-a", version("skill-a", 3, b"v3"))
        .await
        .unwrap();
    assert_eq!(
        store
            .definition("ws-a", "skill-a")
            .await
            .unwrap()
            .unwrap()
            .last_version,
        3
    );
    assert!(matches!(
        store.delete_version("ws-a", "skill-a", 1).await,
        Ok(true)
    ));
    assert!(store.delete_skill("ws-a", "skill-a").await.unwrap());
    assert!(!store.delete_skill("ws-a", "skill-a").await.unwrap());
    assert!(store.definition("ws-b", "skill-a").await.unwrap().is_some());
}

#[tokio::test]
async fn in_memory_conforms() {
    aggregate_lifecycle(&InMemorySkillStore::new()).await;
}

#[tokio::test]
async fn filesystem_conforms_and_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    aggregate_lifecycle(&FsSkillStore::open(directory.path()).unwrap()).await;
    let store = FsSkillStore::open(directory.path()).unwrap();
    store
        .create(
            definition("persist", "binary"),
            version("binary", 1, &[0, 255]),
        )
        .await
        .unwrap();
    drop(store);
    let reopened = FsSkillStore::open(directory.path()).unwrap();
    assert_eq!(
        reopened
            .version("persist", "binary", 1)
            .await
            .unwrap()
            .unwrap()
            .files[1]
            .content,
        vec![0, 255]
    );
}

async fn concurrent_append_is_serialized<S>(store: Arc<S>)
where
    S: SkillStore + 'static,
{
    store
        .create(definition("ws", "skill"), version("skill", 1, b"one"))
        .await
        .unwrap();
    let first = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .append_version("ws", "skill", version("skill", 2, b"first"))
                .await
        })
    };
    let second = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .append_version("ws", "skill", version("skill", 2, b"second"))
                .await
        })
    };
    let results = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(store.list_versions("ws", "skill").await.unwrap().len(), 2);
}

#[tokio::test]
async fn in_memory_serializes_concurrent_append() {
    concurrent_append_is_serialized(Arc::new(InMemorySkillStore::new())).await;
}

#[tokio::test]
async fn filesystem_serializes_concurrent_append() {
    let directory = tempfile::tempdir().unwrap();
    concurrent_append_is_serialized(Arc::new(FsSkillStore::open(directory.path()).unwrap())).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_conforms() {
    aggregate_lifecycle(&awaken_skill_store::SqliteSkillStore::open_in_memory().unwrap()).await;
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_skill_store::PgSkillStore;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    fn database_url() -> String {
        std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".into()
        })
    }

    async fn schema_pool() -> Option<PgPool> {
        let admin = PgPool::connect(&database_url()).await.ok()?;
        let _ = admin.execute("DROP SCHEMA IF EXISTS t_skill CASCADE").await;
        admin.execute("CREATE SCHEMA t_skill").await.ok()?;
        admin.close().await;
        PgPoolOptions::new()
            .after_connect(|connection, _| {
                Box::pin(async move {
                    connection.execute("SET search_path = t_skill").await?;
                    Ok(())
                })
            })
            .connect(&database_url())
            .await
            .ok()
    }

    #[tokio::test]
    async fn postgres_conforms() {
        let Some(pool) = schema_pool().await else {
            return;
        };

        // Causal graph and decision table:
        // absent ledger + verify -> fail/no DDL; migrate -> current ledger;
        // current ledger + verify -> serve/no DDL.
        assert!(
            PgSkillStore::with_existing_pool(pool.clone())
                .await
                .is_err()
        );
        let ledger_after_verify: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('skill_store_schema_migrations')::text")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ledger_after_verify, None, "verify never creates its ledger");

        let store = PgSkillStore::with_pool(pool.clone());
        store.ensure_schema().await.unwrap();
        PgSkillStore::with_existing_pool(pool).await.unwrap();
        aggregate_lifecycle(&store).await;
    }
}
