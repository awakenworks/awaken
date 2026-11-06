// @formal surface exclude=test_only
// @formal owner crates/platform/resources/awaken-file-store/src/lib.rs
// @formal reason: Exercises original File provenance, migration, command replay and independent durable connections; this is regression evidence, not a formal proof.
//! Original File metadata retains personal provenance across retries and reopen.
#![cfg(any(feature = "test-support", feature = "sqlite", feature = "postgres"))]

use awaken_resource_contract::{
    CreateFileRecordOutcome, FileCatalog, FileCatalogError, FileRecord, PrivateContentBinding,
};
use std::sync::Arc;

#[cfg(feature = "postgres")]
#[path = "support/postgres.rs"]
mod postgres_test_support;

fn binding(
    org: &str,
    app: &str,
    account: &str,
    workspace: &str,
    revision: u64,
) -> PrivateContentBinding {
    PrivateContentBinding::new(
        org.into(),
        app.into(),
        account.into(),
        workspace.into(),
        revision,
    )
    .unwrap()
}

fn input() -> FileRecord {
    FileRecord {
        id: "file_original".into(),
        workspace_id: "private-space".into(),
        blob_id: "shared-physical-blob".into(),
        filename: "input.txt".into(),
        mime_type: "text/plain".into(),
        size_bytes: 7,
        created_at: "2026-11-05T00:00:00Z".into(),
        expires_at: None,
        downloadable: false,
        scope_id: None,
        logical_path: None,
        harvest_key: Some("original-command".into()),
        artifact_idempotency_scope: None,
        deleted: false,
        private_binding: Some(binding("org", "desk", "alice", "private-space", 1)),
    }
}

async fn contract(left: Arc<dyn FileCatalog>, right: Arc<dyn FileCatalog>) {
    // P1 same command recovers the original row. P2 changing any coordinate or
    // clearing private provenance conflicts, including after logical deletion.
    // P3 the original row's Workspace and input/artifact identity must agree.
    // P4 colliding requests through independent connections cannot replace the
    // committed provenance. No observed legacy File becomes a private input.
    let original = input();
    assert_eq!(
        left.create_file(original.clone()).await.unwrap(),
        CreateFileRecordOutcome::Inserted(original.clone()),
        "P1"
    );
    let mut replay = original.clone();
    replay.id = "file_retry".into();
    assert_eq!(
        right.create_file(replay.clone()).await.unwrap(),
        CreateFileRecordOutcome::Existing(original.clone()),
        "P1"
    );
    for provenance in [
        Some(binding("other-org", "desk", "alice", "private-space", 1)),
        Some(binding("org", "other-app", "alice", "private-space", 1)),
        Some(binding("org", "desk", "bob", "private-space", 1)),
        Some(binding("org", "desk", "alice", "other-space", 1)),
        Some(binding("org", "desk", "alice", "private-space", 2)),
        None,
    ] {
        let candidate = FileRecord {
            private_binding: provenance,
            ..replay.clone()
        };
        for deleted in [false, true] {
            if deleted {
                left.mark_file_deleted("private-space", &original.id)
                    .await
                    .unwrap();
            }
            assert!(right.create_file(candidate.clone()).await.is_err(), "P2/P3");
            let retained = left
                .get_file("private-space", &original.id, true)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                retained.private_binding, original.private_binding,
                "P2: retained original provenance"
            );
        }
    }
    for (scope_id, logical_path, artifact_idempotency_scope) in [
        (Some("session".into()), None, None),
        (None, Some("output.txt".into()), None),
        (
            Some("session".into()),
            Some("output.txt".into()),
            Some("terminal".into()),
        ),
    ] {
        let malformed = FileRecord {
            id: "file_invalid".into(),
            scope_id,
            logical_path,
            artifact_idempotency_scope,
            ..original.clone()
        };
        assert!(
            matches!(
                left.create_file(malformed).await,
                Err(FileCatalogError::InvalidRecord { .. })
            ),
            "P3"
        );
    }
    let retained = left
        .get_file("private-space", &original.id, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        right.create_file(replay).await.unwrap(),
        CreateFileRecordOutcome::Existing(retained),
        "P2: exact retry never revives a tombstone"
    );
    let mut legacy = original.clone();
    legacy.id = "file_legacy".into();
    legacy.harvest_key = Some("legacy-command".into());
    legacy.private_binding = None;
    left.create_file(legacy.clone()).await.unwrap();
    let adopt = FileRecord {
        id: "file_adopt".into(),
        private_binding: original.private_binding.clone(),
        ..legacy.clone()
    };
    assert!(
        matches!(
            right.create_file(adopt).await,
            Err(FileCatalogError::IdentityConflict { .. })
        ),
        "P4: legacy is never adopted"
    );
    let race_a = FileRecord {
        id: "race_a".into(),
        harvest_key: Some("racing-command".into()),
        ..original.clone()
    };
    let race_b = FileRecord {
        id: "race_b".into(),
        private_binding: Some(binding("org", "desk", "bob", "private-space", 1)),
        ..race_a.clone()
    };
    let (a, b) = tokio::join!(left.create_file(race_a), right.create_file(race_b));
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "P4: exactly one binding wins"
    );
    assert_eq!(
        left.get_file("private-space", "file_legacy", false)
            .await
            .unwrap(),
        Some(legacy),
        "P4"
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn memory_private_input_provenance() {
    let store = Arc::new(awaken_file_store::InMemoryFileStore::new());
    contract(store.clone(), store).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_private_input_provenance_survives_independent_connections_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("private-input.sqlite")
        .to_string_lossy()
        .into_owned();
    let left = Arc::new(
        awaken_file_store::sqlite::SqliteFileStore::open(&path)
            .await
            .unwrap(),
    );
    let right = Arc::new(
        awaken_file_store::sqlite::SqliteFileStore::open(&path)
            .await
            .unwrap(),
    );
    contract(left, right).await;
    let reopened = awaken_file_store::sqlite::SqliteFileStore::open(&path)
        .await
        .unwrap();
    let row = reopened
        .get_file("private-space", "file_original", true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.private_binding,
        input().private_binding,
        "P5: original metadata after reopen"
    );
    assert!(row.deleted, "P5: binding remains on the tombstone");
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_private_input_provenance_survives_independent_connections_and_reopen() {
    let Some(url) = postgres_test_support::isolated_postgres_url("t_file_private_input").await
    else {
        return;
    };
    let left = Arc::new(
        awaken_file_store::postgres::PgFileStore::connect(&url)
            .await
            .unwrap(),
    );
    let right = Arc::new(
        awaken_file_store::postgres::PgFileStore::connect_existing(&url)
            .await
            .unwrap(),
    );
    contract(left, right).await;
    let reopened = awaken_file_store::postgres::PgFileStore::connect_existing(&url)
        .await
        .unwrap();
    let row = reopened
        .get_file("private-space", "file_original", true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.private_binding, input().private_binding, "P5");
    assert!(row.deleted, "P5");
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn migration_retains_legacy_absence_and_corrupt_provenance_never_becomes_public() {
    use awaken_file_store::schema::{BUNDLE_ID, NS, file_store_bundle};
    use awaken_scoped_migration::{MigrationBundle, sqlite::SqliteMigrationRunner};
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("legacy-input.sqlite")
        .to_string_lossy()
        .into_owned();
    let pool = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
        .open()
        .await
        .unwrap();
    let full = file_store_bundle().unwrap();
    let legacy = MigrationBundle::new(BUNDLE_ID, full.migrations()[..7].to_vec()).unwrap();
    SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
        .unwrap()
        .run_bundle(&legacy)
        .await
        .unwrap();
    sqlx::query("INSERT INTO file_store_file (id,workspace_id,blob_id,filename,mime_type,size_bytes,created_at,downloadable,deleted) VALUES ('file_legacy','private-space','blob','legacy.txt','text/plain',1,'2026-11-05T00:00:00Z',0,0)")
        .execute(&pool).await.unwrap();
    let store = awaken_file_store::sqlite::SqliteFileStore::open(&path)
        .await
        .unwrap();
    let old = store
        .get_file("private-space", "file_legacy", false)
        .await
        .unwrap()
        .unwrap();
    assert!(
        old.private_binding.is_none(),
        "P6: additive migration never adopts legacy content"
    );
    assert!(
        serde_json::to_value(&old)
            .unwrap()
            .get("private_binding")
            .is_none(),
        "P6: old record encoding is retained"
    );
    for encoded in [
        "not-json".to_owned(),
        "null".to_owned(),
        serde_json::to_string(&binding("org", "desk", "alice", "wrong-space", 1)).unwrap(),
    ] {
        sqlx::query("UPDATE file_store_file SET private_binding_json=? WHERE id='file_legacy'")
            .bind(encoded)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            store
                .get_file("private-space", "file_legacy", false)
                .await
                .is_err(),
            "P7: malformed provenance is unavailable"
        );
        assert!(
            store.list_files("private-space", None).await.is_err(),
            "P7: list cannot erase the failure"
        );
    }
}
