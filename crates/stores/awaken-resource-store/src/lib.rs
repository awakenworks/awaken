//! Durable resource-lifecycle state adapters.
//!
//! Purge intents and reverse references are resource-plane operational data. They
//! deliberately live outside IAM storage and contain no principal, role, API key
//! or policy. The same ports support the in-process local composition and a
//! replaceable durable adapter.

use std::collections::BTreeSet;

use awaken_resource_contract::{
    ResourceKind, ResourcePurgeError, ResourcePurgeIntent, ResourceReferenceKind,
    ResourceReferenceRecord, ResourceTarget,
};
#[cfg(feature = "sqlite")]
use awaken_sqlite_runtime::SharedSqliteConnection;
#[cfg(feature = "sqlite")]
use rusqlite::Connection;

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "postgres")]
mod postgres_registry;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod schema;
#[cfg(feature = "sqlite")]
mod sqlite_lifecycle;
#[cfg(feature = "sqlite")]
mod sqlite_registry;

#[cfg(feature = "postgres")]
pub use postgres::PostgresResourceStore;

/// SQLite adapter used by the durable single-machine composition.
#[cfg(feature = "sqlite")]
pub struct SqliteResourceStore {
    connection: SharedSqliteConnection,
}

#[cfg(feature = "sqlite")]
impl SqliteResourceStore {
    /// Open the same SQLite adapter against a private in-memory database.
    ///
    /// Ephemeral mode intentionally reuses the durable adapter so lifecycle,
    /// reference and fencing semantics have one implementation on a node.
    #[cfg(any(test, feature = "test-support"))]
    pub fn in_memory() -> Result<Self, ResourcePurgeError> {
        Self::open(":memory:")
    }

    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, ResourcePurgeError> {
        let connection = awaken_sqlite_runtime::SqliteConnectionFactory::file(path)
            .open()
            .map_err(|error| storage(error.to_string()))?;
        let store = Self {
            connection: SharedSqliteConnection::new(connection),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    /// Apply every Resources-owned migration scope. Registry and lifecycle keep
    /// independent ledgers because neither aggregate depends on the other's
    /// tables.
    pub fn ensure_schema(&self) -> Result<(), ResourcePurgeError> {
        let connection = self
            .connection()
            .map_err(|_| storage("resource SQLite connection was poisoned"))?;
        let lifecycle =
            schema::resource_reclamation_bundle().map_err(|error| storage(error.to_string()))?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(schema::NS)
            .map_err(|error| storage(error.to_string()))?
            .run_bundle(&connection, &lifecycle)
            .map_err(|error| storage(error.to_string()))?;
        let registry =
            schema::resource_registry_bundle().map_err(|error| storage(error.to_string()))?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(schema::REGISTRY_NS)
            .map_err(|error| storage(error.to_string()))?
            .run_bundle(&connection, &registry)
            .map(|_| ())
            .map_err(|error| storage(error.to_string()))
    }

    fn connection(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, Connection>> {
        self.connection.lock()
    }

    async fn with_connection<T, F>(&self, operation: F) -> Result<T, ResourcePurgeError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, ResourcePurgeError> + Send + 'static,
    {
        awaken_sqlite_runtime::with_connection(self.connection.clone(), operation)
            .await
            .map_err(storage)?
    }
}

pub(crate) fn validate_fence_request(
    intent_id: &str,
    target: &ResourceTarget,
) -> Result<(), ResourcePurgeError> {
    if intent_id.trim().is_empty()
        || target.workspace_id.trim().is_empty()
        || target.resource_id.trim().is_empty()
    {
        Err(ResourcePurgeError::Invalid(
            "reclamation fence fields must not be empty".into(),
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn validate_reference(
    record: &ResourceReferenceRecord,
) -> Result<(), ResourcePurgeError> {
    if record.target.workspace_id.trim().is_empty()
        || record.target.resource_id.trim().is_empty()
        || record.reference.reference_id.trim().is_empty()
    {
        return Err(ResourcePurgeError::Invalid(
            "resource reference fields must not be empty".into(),
        ));
    }
    Ok(())
}

pub(crate) fn prepare_replacement(
    kind: ResourceReferenceKind,
    reference_id: &str,
    records: Vec<ResourceReferenceRecord>,
) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
    if reference_id.trim().is_empty() {
        return Err(ResourcePurgeError::Invalid(
            "replacement reference_id must not be empty".into(),
        ));
    }
    let mut identities = BTreeSet::new();
    let mut unique = Vec::with_capacity(records.len());
    for record in records {
        validate_reference(&record)?;
        if record.reference.kind != kind || record.reference.reference_id != reference_id {
            return Err(ResourcePurgeError::Invalid(
                "replacement rows must belong to the requested holder".into(),
            ));
        }
        let identity = (
            record.target.workspace_id.clone(),
            record.target.kind,
            record.target.resource_id.clone(),
        );
        if identities.insert(identity) {
            unique.push(record);
        }
    }
    Ok(unique)
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn replacement_coordinates(
    records: &[ResourceReferenceRecord],
) -> BTreeSet<(String, String, String)> {
    records
        .iter()
        .map(|record| {
            (
                record.target.workspace_id.clone(),
                kind_name(record.target.kind).to_string(),
                record.target.resource_id.clone(),
            )
        })
        .collect()
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn encode_intent(intent: &ResourcePurgeIntent) -> Result<String, ResourcePurgeError> {
    serde_json::to_string(intent).map_err(|error| storage(error.to_string()))
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn decode_intent(data: &str) -> Result<ResourcePurgeIntent, ResourcePurgeError> {
    let intent: ResourcePurgeIntent =
        serde_json::from_str(data).map_err(|error| storage(error.to_string()))?;
    intent.validate()?;
    Ok(intent)
}

pub(crate) fn storage(error: impl ToString) -> ResourcePurgeError {
    ResourcePurgeError::Storage(error.to_string())
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn to_i64(value: u64) -> Result<i64, ResourcePurgeError> {
    i64::try_from(value).map_err(|_| ResourcePurgeError::Invalid("integer exceeds i64".into()))
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn kind_name(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::File => "file",
        ResourceKind::MemoryStore => "memory_store",
        ResourceKind::Repository => "repository",
        ResourceKind::Skill => "skill",
    }
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn status_name(status: awaken_resource_contract::ResourcePurgeStatus) -> &'static str {
    use awaken_resource_contract::ResourcePurgeStatus;
    match status {
        ResourcePurgeStatus::Pending => "pending",
        ResourcePurgeStatus::Claimed => "claimed",
        ResourcePurgeStatus::Completed => "completed",
    }
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn reference_kind_name(kind: ResourceReferenceKind) -> &'static str {
    match kind {
        ResourceReferenceKind::LogicalLifecycle => "logical_lifecycle",
        ResourceReferenceKind::WorkspaceOwnership => "workspace_ownership",
        ResourceReferenceKind::AgentBinding => "agent_binding",
        ResourceReferenceKind::SessionBinding => "session_binding",
        ResourceReferenceKind::Artifact => "artifact",
        ResourceReferenceKind::RuntimeHandle => "runtime_handle",
        ResourceReferenceKind::ExtractionIntent => "extraction_intent",
        ResourceReferenceKind::RetentionHold => "retention_hold",
    }
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn parse_reference_kind(
    value: &str,
) -> Result<ResourceReferenceKind, ResourcePurgeError> {
    match value {
        "logical_lifecycle" => Ok(ResourceReferenceKind::LogicalLifecycle),
        // Read the pre-separation spelling so an upgrade cannot make an existing
        // File ownership edge invisible to reclamation safety checks.
        "workspace_ownership" | "workspace_grant" => Ok(ResourceReferenceKind::WorkspaceOwnership),
        "agent_binding" => Ok(ResourceReferenceKind::AgentBinding),
        "session_binding" => Ok(ResourceReferenceKind::SessionBinding),
        "artifact" => Ok(ResourceReferenceKind::Artifact),
        "runtime_handle" => Ok(ResourceReferenceKind::RuntimeHandle),
        "extraction_intent" => Ok(ResourceReferenceKind::ExtractionIntent),
        "retention_hold" => Ok(ResourceReferenceKind::RetentionHold),
        other => Err(storage(format!(
            "unknown resource reference kind `{other}`"
        ))),
    }
}

#[cfg(feature = "sqlite")]
fn reference_params(record: &ResourceReferenceRecord) -> [String; 5] {
    [
        record.target.workspace_id.clone(),
        kind_name(record.target.kind).into(),
        record.target.resource_id.clone(),
        reference_kind_name(record.reference.kind).into(),
        record.reference.reference_id.clone(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{
        AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourcePurgeRepository,
        ResourcePurgeStatus, ResourceReclamationFence, ResourceReference, ResourceReferenceIndex,
        ResourceReferenceKind,
    };
    use proptest::prelude::*;

    fn intent(id: &str) -> ResourcePurgeIntent {
        ResourcePurgeIntent::new(
            id,
            format!("delete:{id}"),
            ResourceTarget::new("workspace-a", ResourceKind::MemoryStore, "memory-1"),
            Some(3),
            10,
            20,
        )
        .unwrap()
    }

    fn reference(workspace: &str, holder: &str) -> ResourceReferenceRecord {
        ResourceReferenceRecord {
            target: ResourceTarget::new(workspace, ResourceKind::File, "hash-1"),
            reference: ResourceReference {
                kind: ResourceReferenceKind::WorkspaceOwnership,
                reference_id: holder.into(),
            },
        }
    }

    async fn repository_spec(store: &(impl ResourcePurgeRepository + ResourceReferenceIndex)) {
        assert_eq!(
            store.put(intent("purge-1")).await.unwrap(),
            PutResourcePurgeOutcome::Inserted
        );
        assert_eq!(
            store.put(intent("purge-1")).await.unwrap(),
            PutResourcePurgeOutcome::Existing
        );
        assert!(store.recoverable(19, 10).await.unwrap().is_empty());
        let mut value = store.recoverable(20, 10).await.unwrap().remove(0);
        let revision = value.revision;
        value.claim("worker", 20, 10).unwrap();
        store.save(revision, value.clone()).await.unwrap();
        assert!(store.recoverable(29, 10).await.unwrap().is_empty());
        assert_eq!(store.recoverable(30, 10).await.unwrap().len(), 1);
        assert!(matches!(
            store.save(revision, value).await,
            Err(ResourcePurgeError::RevisionConflict(_))
        ));

        let a = reference("workspace-a", "ownership-a");
        let b = reference("workspace-b", "ownership-b");
        assert!(store.add_reference(a.clone()).await.unwrap());
        assert!(!store.add_reference(a.clone()).await.unwrap());
        assert!(store.add_reference(b.clone()).await.unwrap());
        assert_eq!(
            store.references(&a.target).await.unwrap(),
            vec![a.reference.clone()]
        );
        assert_eq!(
            store
                .references_for_resource(ResourceKind::File, "hash-1")
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(store.remove_reference(&b).await.unwrap());
        assert!(!store.remove_reference(&b).await.unwrap());
        assert_eq!(
            store
                .references_for_resource(ResourceKind::File, "hash-1")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(matches!(
            store.acquire_reclamation("intent-a", &a.target).await.unwrap(),
            AcquireResourceReclamationOutcome::Blocked(rows) if rows == vec![a.clone()]
        ));
        assert!(store.remove_reference(&a).await.unwrap());
        assert_eq!(
            store
                .acquire_reclamation("intent-a", &a.target)
                .await
                .unwrap(),
            AcquireResourceReclamationOutcome::Acquired
        );
        assert_eq!(
            store
                .acquire_reclamation("intent-a", &a.target)
                .await
                .unwrap(),
            AcquireResourceReclamationOutcome::AlreadyOwned
        );
        assert_eq!(
            store
                .acquire_reclamation("intent-b", &b.target)
                .await
                .unwrap(),
            AcquireResourceReclamationOutcome::Contended
        );
        assert!(matches!(
            store.add_reference(b.clone()).await,
            Err(ResourcePurgeError::ReclamationFenced {
                kind: ResourceKind::File,
                resource_id
            }) if resource_id == "hash-1"
        ));
        assert_eq!(
            store.release_reclamation("intent-b", &b.target).await,
            Err(ResourcePurgeError::StaleReclamationFence)
        );
        assert!(
            store
                .release_reclamation("intent-a", &a.target)
                .await
                .unwrap()
        );
        assert!(
            !store
                .release_reclamation("intent-a", &a.target)
                .await
                .unwrap()
        );
        assert!(store.add_reference(b.clone()).await.unwrap());
        let replacement = ResourceReferenceRecord {
            target: ResourceTarget::new("workspace-c", ResourceKind::File, "hash-2"),
            reference: ResourceReference {
                kind: ResourceReferenceKind::SessionBinding,
                reference_id: "session-1".into(),
            },
        };
        /* Replacement set decision. C1 one holder repeats an identical target
         * coordinate (for example, the same content through two mount paths).
         * E1 persist one safety edge without rejecting the valid replacement.
         * Rule D1 C1=>E1; adapter-specific SQL must not create another policy. */
        store
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                "session-1",
                vec![replacement.clone(), replacement.clone()],
            )
            .await
            .unwrap();
        assert_eq!(
            store.references(&replacement.target).await.unwrap().len(),
            1
        );
        store
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                "session-1",
                Vec::new(),
            )
            .await
            .unwrap();
        assert!(
            store
                .references(&replacement.target)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.get("purge-1").await.unwrap().unwrap().status,
            ResourcePurgeStatus::Claimed
        );
    }

    #[tokio::test]
    async fn sqlite_in_memory_conforms() {
        repository_spec(&SqliteResourceStore::in_memory().unwrap()).await;
    }

    /// Async-adapter cause/effect graph: C1 the SQLite connection is occupied;
    /// C2 two lifecycle calls queue on a two-worker runtime; C3 an authority
    /// timer becomes ready before the connection is released. Effects: E1 the
    /// timer fires within its deadline; E2 both queued operations finish after
    /// release; E3 no async method waits on a raw connection mutex.
    /// Decision rule Q1=C1+C2+C3=>E1+E2+E3.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_lifecycle_contention_does_not_starve_authority_timers() {
        let store = std::sync::Arc::new(SqliteResourceStore::in_memory().unwrap());
        let held = store.connection.clone();
        let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
        let holder = std::thread::spawn(move || {
            let _guard = held.lock().expect("Q1 connection lock");
            held_tx.send(()).expect("Q1 announce connection owner");
            std::thread::sleep(std::time::Duration::from_millis(250));
        });
        held_rx.recv().expect("Q1 connection held");

        let left_store = std::sync::Arc::clone(&store);
        let left = tokio::spawn(async move { left_store.get("missing-left").await });
        let right_store = std::sync::Arc::clone(&store);
        let right = tokio::spawn(async move { right_store.get("missing-right").await });
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            tokio::time::sleep(std::time::Duration::from_millis(10)),
        )
        .await
        .expect("Q1/E1 authority timer remains schedulable");

        holder.join().expect("Q1 release connection");
        assert_eq!(left.await.expect("Q1 left task").expect("Q1/E2"), None);
        assert_eq!(right.await.expect("Q1 right task").expect("Q1/E2"), None);
    }

    /// Replacement write-amplification cause/effect graph: C1 a holder has an
    /// exact durable target set; C2 the caller repeats that set in the same or a
    /// duplicate/reordered representation; C3 the caller supplies a genuinely
    /// changed set. Effects: E1 exact-set retries perform zero SQLite row
    /// changes; E2 normalized retries also perform zero changes; E3 a changed
    /// set is atomically persisted. The reverse-reference index remains the one
    /// deletion-safety authority; this test forbids a parallel dirty cache.
    ///
    /// | Rule | durable set | requested set | Effect |
    /// |---|---|---|---|
    /// | N1 | A | A | E1 |
    /// | N2 | A | A,A reordered | E2 |
    /// | N3 | A | B | E3 |
    #[tokio::test]
    async fn sqlite_identical_reference_replacement_is_a_storage_noop() {
        let store = SqliteResourceStore::in_memory().unwrap();
        let holder = "stable-session";
        let first = ResourceReferenceRecord {
            target: ResourceTarget::new("workspace-a", ResourceKind::File, "blob-a"),
            reference: ResourceReference {
                kind: ResourceReferenceKind::SessionBinding,
                reference_id: holder.into(),
            },
        };
        let second = ResourceReferenceRecord {
            target: ResourceTarget::new("workspace-a", ResourceKind::Skill, "skill-b"),
            reference: first.reference.clone(),
        };
        store
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                holder,
                vec![first.clone(), second.clone()],
            )
            .await
            .expect("N1 initial set");
        let before = store.connection().unwrap().total_changes();
        store
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                holder,
                vec![second.clone(), first.clone(), first.clone()],
            )
            .await
            .expect("N1-N2 repeated set");
        assert_eq!(
            store.connection().unwrap().total_changes(),
            before,
            "N1-N2/E1-E2"
        );

        store
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                holder,
                vec![second.clone()],
            )
            .await
            .expect("N3 changed set");
        assert!(
            store.connection().unwrap().total_changes() > before,
            "N3/E3"
        );
        assert!(
            store.references(&first.target).await.unwrap().is_empty(),
            "N3/E3"
        );
        assert_eq!(
            store.references(&second.target).await.unwrap(),
            vec![second.reference]
        );
    }

    /// Storage-local fence FMECA and cause/effect decision table. C1 the first
    /// reference scan is empty; C2 inserting the fence fires a storage-local
    /// hook that adds a reference; C3 the late row is valid or corrupt. Effects
    /// are E1 remove this intent's fence and return Blocked with the durable row,
    /// or E2 roll back the fence and fail closed on corrupt reference data.
    ///
    /// | Rule | Pre-scan | Post-insert row | Effect |
    /// |---|---|---|---|
    /// | S1 | empty | valid | E1 Blocked, reference retained |
    /// | S2 | empty | corrupt | E2 error, no fence committed |
    #[tokio::test]
    async fn sqlite_acquire_rechecks_references_added_by_the_fence_transaction() {
        let store = SqliteResourceStore::in_memory().unwrap();
        let target = ResourceTarget::new("workspace-a", ResourceKind::File, "late-hash");
        store
            .connection()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER inject_late_reference
                 AFTER INSERT ON resource_lifecycle_reclamation_fences
                 WHEN NEW.resource_id = 'late-hash'
                 BEGIN
                   INSERT INTO resource_lifecycle_references(
                     workspace_id, resource_kind, resource_id, reference_kind, reference_id
                   ) VALUES (
                     'workspace-a', 'file', 'late-hash', 'session_binding', 'session-late'
                   );
                 END;",
            )
            .unwrap();

        let outcome = store
            .acquire_reclamation("intent-late", &target)
            .await
            .expect("S1 storage transaction");
        assert!(
            matches!(
                &outcome,
                AcquireResourceReclamationOutcome::Blocked(rows)
                    if rows.len() == 1
                        && rows[0].reference.reference_id == "session-late"
            ),
            "S1/E1: {outcome:?}"
        );
        let fence_count: i64 = store
            .connection()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM resource_lifecycle_reclamation_fences
                 WHERE resource_kind = 'file' AND resource_id = 'late-hash'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fence_count, 0, "S1/E1");

        let corrupt_target = ResourceTarget::new("workspace-a", ResourceKind::File, "corrupt-hash");
        store
            .connection()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER inject_corrupt_late_reference
                 AFTER INSERT ON resource_lifecycle_reclamation_fences
                 WHEN NEW.resource_id = 'corrupt-hash'
                 BEGIN
                   INSERT INTO resource_lifecycle_references(
                     workspace_id, resource_kind, resource_id, reference_kind, reference_id
                   ) VALUES (
                     'workspace-a', 'file', 'corrupt-hash', 'unknown_kind', 'session-corrupt'
                   );
                 END;",
            )
            .unwrap();
        let error = store
            .acquire_reclamation("intent-corrupt", &corrupt_target)
            .await
            .expect_err("S2 corrupt storage-local row must fail closed");
        assert!(
            matches!(error, ResourcePurgeError::Storage(_)),
            "S2/E2: {error:?}"
        );
        let (fence_count, reference_count): (i64, i64) = store
            .connection()
            .unwrap()
            .query_row(
                "SELECT
                   (SELECT count(*) FROM resource_lifecycle_reclamation_fences
                    WHERE resource_kind = 'file' AND resource_id = 'corrupt-hash'),
                   (SELECT count(*) FROM resource_lifecycle_references
                    WHERE resource_kind = 'file' AND resource_id = 'corrupt-hash')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((fence_count, reference_count), (0, 0), "S2/E2");
    }

    /*
     * Concurrent idempotency decision table. C1 two independent SQLite
     * connections address one lifecycle database; C2 both submit the same valid
     * purge request concurrently; C3 a distinct request reuses that key.
     * Effects: E1 exactly one insert and one existing outcome; E2 one durable
     * intent; E3 conflict remains terminal. Rules: R1 C1+C2=>E1+E2;
     * R2 C1+C3=>E3. This test owns the cross-connection write-lock invariant;
     * repository_spec owns the sequential protocol.
     */
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_concurrent_duplicate_put_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resources.db");
        let first = std::sync::Arc::new(SqliteResourceStore::open(&path).unwrap());
        let second = std::sync::Arc::new(SqliteResourceStore::open(&path).unwrap());
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

        let submit = |store: std::sync::Arc<SqliteResourceStore>,
                      barrier: std::sync::Arc<tokio::sync::Barrier>| {
            tokio::spawn(async move {
                barrier.wait().await;
                store.put(intent("purge-concurrent")).await
            })
        };
        let (left, right) = tokio::join!(
            submit(first.clone(), barrier.clone()),
            submit(second.clone(), barrier),
        );
        let outcomes = [left.unwrap().unwrap(), right.unwrap().unwrap()];

        assert!(outcomes.contains(&PutResourcePurgeOutcome::Inserted));
        assert!(outcomes.contains(&PutResourcePurgeOutcome::Existing));
        assert!(first.get("purge-concurrent").await.unwrap().is_some());
        let conflicting = ResourcePurgeIntent::new(
            "another-intent",
            "delete:purge-concurrent",
            ResourceTarget::new("workspace-a", ResourceKind::MemoryStore, "memory-2"),
            Some(3),
            10,
            20,
        )
        .unwrap();
        assert!(matches!(
            second.put(conflicting).await,
            Err(ResourcePurgeError::IdempotencyConflict(_))
        ));
    }

    #[test]
    fn sqlite_records_the_scoped_resource_reclamation_migration() {
        let store = SqliteResourceStore::in_memory().unwrap();
        let applied = store
            .connection()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM resource_lifecycle_schema_migrations
                 WHERE bundle_id = 'awaken.resource_lifecycle' AND version = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(applied, 1);
    }

    proptest! {
        #[test]
        fn sqlite_in_memory_reference_and_fence_protocol_matches_the_small_model(
            actions in proptest::collection::vec(0u8..6, 0..128)
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let store = SqliteResourceStore::in_memory().unwrap();
                let row = reference("workspace-a", "ownership-a");
                let mut referenced = false;
                let mut fence: Option<&str> = None;
                for action in actions {
                    match action {
                        0 => {
                            let actual = store.add_reference(row.clone()).await;
                            let expected = if fence.is_some() {
                                Err(ResourcePurgeError::ReclamationFenced {
                                    kind: ResourceKind::File,
                                    resource_id: "hash-1".into(),
                                })
                            } else {
                                let inserted = !referenced;
                                referenced = true;
                                Ok(inserted)
                            };
                            prop_assert_eq!(actual, expected);
                        }
                        1 => {
                            let actual = store.remove_reference(&row).await.unwrap();
                            prop_assert_eq!(actual, referenced);
                            referenced = false;
                        }
                        2 | 3 => {
                            let owner = if action == 2 { "intent-a" } else { "intent-b" };
                            let actual = store
                                .acquire_reclamation(owner, &row.target)
                                .await
                                .unwrap();
                            let expected = match fence {
                                Some(current) if current == owner => {
                                    AcquireResourceReclamationOutcome::AlreadyOwned
                                }
                                Some(_) => AcquireResourceReclamationOutcome::Contended,
                                None if referenced => {
                                    AcquireResourceReclamationOutcome::Blocked(vec![row.clone()])
                                }
                                None => {
                                    fence = Some(owner);
                                    AcquireResourceReclamationOutcome::Acquired
                                }
                            };
                            prop_assert_eq!(actual, expected);
                        }
                        4 | 5 => {
                            let owner = if action == 4 { "intent-a" } else { "intent-b" };
                            let actual = store.release_reclamation(owner, &row.target).await;
                            let expected = match fence {
                                Some(current) if current == owner => {
                                    fence = None;
                                    Ok(true)
                                }
                                Some(_) => Err(ResourcePurgeError::StaleReclamationFence),
                                None => Ok(false),
                            };
                            prop_assert_eq!(actual, expected);
                        }
                        _ => unreachable!(),
                    }
                    prop_assert!(!(referenced && fence.is_some()));
                }
                Ok(())
            })?;
        }
    }

    #[test]
    fn legacy_workspace_grant_rows_remain_ownership_edges() {
        assert_eq!(
            parse_reference_kind("workspace_grant").unwrap(),
            ResourceReferenceKind::WorkspaceOwnership
        );
        assert_eq!(
            reference_kind_name(ResourceReferenceKind::WorkspaceOwnership),
            "workspace_ownership"
        );
    }

    #[tokio::test]
    async fn sqlite_is_durable_and_conforms() {
        // Test design — durable partition coverage: every ResourceKind is a
        // separate storage identity partition. One row per partition must survive
        // close/reopen without kind aliasing or loss.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resources.db");
        let store = SqliteResourceStore::open(&path).unwrap();
        repository_spec(&store).await;
        let kinds = [
            ResourceKind::File,
            ResourceKind::MemoryStore,
            ResourceKind::Repository,
            ResourceKind::Skill,
        ];
        for (ordinal, kind) in kinds.into_iter().enumerate() {
            store
                .add_reference(ResourceReferenceRecord {
                    target: ResourceTarget::new(
                        "workspace-all-kinds",
                        kind,
                        format!("resource-{ordinal}"),
                    ),
                    reference: ResourceReference {
                        kind: ResourceReferenceKind::LogicalLifecycle,
                        reference_id: format!("lifecycle-{ordinal}"),
                    },
                })
                .await
                .unwrap();
        }
        drop(store);
        let reopened = SqliteResourceStore::open(path).unwrap();
        assert!(reopened.get("purge-1").await.unwrap().is_some());
        assert_eq!(
            reopened
                .references_for_resource(ResourceKind::File, "hash-1")
                .await
                .unwrap()
                .len(),
            1
        );
        for (ordinal, kind) in kinds.into_iter().enumerate() {
            let rows = reopened
                .references_for_resource(kind, &format!("resource-{ordinal}"))
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].target.kind, kind);
            assert_eq!(
                rows[0].reference.reference_id,
                format!("lifecycle-{ordinal}")
            );
        }
    }
}
