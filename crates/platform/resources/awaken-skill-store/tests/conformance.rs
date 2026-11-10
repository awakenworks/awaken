// @formal evidence skill.pinned_version_survives_retirement
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
    // R0 the initial head and highest ordinal must name the sole initial
    // version. Every direct backend rejects a fabricated history before insert.
    let mut invalid = definition("ws-invalid", "invalid-frame");
    invalid.last_version = 2;
    assert!(matches!(
        store
            .create(invalid, version("invalid-frame", 1, b"v1"))
            .await,
        Err(SkillStoreError::InvalidVersion { .. })
    ));
    assert!(
        store
            .definition("ws-invalid", "invalid-frame")
            .await
            .unwrap()
            .is_none()
    );
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
    let pinned_v2 = store.version("ws-a", "skill-a", 2).await.unwrap().unwrap();

    // Retirement cause/effect table over the real adapter transition:
    // R1 present visible v2 with another visible version -> hide v2, move the
    // latest pointer, preserve the exact pinned v2 bundle; R2 repeat retired v2
    // -> false and preserve the same bundle; R3 absent ordinal -> false and no
    // history change; R4 only visible version -> reject and preserve it.
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
        store.list_versions("ws-a", "skill-a").await.unwrap(),
        vec![store.version("ws-a", "skill-a", 1).await.unwrap().unwrap()],
        "R1 hides only the retired version"
    );
    assert_eq!(
        store.version("ws-a", "skill-a", 2).await.unwrap(),
        Some(pinned_v2.clone()),
        "R1 retains exact pinned bytes"
    );
    assert!(!store.delete_version("ws-a", "skill-a", 2).await.unwrap());
    assert_eq!(
        store.version("ws-a", "skill-a", 2).await.unwrap(),
        Some(pinned_v2),
        "R2 remains byte-exact"
    );
    assert!(!store.delete_version("ws-a", "skill-a", 99).await.unwrap());
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
    let pinned_v3 = store.version("ws-a", "skill-a", 3).await.unwrap().unwrap();
    assert!(matches!(
        store.delete_version("ws-a", "skill-a", 3).await,
        Err(SkillStoreError::LifecycleConflict { .. })
    ));
    assert_eq!(
        store.version("ws-a", "skill-a", 3).await.unwrap(),
        Some(pinned_v3),
        "R4 preserves the only visible version"
    );
    assert!(store.delete_skill("ws-a", "skill-a").await.unwrap());
    assert!(!store.delete_skill("ws-a", "skill-a").await.unwrap());
    assert!(store.definition("ws-b", "skill-a").await.unwrap().is_some());
    maximum_version_admission_preserves_error_precedence(store).await;
}

async fn maximum_version_admission_preserves_error_precedence(store: &dyn SkillStore) {
    // MX0 Store.create accepts a consistent, nonzero initial MAX frame. For
    // active MAX, identity/ordinal/bundle defects -> InvalidVersion before
    // the existing-key collision; a valid MAX candidate -> VersionConflict.
    // MX1 deleting the aggregate takes precedence over every same fault ->
    // NotFound. Every failed command retains the exact MAX immutable record.
    // This table uses the original backend and fixture constructors; no
    // private aggregate mutation or copied successor predicate is involved.
    let id = "maximum-version";
    let workspace = "ws-maximum-version";
    let original = version(id, u64::MAX, b"pinned MAX bytes");
    let mut initial = definition(workspace, id);
    initial.latest_version = u64::MAX;
    initial.last_version = u64::MAX;
    store
        .create(initial.clone(), original.clone())
        .await
        .unwrap();
    // MX2 the last available successor is still legal: an initial MAX-1
    // followed by append MAX must succeed on this same real backend, while
    // retaining MAX-1 byte-for-byte. This boundary positive prevents an
    // exhaustion guard from rejecting the predecessor one ordinal too early.
    let boundary_id = "maximum-predecessor";
    let predecessor = version(boundary_id, u64::MAX - 1, b"pinned predecessor bytes");
    let mut boundary_definition = definition(workspace, boundary_id);
    boundary_definition.latest_version = u64::MAX - 1;
    boundary_definition.last_version = u64::MAX - 1;
    store
        .create(boundary_definition, predecessor.clone())
        .await
        .unwrap();
    let last = version(boundary_id, u64::MAX, b"last available successor bytes");
    store
        .append_version(workspace, boundary_id, last.clone())
        .await
        .unwrap();
    assert_eq!(
        store
            .version(workspace, boundary_id, u64::MAX - 1)
            .await
            .unwrap(),
        Some(predecessor),
        "MX2 predecessor immutable"
    );
    assert_eq!(
        store
            .version(workspace, boundary_id, u64::MAX)
            .await
            .unwrap(),
        Some(last),
        "MX2 final successor admitted"
    );
    let boundary = store
        .definition(workspace, boundary_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (boundary.latest_version, boundary.last_version),
        (u64::MAX, u64::MAX),
        "MX2 high-water advanced to MAX"
    );
    assert!(
        store.delete_skill(workspace, boundary_id).await.unwrap(),
        "MX2 test-owned retirement"
    );
    assert_eq!(
        store.purge_skill(workspace, boundary_id).await.unwrap(),
        2,
        "MX2 test-owned cleanup"
    );
    for deleted in [false, true] {
        if deleted {
            assert!(
                store.delete_skill(workspace, id).await.unwrap(),
                "MX1 tombstone"
            );
        }
        for fault in ["valid", "identity", "ordinal", "bundle", "skill-md"] {
            let mut candidate = original.clone();
            match fault {
                "identity" => candidate.skill_id = "foreign-skill".into(),
                "ordinal" => candidate.version = u64::MAX - 1,
                "bundle" => candidate.bundle_sha256 = "wrong hash".into(),
                "skill-md" => {
                    candidate.files[0].path = "OTHER.md".into();
                    candidate.bundle_sha256 = bundle_sha256(&candidate.files);
                }
                _ => {}
            }
            let result = store.append_version(workspace, id, candidate).await;
            // MX0/MX1 classify each row exactly: tombstones are NotFound;
            // a valid live MAX candidate is VersionConflict; only the four
            // structural faults are InvalidVersion. The valid row must not
            // accidentally accept that last classification.
            assert!(
                matches!(
                    (deleted, fault, &result),
                    (true, _, Err(SkillStoreError::NotFound(_)))
                        | (false, "valid", Err(SkillStoreError::VersionConflict(_)))
                        | (
                            false,
                            "identity" | "ordinal" | "bundle" | "skill-md",
                            Err(SkillStoreError::InvalidVersion { .. })
                        )
                ),
                "MX0/MX1/{deleted}/{fault}: {result:?}"
            );
            assert_eq!(
                store.version(workspace, id, u64::MAX).await.unwrap(),
                Some(original.clone()),
                "MX immutable MAX/{deleted}/{fault}"
            );
            assert_eq!(
                store.definition(workspace, id).await.unwrap(),
                if deleted { None } else { Some(initial.clone()) },
                "MX no failed-write mutation/{deleted}/{fault}"
            );
        }
    }
    assert_eq!(
        store.purge_skill(workspace, id).await.unwrap(),
        1,
        "MX test-owned cleanup"
    );
}

async fn corrupt_frame_is_not_absent_or_overwritten(store: &dyn SkillStore) {
    // D1 corrupt persisted frame => every scoped read fails CorruptData;
    // D2 append/delete after D1 => same error, never rewrite damaged evidence.
    // Each durable adapter injects and re-reads its own bytes around this suite.
    assert!(
        matches!(
            store.definition("fault-workspace", "fault-skill").await,
            Err(SkillStoreError::CorruptData { .. })
        ),
        "D1 definition"
    );
    assert!(
        matches!(
            store.version("fault-workspace", "fault-skill", 1).await,
            Err(SkillStoreError::CorruptData { .. })
        ),
        "D1 pin"
    );
    assert!(
        matches!(
            store.snapshot_latest_versions("fault-workspace").await,
            Err(SkillStoreError::CorruptData { .. })
        ),
        "D1 snapshot"
    );
    assert!(
        matches!(
            store
                .append_version(
                    "fault-workspace",
                    "fault-skill",
                    version("fault-skill", 2, b"v2")
                )
                .await,
            Err(SkillStoreError::CorruptData { .. })
        ),
        "D2 append"
    );
    assert!(
        matches!(
            store.delete_skill("fault-workspace", "fault-skill").await,
            Err(SkillStoreError::CorruptData { .. })
        ),
        "D2 delete"
    );
}

#[tokio::test]
async fn filesystem_corruption_retains_the_original_frame() {
    let directory = tempfile::tempdir().unwrap();
    let store = FsSkillStore::open(directory.path()).unwrap();
    store
        .create(
            definition("fault-workspace", "fault-skill"),
            version("fault-skill", 1, b"v1"),
        )
        .await
        .unwrap();
    // The fresh store has one Workspace directory and one committed aggregate;
    // discover that exact fixture rather than duplicating its identity codec.
    let workspace = std::fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    let path = std::fs::read_dir(workspace)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    std::fs::write(&path, b"{}").unwrap();
    corrupt_frame_is_not_absent_or_overwritten(&store).await;
    assert_eq!(std::fs::read(path).unwrap(), b"{}", "D2 original bytes");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_corruption_retains_the_original_frame() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt.sqlite");
    let store = awaken_skill_store::SqliteSkillStore::open(path.to_str().unwrap())
        .await
        .unwrap();
    store
        .create(
            definition("fault-workspace", "fault-skill"),
            version("fault-skill", 1, b"v1"),
        )
        .await
        .unwrap();
    let connection = awaken_sqlite_pool::EmbeddedSqlite::file(path)
        .open()
        .await
        .unwrap();
    sqlx::query("UPDATE skill_store_aggregate SET data='{}' WHERE workspace_id=?1 AND id=?2")
        .bind("fault-workspace")
        .bind("fault-skill")
        .execute(&connection)
        .await
        .unwrap();
    corrupt_frame_is_not_absent_or_overwritten(&store).await;
    let stored: String = sqlx::query_scalar(
        "SELECT data FROM skill_store_aggregate WHERE workspace_id=?1 AND id=?2",
    )
    .bind("fault-workspace")
    .bind("fault-skill")
    .fetch_one(&connection)
    .await
    .unwrap();
    assert_eq!(stored, "{}", "D2 original bytes");
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
    assert_eq!(
        store
            .version("ws-a", "skill-a", 2)
            .await
            .unwrap()
            .unwrap()
            .files[1]
            .content,
        b"v2",
        "retired pinned bytes survive a filesystem reopen"
    );
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
        let output_path = directory.path().join(format!("output-{name}.log"));
        let output = std::fs::File::create(&output_path).unwrap();
        let child = std::process::Command::new(&executable)
            .arg("--exact")
            .arg("filesystem_process_append_helper")
            .arg("--nocapture")
            .env("AWAKEN_SKILL_PROCESS_ROOT", directory.path())
            .env("AWAKEN_SKILL_PROCESS_MARKER", name)
            .env("AWAKEN_SKILL_PROCESS_READY", &ready)
            .env("AWAKEN_SKILL_PROCESS_RESULT", &result)
            // The helper's libtest stream must not masquerade as the parent's
            // named outcomes. File redirection avoids pipe capacity deadlocks
            // and retains diagnostics for the actual child-status assertion.
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        (child, ready, result, output_path)
    };
    let (mut left, left_ready, left_result, left_output) = spawn("left");
    let (mut right, right_ready, right_result, right_output) = spawn("right");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !(left_ready.exists() && right_ready.exists()) {
        assert!(
            std::time::Instant::now() < deadline,
            "helpers did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::fs::write(directory.path().join("go"), b"go").unwrap();
    assert!(
        left.wait().unwrap().success(),
        "{}",
        std::fs::read_to_string(left_output).unwrap()
    );
    assert!(
        right.wait().unwrap().success(),
        "{}",
        std::fs::read_to_string(right_output).unwrap()
    );
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
    aggregate_lifecycle(
        &awaken_skill_store::SqliteSkillStore::open_in_memory()
            .await
            .unwrap(),
    )
    .await;
}

#[cfg(all(feature = "sqlite", feature = "test-support"))]
#[tokio::test]
async fn sqlite_retired_pin_survives_reopen() {
    // Persistence decision rule: a committed visible-version retirement (C1)
    // followed by connection loss/reopen (C2) must keep the retired ordinal out
    // of listings (E1) and return the identical pinned bundle by exact version
    // (E2). This crosses the adapter serialization boundary omitted by the pure
    // retirement proof; SQLite transaction/durability correctness remains TCB.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("skills.sqlite");
    let path = path.to_str().unwrap();
    let store = awaken_skill_store::SqliteSkillStore::open(path)
        .await
        .unwrap();
    store
        .create(
            definition("persist", "retired"),
            version("retired", 1, b"v1"),
        )
        .await
        .unwrap();
    let pinned = version("retired", 2, b"pinned-v2");
    store
        .append_version("persist", "retired", pinned.clone())
        .await
        .unwrap();
    assert!(store.delete_version("persist", "retired", 2).await.unwrap());
    drop(store);

    let reopened = awaken_skill_store::SqliteSkillStore::open(path)
        .await
        .unwrap();
    assert_eq!(
        reopened.list_versions("persist", "retired").await.unwrap(),
        vec![version("retired", 1, b"v1")],
        "C1+C2 -> E1"
    );
    assert_eq!(
        reopened.version("persist", "retired", 2).await.unwrap(),
        Some(pinned),
        "C1+C2 -> E2"
    );
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_skill_store::PgSkillStore;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    async fn schema_pool() -> Option<PgPool> {
        let url = awaken_reliability_testkit::postgres_fixture_url()?;
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
            .expect("configured PostgreSQL must be reachable");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("t_skill_{}_{nonce:x}", std::process::id());
        admin
            .execute(
                // The schema is a fixed fixture prefix plus integer process/time coordinates.
                sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")),
            )
            .await
            .expect("create owned test schema");
        admin.close().await;
        Some(
            PgPoolOptions::new()
                .max_connections(4)
                .after_connect(move |connection, _| {
                    let schema = schema.clone();
                    Box::pin(async move {
                        sqlx::query("SELECT set_config('search_path', $1, false)")
                            .bind(schema)
                            .execute(connection)
                            .await?;
                        Ok(())
                    })
                })
                .connect(&url)
                .await
                .expect("configured schema connection must succeed"),
        )
    }

    #[test]
    fn fixture_absence_and_configured_failure_are_distinct() {
        awaken_reliability_testkit::assert_postgres_fixture_child_contract(
            "postgres::postgres_conforms",
        );
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
        PgSkillStore::with_existing_pool(pool.clone())
            .await
            .unwrap();
        aggregate_lifecycle(&store).await;
        store
            .create(
                definition("fault-workspace", "fault-skill"),
                version("fault-skill", 1, b"v1"),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE skill_store_aggregate SET data='{}' WHERE workspace_id=$1 AND id=$2")
            .bind("fault-workspace")
            .bind("fault-skill")
            .execute(&pool)
            .await
            .unwrap();
        corrupt_frame_is_not_absent_or_overwritten(&store).await;
        let stored: String = sqlx::query_scalar(
            "SELECT data FROM skill_store_aggregate WHERE workspace_id=$1 AND id=$2",
        )
        .bind("fault-workspace")
        .bind("fault-skill")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, "{}", "D2 original bytes");
    }
}
