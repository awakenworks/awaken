// @formal entrypoint resource_catalog.aggregate_revision_is_strictly_monotonic_and_never_wraps
// @formal entrypoint resource_catalog.config_publication_is_exact_and_never_wraps
// @formal entrypoint resource_catalog.lifecycle_timestamp_projection_is_total_and_exact
//! SQLite adapter for the Resource Registry aggregate repository.

use async_trait::async_trait;
use awaken_resource_contract::{
    AggregateRevision, InsertOutcome, MemoryStoreAggregate, RegistryRepositoryError,
    ReplaceOutcome, RepositoryAggregate, ResourceRegistryRepository, Stored,
};
use serde::{Serialize, de::DeserializeOwned};

use crate::SqliteResourceStore;
use crate::schema::REGISTRY_NS;

const MEMORY: &str = "memory_store";
const REPOSITORY: &str = "repository";

fn indeterminate(error: impl ToString) -> RegistryRepositoryError {
    RegistryRepositoryError::Indeterminate {
        detail: error.to_string(),
    }
}

fn corrupt(error: impl ToString) -> RegistryRepositoryError {
    RegistryRepositoryError::CorruptData(error.to_string())
}

/// Any SQLite constraint failure (the primary result code of every extended
/// `SQLITE_CONSTRAINT_*` code), which an insert of an existing key raises.
fn is_constraint_violation(error: &sqlx::Error) -> bool {
    const SQLITE_CONSTRAINT: i32 = 19;
    error
        .as_database_error()
        .and_then(|error| error.code())
        .and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| code & 0xff == SQLITE_CONSTRAINT)
}

impl SqliteResourceStore {
    async fn registry_record<T: DeserializeOwned + Send + 'static>(
        &self,
        kind: &'static str,
        id: &str,
    ) -> Result<Option<Stored<T>>, RegistryRepositoryError> {
        let row: Option<(i64, String)> = sqlx::query_as(&format!(
            "SELECT revision, data FROM {REGISTRY_NS}_entry \
             WHERE kind = ?1 AND id = ?2"
        ))
        .bind(kind)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(indeterminate)?;
        row.map(|(revision, data)| {
            let revision = u64::try_from(revision)
                .map_err(corrupt)
                .and_then(AggregateRevision::new)?;
            let aggregate = serde_json::from_str(&data).map_err(corrupt)?;
            Ok(Stored {
                revision,
                aggregate,
            })
        })
        .transpose()
    }

    async fn insert_registry_record<T: Serialize>(
        &self,
        kind: &'static str,
        id: &str,
        aggregate: &T,
    ) -> Result<InsertOutcome, RegistryRepositoryError> {
        let data = serde_json::to_string(aggregate).map_err(corrupt)?;
        match sqlx::query(&format!(
            "INSERT INTO {REGISTRY_NS}_entry (kind, id, revision, data) \
             VALUES (?1, ?2, 1, ?3)"
        ))
        .bind(kind)
        .bind(id)
        .bind(data)
        .execute(&self.pool)
        .await
        {
            Ok(_) => Ok(InsertOutcome::Inserted),
            Err(error) if is_constraint_violation(&error) => Ok(InsertOutcome::AlreadyRegistered),
            Err(error) => Err(indeterminate(error)),
        }
    }

    async fn replace_registry_record<T: Serialize>(
        &self,
        kind: &'static str,
        id: &str,
        expected_revision: AggregateRevision,
        aggregate: &T,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError> {
        let next = expected_revision.checked_next().ok_or_else(|| {
            RegistryRepositoryError::CorruptData(format!(
                "resource `{id}` exhausted aggregate revisions"
            ))
        })?;
        let data = serde_json::to_string(aggregate).map_err(corrupt)?;
        let changed = sqlx::query(&format!(
            "UPDATE {REGISTRY_NS}_entry SET revision = ?4, data = ?5 \
             WHERE kind = ?1 AND id = ?2 AND revision = ?3"
        ))
        .bind(kind)
        .bind(id)
        .bind(expected_revision.get() as i64)
        .bind(next.get() as i64)
        .bind(data)
        .execute(&self.pool)
        .await
        .map_err(indeterminate)?
        .rows_affected();
        Ok(if changed == 1 {
            ReplaceOutcome::Replaced { revision: next }
        } else {
            ReplaceOutcome::ConcurrentModification
        })
    }

    async fn list_registry_records<T: DeserializeOwned>(
        &self,
        kind: &'static str,
        validate: impl Fn(&T, &str) -> Result<(), RegistryRepositoryError>,
    ) -> Result<Vec<Stored<T>>, RegistryRepositoryError> {
        let rows: Vec<(String, i64, String)> = sqlx::query_as(&format!(
            "SELECT id, revision, data FROM {REGISTRY_NS}_entry \
             WHERE kind = ?1 ORDER BY id"
        ))
        .bind(kind)
        .fetch_all(&self.pool)
        .await
        .map_err(indeterminate)?;
        rows.into_iter()
            .map(|(id, revision, data)| {
                let revision = u64::try_from(revision)
                    .map_err(corrupt)
                    .and_then(AggregateRevision::new)?;
                let aggregate: T = serde_json::from_str(&data).map_err(corrupt)?;
                validate(&aggregate, &id)?;
                Ok(Stored {
                    revision,
                    aggregate,
                })
            })
            .collect()
    }
}

#[async_trait]
impl ResourceRegistryRepository for SqliteResourceStore {
    async fn load_memory_store(
        &self,
        id: &str,
    ) -> Result<Option<Stored<MemoryStoreAggregate>>, RegistryRepositoryError> {
        let stored = self
            .registry_record::<MemoryStoreAggregate>(MEMORY, id)
            .await?;
        if let Some(stored) = &stored {
            if stored.aggregate.definition().id.as_str() != id {
                return Err(corrupt(format!(
                    "row id `{id}` does not match MemoryStore aggregate id `{}`",
                    stored.aggregate.definition().id
                )));
            }
            stored.aggregate.validate_integrity().map_err(corrupt)?;
        }
        Ok(stored)
    }

    async fn insert_memory_store(
        &self,
        aggregate: &MemoryStoreAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError> {
        self.insert_registry_record(MEMORY, aggregate.definition().id.as_str(), aggregate)
            .await
    }

    async fn replace_memory_store(
        &self,
        expected_revision: AggregateRevision,
        aggregate: &MemoryStoreAggregate,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError> {
        self.replace_registry_record(
            MEMORY,
            aggregate.definition().id.as_str(),
            expected_revision,
            aggregate,
        )
        .await
    }

    async fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<Stored<MemoryStoreAggregate>>, RegistryRepositoryError> {
        Ok(self
            .list_registry_records(MEMORY, |aggregate: &MemoryStoreAggregate, id| {
                if aggregate.definition().id.as_str() != id {
                    return Err(corrupt(format!(
                        "row id `{id}` does not match aggregate identity"
                    )));
                }
                aggregate.validate_integrity().map_err(corrupt)
            })
            .await?
            .into_iter()
            .filter(|stored| stored.aggregate.definition().workspace_id == workspace_id)
            .collect())
    }

    async fn list_repositories(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<Stored<RepositoryAggregate>>, RegistryRepositoryError> {
        Ok(self
            .list_registry_records(REPOSITORY, |aggregate: &RepositoryAggregate, id| {
                if aggregate.definition().id.as_str() != id {
                    return Err(corrupt(format!(
                        "row id `{id}` does not match aggregate identity"
                    )));
                }
                aggregate.validate_integrity().map_err(corrupt)
            })
            .await?
            .into_iter()
            .filter(|stored| stored.aggregate.definition().workspace_id == workspace_id)
            .collect())
    }

    async fn load_repository(
        &self,
        id: &str,
    ) -> Result<Option<Stored<RepositoryAggregate>>, RegistryRepositoryError> {
        let stored = self
            .registry_record::<RepositoryAggregate>(REPOSITORY, id)
            .await?;
        if let Some(stored) = &stored {
            if stored.aggregate.definition().id.as_str() != id {
                return Err(corrupt(format!(
                    "row id `{id}` does not match Repository aggregate id `{}`",
                    stored.aggregate.definition().id
                )));
            }
            stored.aggregate.validate_integrity().map_err(corrupt)?;
        }
        Ok(stored)
    }

    async fn insert_repository(
        &self,
        aggregate: &RepositoryAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError> {
        self.insert_registry_record(REPOSITORY, aggregate.definition().id.as_str(), aggregate)
            .await
    }

    async fn replace_repository(
        &self,
        expected_revision: AggregateRevision,
        aggregate: &RepositoryAggregate,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError> {
        self.replace_registry_record(
            REPOSITORY,
            aggregate.definition().id.as_str(),
            expected_revision,
            aggregate,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Barrier;

    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceState,
        RetentionPolicy,
    };

    use super::*;

    fn memory() -> MemoryStoreAggregate {
        MemoryStoreAggregate::register(
            MemoryStoreDefinition {
                id: "memory-cas".into(),
                workspace_id: "workspace".into(),
                name: "Memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            MemoryStoreConfigVersion {
                memory_store_id: "memory-cas".into(),
                version: ConfigVersion::INITIAL,
                retention_policy: RetentionPolicy::default(),
            },
        )
        .expect("valid CAS fixture")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn published_v1_rows_upgrade_in_place_with_the_initial_revision() {
        // Migration contract:
        // R1 the published bundle id and table prefix remain physical protocol;
        // R2 applying V2 changes that table in place, defaulting existing rows
        // to revision 1; R3 the domain aggregate remains readable afterwards.
        // A renamed bundle/prefix would instead create an empty parallel store
        // and this test would fail at R2 or R3.
        let directory = tempfile::tempdir().expect("Registry directory");
        let path = directory.path().join("resources.db");
        let connection = awaken_sqlite_pool::EmbeddedSqlite::file(&path)
            .open()
            .await
            .expect("open V1 database");
        let v1 = awaken_scoped_migration::MigrationBundle::new(
            crate::schema::REGISTRY_BUNDLE_ID,
            vec![
                awaken_scoped_migration::Migration::new(
                    1,
                    "create resource catalog aggregate",
                    include_str!("migrations/V0001__resource_catalog.sql").trim(),
                )
                .expect("valid published V1"),
            ],
        )
        .expect("valid published Registry bundle");
        awaken_scoped_migration::sqlite::SqliteMigrationRunner::with_prefix(
            connection.sqlx().clone(),
            crate::schema::REGISTRY_NS,
        )
        .expect("valid published prefix")
        .run_bundle(&v1)
        .await
        .expect("apply published V1");
        let aggregate = memory();
        sqlx::query("INSERT INTO resource_catalog_entry (kind, id, data) VALUES (?1, ?2, ?3)")
            .bind(MEMORY)
            .bind(aggregate.definition().id.as_str())
            .bind(serde_json::to_string(&aggregate).expect("serialize V1 aggregate"))
            .execute(&connection)
            .await
            .expect("seed V1 row");
        connection.close().await;
        drop(connection);

        let upgraded = SqliteResourceStore::open(&path)
            .await
            .expect("apply Registry V2");
        let stored = upgraded
            .load_memory_store("memory-cas")
            .await
            .expect("read upgraded row")
            .expect("V1 row survives in-place upgrade");
        assert_eq!(stored.revision, AggregateRevision::INITIAL);
        assert_eq!(stored.aggregate, aggregate);
        let applied = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM resource_catalog_schema_migrations \
             WHERE bundle_id = 'awaken.resource_catalog' AND version = 2",
        )
        .fetch_one(&upgraded.pool)
        .await
        .expect("read Registry migration ledger");
        assert_eq!(applied, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn row_identity_mismatch_fails_closed() {
        // Corruption partition: valid aggregate JSON stored under a different
        // physical id must be rejected by both point lookup and inventory. The
        // domain validates aggregate invariants; the adapter owns row identity.
        let store = SqliteResourceStore::in_memory()
            .await
            .expect("open Registry");
        let aggregate = memory();
        sqlx::query(
            "INSERT INTO resource_catalog_entry (kind, id, revision, data) \
             VALUES (?1, ?2, 1, ?3)",
        )
        .bind(MEMORY)
        .bind("aliased-row")
        .bind(serde_json::to_string(&aggregate).expect("serialize aggregate"))
        .execute(&store.pool)
        .await
        .expect("seed mismatched row");
        assert!(matches!(
            store.load_memory_store("aliased-row").await,
            Err(RegistryRepositoryError::CorruptData(_))
        ));
        assert!(matches!(
            store.list_memory_stores("workspace").await,
            Err(RegistryRepositoryError::CorruptData(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn concurrent_replacements_have_one_linearizable_winner() {
        // State model: both writers observe revision 1; CAS(1,A) and CAS(1,B)
        // race. Exactly one advances to revision 2, the loser reports conflict,
        // and reopen exposes one complete aggregate rather than a torn merge.
        let directory = tempfile::tempdir().expect("Registry directory");
        let path = directory.path().join("resources.db");
        let seed = SqliteResourceStore::open(&path)
            .await
            .expect("open Registry seed");
        assert_eq!(
            seed.insert_memory_store(&memory())
                .await
                .expect("insert seed"),
            InsertOutcome::Inserted
        );
        drop(seed);

        let left = SqliteResourceStore::open(&path)
            .await
            .expect("open left writer");
        let right = SqliteResourceStore::open(&path)
            .await
            .expect("open right writer");
        let mut left_state = left
            .load_memory_store("memory-cas")
            .await
            .expect("left load")
            .expect("left aggregate");
        let mut right_state = right
            .load_memory_store("memory-cas")
            .await
            .expect("right load")
            .expect("right aggregate");
        left_state
            .aggregate
            .update_profile("Left".into(), String::new(), Default::default(), 2);
        right_state
            .aggregate
            .update_profile("Right".into(), String::new(), Default::default(), 2);
        let barrier = Arc::new(Barrier::new(3));
        let left_barrier = barrier.clone();
        let left_writer = tokio::spawn(async move {
            left_barrier.wait().await;
            left.replace_memory_store(left_state.revision, &left_state.aggregate)
                .await
                .expect("left CAS")
        });
        let right_barrier = barrier.clone();
        let right_writer = tokio::spawn(async move {
            right_barrier.wait().await;
            right
                .replace_memory_store(right_state.revision, &right_state.aggregate)
                .await
                .expect("right CAS")
        });
        barrier.wait().await;
        let outcomes = [
            left_writer.await.expect("left writer joins"),
            right_writer.await.expect("right writer joins"),
        ];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ReplaceOutcome::Replaced { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ReplaceOutcome::ConcurrentModification))
                .count(),
            1
        );
        let reopened = SqliteResourceStore::open(path)
            .await
            .expect("reopen Registry");
        let durable = reopened
            .load_memory_store("memory-cas")
            .await
            .expect("durable load")
            .expect("durable aggregate");
        assert_eq!(durable.revision.get(), 2);
        assert!(matches!(
            durable.aggregate.definition().name.as_str(),
            "Left" | "Right"
        ));
    }
}
