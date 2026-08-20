//! Backend-generic conformance for the versioned, binary-safe Skill repository.

#[cfg(feature = "test-support")]
use std::sync::Arc;

#[cfg(feature = "test-support")]
use awaken_skill_store::InMemorySkillStore;
use awaken_skill_store::{
    FsSkillStore, SkillBundleFile, SkillDefinition, SkillStore, SkillStoreError, SkillVersion,
    bundle_sha256,
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
    let ordinary_hash = bundle_sha256(std::slice::from_ref(&ordinary));
    let executable = SkillBundleFile {
        executable: true,
        ..ordinary
    };
    assert_ne!(ordinary_hash, bundle_sha256(&[executable]));
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

#[cfg(feature = "test-support")]
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

#[cfg(feature = "test-support")]
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

#[cfg(feature = "test-support")]
#[tokio::test]
async fn in_memory_serializes_concurrent_append() {
    concurrent_append_is_serialized(Arc::new(InMemorySkillStore::new())).await;
}

#[tokio::test]
async fn filesystem_process_append_helper() {
    let Ok(root) = std::env::var("AWAKEN_SKILL_PROCESS_ROOT") else {
        return;
    };
    let marker = std::env::var("AWAKEN_SKILL_PROCESS_MARKER").unwrap();
    let ready = std::env::var("AWAKEN_SKILL_PROCESS_READY").unwrap();
    let result = std::env::var("AWAKEN_SKILL_PROCESS_RESULT").unwrap();
    let go = std::path::Path::new(&root).join("go");
    std::fs::write(ready, b"ready").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !go.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "parent never released helper"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let store = FsSkillStore::open(root).unwrap();
    let outcome = store
        .append_version("ws", "skill", version("skill", 2, marker.as_bytes()))
        .await;
    let outcome = if outcome.is_ok() { "ok" } else { "conflict" };
    std::fs::write(result, outcome).expect("write helper outcome");
}

#[tokio::test]
async fn filesystem_processes_do_not_lose_a_concurrent_version() {
    // Test design — cross-process read/modify/write history, which subsumes the
    // weaker same-handle and two-handle cases: two OS processes are released at
    // the same durable V1 and race to append V2. The filesystem lock serializes
    // the complete read/validate/rename transition, so exactly one succeeds and
    // the other observes a conflict. Reopen must expose one complete V2 and no
    // torn aggregate or lost acknowledged update.
    let directory = tempfile::tempdir().unwrap();
    FsSkillStore::open(directory.path())
        .unwrap()
        .create(definition("ws", "skill"), version("skill", 1, b"one"))
        .await
        .unwrap();

    let executable = std::env::current_exe().unwrap();
    let spawn = |name: &str| {
        let ready = directory.path().join(format!("ready-{name}"));
        let result = directory.path().join(format!("result-{name}"));
        let child = std::process::Command::new(&executable)
            .arg("--exact")
            .arg("filesystem_process_append_helper")
            .arg("--nocapture")
            .env("AWAKEN_SKILL_PROCESS_ROOT", directory.path())
            .env("AWAKEN_SKILL_PROCESS_MARKER", name)
            .env("AWAKEN_SKILL_PROCESS_READY", &ready)
            .env("AWAKEN_SKILL_PROCESS_RESULT", &result)
            .spawn()
            .unwrap();
        (child, ready, result)
    };
    let (mut left, left_ready, left_result) = spawn("left");
    let (mut right, right_ready, right_result) = spawn("right");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !(left_ready.exists() && right_ready.exists()) {
        assert!(
            std::time::Instant::now() < deadline,
            "helpers did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::fs::write(directory.path().join("go"), b"go").unwrap();
    assert!(left.wait().unwrap().success());
    assert!(right.wait().unwrap().success());
    let outcomes = [
        std::fs::read_to_string(left_result).unwrap(),
        std::fs::read_to_string(right_result).unwrap(),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|value| value.as_str() == "ok")
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|value| value.as_str() == "conflict")
            .count(),
        1
    );

    let reopened = FsSkillStore::open(directory.path()).unwrap();
    assert_eq!(
        reopened.list_versions("ws", "skill").await.unwrap().len(),
        2
    );
}

#[cfg(all(feature = "sqlite", feature = "test-support"))]
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
