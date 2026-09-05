//! The same cause/effect contract runs against all three FileCatalog adapters.
#![cfg(any(feature = "test-support", feature = "sqlite", feature = "postgres"))]

use std::sync::Arc;

use awaken_resource_contract::{
    ArtifactCapture, FileCatalog, FileRecord, ReplaceArtifactCapture,
    ReplaceArtifactCaptureOutcome as Outcome, content_id, harvest_idempotency_key,
};

fn file(id: &str, path: &str) -> FileRecord {
    let blob_id = content_id(id.as_bytes());
    FileRecord {
        id: id.into(),
        workspace_id: "capture-workspace".into(),
        blob_id: blob_id.clone(),
        filename: path.into(),
        mime_type: "text/plain".into(),
        size_bytes: id.len() as u64,
        // Deliberately equal creation times: neither sorting nor timestamps is
        // evidence of which version a complete scan selected.
        created_at: "2026-09-06T00:00:00Z".into(),
        expires_at: None,
        downloadable: true,
        scope_id: Some("capture-session".into()),
        logical_path: Some(path.into()),
        harvest_key: Some(harvest_idempotency_key("capture-session", path, &blob_id)),
        artifact_idempotency_scope: None,
        deleted: false,
    }
}

fn request(revision: u64, ids: &[&str]) -> ReplaceArtifactCapture {
    ReplaceArtifactCapture {
        workspace_id: "capture-workspace".into(),
        session_id: "capture-session".into(),
        expected_revision: revision,
        file_ids: ids.iter().map(|id| (*id).into()).collect(),
    }
}

async fn current(catalog: &dyn FileCatalog) -> Option<ArtifactCapture> {
    catalog
        .artifact_capture("capture-workspace", "capture-session")
        .await
        .unwrap()
}

async fn contract(catalog: Arc<dyn FileCatalog>) {
    // Cause graph: C1 expected revision matches; C2 exact prior write replay;
    // C3 every referenced File is a live artifact of this scope; C4 ids and
    // paths are unique; C5 complete scan is empty; C6 old File is later deleted.
    // Effects E1 atomic new capture; E2 exact replay; E3 conflict/no mutation;
    // E4 invalid/no mutation; E5 history and File lifecycle remain independent.
    // Rules: R1 !existing+C1+C3+C4 -> E1; R2 C2 -> E2; R3 !C1+!C2 -> E3;
    // R4 C1+(!C3|!C4) -> E4; R5 C1+C5 -> E1(empty), never unknown;
    // R6 A->B->A -> E1 reuses A's id; R7 C6 -> E5, no fallback to B;
    // R8 concurrent different writes from one revision -> one E1 and one E3;
    // R9 concurrent identical writes -> one E1 and one E2.
    assert_eq!(
        current(catalog.as_ref()).await,
        None,
        "R1 unknown is not empty"
    );
    for record in [
        file("capture-a", "report.txt"),
        file("capture-b", "report.txt"),
        file("capture-c", "report.txt"),
        file("capture-other", "notes.txt"),
    ] {
        catalog.create_file(record).await.unwrap();
    }
    let a = request(0, &["capture-a"]);
    assert!(
        matches!(
            catalog.replace_artifact_capture(a.clone()).await.unwrap(),
            Outcome::Applied(_)
        ),
        "R1"
    );
    assert!(
        matches!(
            catalog.replace_artifact_capture(a.clone()).await.unwrap(),
            Outcome::Replayed(_)
        ),
        "R2"
    );
    assert!(
        matches!(
            catalog
                .replace_artifact_capture(request(1, &["capture-b"]))
                .await
                .unwrap(),
            Outcome::Applied(_)
        ),
        "R6 B"
    );
    let back = request(2, &["capture-a"]);
    assert!(
        matches!(
            catalog
                .replace_artifact_capture(back.clone())
                .await
                .unwrap(),
            Outcome::Applied(_)
        ),
        "R6 A again"
    );
    let stable = current(catalog.as_ref()).await.unwrap();
    assert_eq!(stable.revision, 3);
    assert_eq!(stable.file_ids, ["capture-a"]);
    assert_eq!(
        catalog.replace_artifact_capture(a).await.unwrap(),
        Outcome::Conflict,
        "R3 stale replay cannot undo B->A"
    );
    assert_eq!(
        catalog
            .replace_artifact_capture(request(8, &[]))
            .await
            .unwrap(),
        Outcome::Conflict,
        "R3 future revision"
    );
    assert_eq!(
        catalog
            .artifact_capture("foreign", "capture-session")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        catalog
            .artifact_capture("capture-workspace", "foreign")
            .await
            .unwrap(),
        None
    );

    // R4 independently falsify every identity/admission predicate. Multi-file
    // failure must preserve the WHOLE previous capture, not publish its good prefix.
    for (index, cause) in [
        "workspace",
        "session",
        "upload",
        "deleted",
        "download",
        "expiry",
        "harvest",
        "path",
        "filename",
    ]
    .iter()
    .enumerate()
    {
        let id = format!("capture-invalid-{index}");
        let mut record = file(&id, &format!("invalid-{index}.txt"));
        match *cause {
            "workspace" => record.workspace_id = "foreign".into(),
            "session" => record.scope_id = Some("foreign".into()),
            "upload" => {
                record.logical_path = None;
                record.harvest_key = None;
            }
            "deleted" => record.deleted = true,
            "download" => record.downloadable = false,
            "expiry" => record.expires_at = Some("2099-01-01T00:00:00Z".into()),
            "harvest" => record.harvest_key = Some("unrelated".into()),
            "path" => {
                record.logical_path = Some("../outside.txt".into());
                record.filename = "../outside.txt".into();
                record.harvest_key = Some(harvest_idempotency_key(
                    "capture-session",
                    "../outside.txt",
                    &record.blob_id,
                ));
            }
            "filename" => record.filename = "different.txt".into(),
            _ => unreachable!(),
        }
        catalog.create_file(record).await.unwrap();
        assert!(
            catalog
                .replace_artifact_capture(request(3, &["capture-other", &id]))
                .await
                .is_err(),
            "R4 {cause}"
        );
        assert_eq!(
            current(catalog.as_ref()).await,
            Some(stable.clone()),
            "R4 {cause}"
        );
    }
    for ids in [
        vec!["missing"],
        vec!["capture-a", "capture-a"],
        vec!["capture-a", "capture-b"],
        vec![" "],
    ] {
        assert!(
            catalog
                .replace_artifact_capture(request(3, &ids))
                .await
                .is_err(),
            "R4 {ids:?}"
        );
        assert_eq!(current(catalog.as_ref()).await, Some(stable.clone()));
    }
    let mut blank_workspace = request(3, &[]);
    blank_workspace.workspace_id.clear();
    let mut blank_session = request(3, &[]);
    blank_session.session_id.clear();
    for invalid in [
        request(i64::MAX as u64, &[]),
        request(u64::MAX, &[]),
        blank_workspace,
        blank_session,
    ] {
        assert!(
            catalog.replace_artifact_capture(invalid).await.is_err(),
            "R4 invalid scope/overflow"
        );
    }
    let mut invalid = request(0, &["missing"]);
    invalid.session_id = "absent-session".into();
    assert!(catalog.replace_artifact_capture(invalid).await.is_err());
    assert_eq!(
        catalog
            .artifact_capture("capture-workspace", "absent-session")
            .await
            .unwrap(),
        None,
        "R4 failed first write leaves no seed"
    );

    catalog
        .mark_file_deleted("capture-workspace", "capture-a")
        .await
        .unwrap();
    assert_eq!(
        current(catalog.as_ref()).await,
        Some(stable),
        "R7 exact deleted id retained"
    );
    assert!(
        catalog
            .get_file("capture-workspace", "capture-a", false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        matches!(
            catalog.replace_artifact_capture(back).await.unwrap(),
            Outcome::Replayed(_)
        ),
        "R2/R7 old receipt does not revive File"
    );
    assert!(
        catalog
            .replace_artifact_capture(request(3, &["capture-a"]))
            .await
            .is_err(),
        "R7 new capture cannot select a deleted File"
    );
    assert!(
        matches!(
            catalog
                .replace_artifact_capture(request(3, &[]))
                .await
                .unwrap(),
            Outcome::Applied(_)
        ),
        "R5"
    );
    assert!(current(catalog.as_ref()).await.unwrap().file_ids.is_empty());
    assert!(
        catalog
            .get_file("capture-workspace", "capture-b", false)
            .await
            .unwrap()
            .is_some(),
        "R5 retains history"
    );

    let (left, right) = tokio::join!(
        catalog.replace_artifact_capture(request(4, &["capture-b"])),
        catalog.replace_artifact_capture(request(4, &["capture-c"]))
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Applied(_)))
            .count(),
        1,
        "R8"
    );
    assert_eq!(
        outcomes.iter().filter(|o| **o == Outcome::Conflict).count(),
        1,
        "R8"
    );
    let (left, right) = tokio::join!(
        catalog.replace_artifact_capture(request(5, &["capture-other", "capture-b"])),
        catalog.replace_artifact_capture(request(5, &["capture-b", "capture-other"]))
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Applied(_)))
            .count(),
        1,
        "R9"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Replayed(_)))
            .count(),
        1,
        "R9 order-independent exact replay"
    );
    assert_eq!(current(catalog.as_ref()).await.unwrap().revision, 6);

    // R8 also applies before a capture row exists: competing first writers
    // must not both succeed or expose a revision-zero placeholder. A future
    // revision on an absent scope must leave it absent (R3).
    let mut first = request(9, &[]);
    first.session_id = "concurrent-first".into();
    assert_eq!(
        catalog
            .replace_artifact_capture(first.clone())
            .await
            .unwrap(),
        Outcome::Conflict
    );
    assert_eq!(
        catalog
            .artifact_capture("capture-workspace", &first.session_id)
            .await
            .unwrap(),
        None
    );
    let mut record = file("capture-first", "first.txt");
    record.scope_id = Some(first.session_id.clone());
    record.harvest_key = Some(harvest_idempotency_key(
        &first.session_id,
        "first.txt",
        &record.blob_id,
    ));
    catalog.create_file(record).await.unwrap();
    first.expected_revision = 0;
    let mut other = first.clone();
    other.file_ids = vec!["capture-first".into()];
    let (left, right) = tokio::join!(
        catalog.replace_artifact_capture(first.clone()),
        catalog.replace_artifact_capture(other),
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Applied(_)))
            .count(),
        1,
        "R8 first create"
    );
    assert_eq!(
        outcomes.iter().filter(|o| **o == Outcome::Conflict).count(),
        1,
        "R8 first create"
    );
    assert_eq!(
        catalog
            .artifact_capture("capture-workspace", &first.session_id)
            .await
            .unwrap()
            .unwrap()
            .revision,
        1
    );
}

// D1 persisted zero/negative revision, malformed JSON, blank/duplicate or
// unsorted ids -> storage error on both read and replace, never an empty/new
// capture. The adapters must leave the original damaged row unchanged.
#[cfg(any(feature = "sqlite", feature = "postgres"))]
const CORRUPT_CAPTURES: &[(i64, &str)] = &[
    (0, "[]"),
    (-1, "[]"),
    (1, "not-json"),
    (1, "{}"),
    (1, r#"[" "]"#),
    (1, r#"["a","a"]"#),
    (1, r#"["b","a"]"#),
];

#[cfg(any(feature = "sqlite", feature = "postgres"))]
async fn reject_corrupt(catalog: &dyn FileCatalog, session: &str) {
    assert!(
        matches!(
            catalog.artifact_capture("capture-workspace", session).await,
            Err(awaken_resource_contract::FileCatalogError::Storage(_))
        ),
        "D1 read {session}"
    );
    let mut replacement = request(0, &[]);
    replacement.session_id = session.into();
    assert!(
        matches!(
            catalog.replace_artifact_capture(replacement).await,
            Err(awaken_resource_contract::FileCatalogError::Storage(_))
        ),
        "D1 replace {session}"
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_corrupt_capture_is_neither_read_nor_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("corrupt.db");
    let store = awaken_file_store::sqlite::SqliteFileStore::open(path.to_str().unwrap()).unwrap();
    let connection = awaken_sqlite_pool::SqliteConnectionFactory::file(path)
        .open()
        .unwrap();
    for (index, &(revision, ids)) in CORRUPT_CAPTURES.iter().enumerate() {
        let session = format!("corrupt-{index}");
        connection
            .execute(
                "INSERT INTO file_store_artifact_capture VALUES (?1,?2,?3,?4)",
                rusqlite::params!["capture-workspace", session, revision, ids],
            )
            .unwrap();
        reject_corrupt(&store, &session).await;
        let unchanged: (i64, String) = connection.query_row(
            "SELECT revision,file_ids_json FROM file_store_artifact_capture WHERE session_id=?1",
            [&session], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!(unchanged, (revision, ids.into()), "D1 no rewrite");
    }
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn memory_capture_contract() {
    contract(Arc::new(awaken_file_store::InMemoryFileStore::new())).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_capture_contract_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("files.db");
    contract(Arc::new(
        awaken_file_store::sqlite::SqliteFileStore::open(path.to_str().unwrap()).unwrap(),
    ))
    .await;
    let reopened =
        awaken_file_store::sqlite::SqliteFileStore::open(path.to_str().unwrap()).unwrap();
    assert_eq!(
        current(&reopened).await.unwrap().revision,
        6,
        "R10 durable read after reopen"
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_independent_connections_serialize_capture_creation() {
    // R8b two independently opened catalogs have no shared process mutex.
    // C1 same database + C2 both expect revision zero + C3 different valid sets
    // -> exactly one Applied and one Conflict; both connections observe the
    // same durable revision. SQL transaction locking must own serialization.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("concurrent.db");
    let left = awaken_file_store::sqlite::SqliteFileStore::open(path.to_str().unwrap()).unwrap();
    let right = awaken_file_store::sqlite::SqliteFileStore::open(path.to_str().unwrap()).unwrap();
    left.create_file(file("capture-a", "report.txt"))
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        left.replace_artifact_capture(request(0, &["capture-a"])),
        right.replace_artifact_capture(request(0, &[])),
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Applied(_)))
            .count(),
        1,
        "R8b"
    );
    assert_eq!(
        outcomes.iter().filter(|o| **o == Outcome::Conflict).count(),
        1,
        "R8b"
    );
    assert_eq!(
        current(&left).await,
        current(&right).await,
        "R8b durable agreement"
    );
    assert_eq!(current(&left).await.unwrap().revision, 1);
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_v5_upgrade_preserves_files_without_guessing_a_capture() {
    // M1 V5 contains two same-path historical Files -> V6 preserves both ids
    // and adds no guessed head. M2 the first explicit complete capture selects
    // an older id; M3 schema replay retains that exact choice and revision.
    use awaken_file_store::schema::{BUNDLE_ID, NS, file_store_bundle};
    let full = file_store_bundle().unwrap();
    let legacy =
        awaken_scoped_migration::MigrationBundle::new(BUNDLE_ID, full.migrations()[..5].to_vec())
            .unwrap();
    let connection = awaken_sqlite_pool::SqliteConnectionFactory::memory()
        .open()
        .unwrap();
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .unwrap()
        .run_bundle(&connection, &legacy)
        .unwrap();
    let store = awaken_file_store::sqlite::SqliteFileStore::over(connection);
    for record in [
        file("capture-a", "report.txt"),
        file("capture-b", "report.txt"),
    ] {
        store.create_file(record).await.unwrap();
    }
    let before = store
        .list_files("capture-workspace", Some("capture-session"))
        .await
        .unwrap();
    store.ensure_schema().unwrap();
    assert_eq!(current(&store).await, None, "M1");
    assert_eq!(
        store
            .list_files("capture-workspace", Some("capture-session"))
            .await
            .unwrap(),
        before,
        "M1"
    );
    store
        .replace_artifact_capture(request(0, &["capture-a"]))
        .await
        .unwrap();
    store.ensure_schema().unwrap();
    assert_eq!(
        current(&store).await.unwrap().file_ids,
        ["capture-a"],
        "M2/M3"
    );
    assert_eq!(current(&store).await.unwrap().revision, 1, "M3");
}

#[cfg(feature = "postgres")]
#[tokio::test]
#[ignore = "requires an isolated AWAKEN_CAPTURE_TEST_DATABASE_URL"]
async fn postgres_capture_contract_and_reconnect() {
    let url = std::env::var("AWAKEN_CAPTURE_TEST_DATABASE_URL").expect("isolated test database");
    contract(Arc::new(
        awaken_file_store::postgres::PgFileStore::connect(&url)
            .await
            .unwrap(),
    ))
    .await;
    let reopened = awaken_file_store::postgres::PgFileStore::connect(&url)
        .await
        .unwrap();
    assert_eq!(
        current(&reopened).await.unwrap().revision,
        6,
        "R10 durable read after reconnect"
    );

    // M1-M3 repeat the SQLite migration rules against an actual PostgreSQL
    // schema in the same explicitly isolated test database.
    use awaken_file_store::schema::{BUNDLE_ID, NS, file_store_bundle};
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("CREATE SCHEMA capture_migration")
        .execute(&pool)
        .await
        .unwrap();
    for (index, &(revision, ids)) in CORRUPT_CAPTURES.iter().enumerate() {
        let session = format!("corrupt-{index}");
        sqlx::query("INSERT INTO file_store_artifact_capture VALUES ($1,$2,$3,$4)")
            .bind("capture-workspace")
            .bind(&session)
            .bind(revision)
            .bind(ids)
            .execute(&pool)
            .await
            .unwrap();
        reject_corrupt(&reopened, &session).await;
        let unchanged: (i64, String) = sqlx::query_as(
            "SELECT revision,file_ids_json FROM file_store_artifact_capture WHERE session_id=$1",
        )
        .bind(&session)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(unchanged, (revision, ids.into()), "D1 no rewrite");
    }
    pool.close().await;
    let migration_url = format!("{url}?options=-c%20search_path%3Dcapture_migration");
    let pool = sqlx::PgPool::connect(&migration_url).await.unwrap();
    let full = file_store_bundle().unwrap();
    let legacy =
        awaken_scoped_migration::MigrationBundle::new(BUNDLE_ID, full.migrations()[..5].to_vec())
            .unwrap();
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .unwrap()
        .run_bundle(&legacy)
        .await
        .unwrap();
    let store = awaken_file_store::postgres::PgFileStore::with_pool(pool);
    for record in [
        file("capture-a", "report.txt"),
        file("capture-b", "report.txt"),
    ] {
        store.create_file(record).await.unwrap();
    }
    let before = store
        .list_files("capture-workspace", Some("capture-session"))
        .await
        .unwrap();
    store.ensure_schema().await.unwrap();
    assert_eq!(current(&store).await, None, "M1");
    assert_eq!(
        store
            .list_files("capture-workspace", Some("capture-session"))
            .await
            .unwrap(),
        before,
        "M1"
    );
    store
        .replace_artifact_capture(request(0, &["capture-a"]))
        .await
        .unwrap();
    store.ensure_schema().await.unwrap();
    assert_eq!(
        current(&store).await.unwrap().file_ids,
        ["capture-a"],
        "M2/M3"
    );
    assert_eq!(current(&store).await.unwrap().revision, 1, "M3");
}
