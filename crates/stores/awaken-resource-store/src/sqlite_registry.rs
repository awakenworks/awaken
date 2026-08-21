//! SQLite adapter for the Resource Registry aggregate repository.

use awaken_resource_contract::{
    AggregateRevision, InsertOutcome, MemoryStoreAggregate, RegistryRepositoryError,
    ReplaceOutcome, RepositoryAggregate, ResourceRegistryRepository, Stored,
};
use rusqlite::{OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::SqliteResourceStore;
use crate::schema::REGISTRY_NS;

const MEMORY: &str = "memory_store";
const REPOSITORY: &str = "repository";

fn unavailable(error: impl ToString) -> RegistryRepositoryError {
    RegistryRepositoryError::Unavailable(error.to_string())
}

fn corrupt(error: impl ToString) -> RegistryRepositoryError {
    RegistryRepositoryError::CorruptData(error.to_string())
}

impl SqliteResourceStore {
    fn registry_record<T: DeserializeOwned>(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<Option<Stored<T>>, RegistryRepositoryError> {
        let row: Option<(i64, String)> = self
            .connection()
            .query_row(
                &format!(
                    "SELECT revision, data FROM {REGISTRY_NS}_entry WHERE kind = ?1 AND id = ?2"
                ),
                params![kind, id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(unavailable)?;
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

    fn insert_registry_record<T: Serialize>(
        &self,
        kind: &str,
        id: &str,
        aggregate: &T,
    ) -> Result<InsertOutcome, RegistryRepositoryError> {
        let data = serde_json::to_string(aggregate).map_err(corrupt)?;
        match self.connection().execute(
            &format!(
                "INSERT INTO {REGISTRY_NS}_entry (kind, id, revision, data) VALUES (?1, ?2, 1, ?3)"
            ),
            params![kind, id, data],
        ) {
            Ok(_) => Ok(InsertOutcome::Inserted),
            Err(error)
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) =>
            {
                Ok(InsertOutcome::AlreadyRegistered)
            }
            Err(error) => Err(unavailable(error)),
        }
    }

    fn replace_registry_record<T: Serialize>(
        &self,
        kind: &str,
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
        let changed = self
            .connection()
            .execute(
                &format!(
                    "UPDATE {REGISTRY_NS}_entry SET revision = ?4, data = ?5 \
                     WHERE kind = ?1 AND id = ?2 AND revision = ?3"
                ),
                params![
                    kind,
                    id,
                    expected_revision.get() as i64,
                    next.get() as i64,
                    data
                ],
            )
            .map_err(unavailable)?;
        Ok(if changed == 1 {
            ReplaceOutcome::Replaced { revision: next }
        } else {
            ReplaceOutcome::ConcurrentModification
        })
    }

    fn list_memory_registry_records(
        &self,
    ) -> Result<Vec<Stored<MemoryStoreAggregate>>, RegistryRepositoryError> {
        let connection = self.connection();
        let mut statement = connection
            .prepare(&format!(
                "SELECT id, revision, data FROM {REGISTRY_NS}_entry \
                 WHERE kind = ?1 ORDER BY id"
            ))
            .map_err(unavailable)?;
        let rows = statement
            .query_map(params![MEMORY], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(unavailable)?;
        rows.map(|row| {
            let (id, revision, data) = row.map_err(unavailable)?;
            let revision = u64::try_from(revision)
                .map_err(corrupt)
                .and_then(AggregateRevision::new)?;
            let aggregate: MemoryStoreAggregate = serde_json::from_str(&data).map_err(corrupt)?;
            if aggregate.definition().id.as_str() != id {
                return Err(corrupt(format!(
                    "row id `{id}` does not match MemoryStore aggregate id `{}`",
                    aggregate.definition().id
                )));
            }
            aggregate
                .validate_integrity()
                .map_err(|error| corrupt(format!("resource `{id}`: {error}")))?;
            Ok(Stored {
                revision,
                aggregate,
            })
        })
        .collect()
    }
}

impl ResourceRegistryRepository for SqliteResourceStore {
    fn load_memory_store(
        &self,
        id: &str,
    ) -> Result<Option<Stored<MemoryStoreAggregate>>, RegistryRepositoryError> {
        let stored = self.registry_record::<MemoryStoreAggregate>(MEMORY, id)?;
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

    fn insert_memory_store(
        &self,
        aggregate: &MemoryStoreAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError> {
        self.insert_registry_record(MEMORY, aggregate.definition().id.as_str(), aggregate)
    }

    fn replace_memory_store(
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
    }

    fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<Stored<MemoryStoreAggregate>>, RegistryRepositoryError> {
        Ok(self
            .list_memory_registry_records()?
            .into_iter()
            .filter(|stored| stored.aggregate.definition().workspace_id == workspace_id)
            .collect())
    }

    fn load_repository(
        &self,
        id: &str,
    ) -> Result<Option<Stored<RepositoryAggregate>>, RegistryRepositoryError> {
        let stored = self.registry_record::<RepositoryAggregate>(REPOSITORY, id)?;
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

    fn insert_repository(
        &self,
        aggregate: &RepositoryAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError> {
        self.insert_registry_record(REPOSITORY, aggregate.definition().id.as_str(), aggregate)
    }

    fn replace_repository(
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
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

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

    #[test]
    fn published_v1_rows_upgrade_in_place_with_the_initial_revision() {
        // Migration contract:
        // R1 the published bundle id and table prefix remain physical protocol;
        // R2 applying V2 changes that table in place, defaulting existing rows
        // to revision 1; R3 the domain aggregate remains readable afterwards.
        // A renamed bundle/prefix would instead create an empty parallel store
        // and this test would fail at R2 or R3.
        let directory = tempfile::tempdir().expect("Registry directory");
        let path = directory.path().join("resources.db");
        let connection = rusqlite::Connection::open(&path).expect("open V1 database");
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
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(
            crate::schema::REGISTRY_NS,
        )
        .expect("valid published prefix")
        .run_bundle(&connection, &v1)
        .expect("apply published V1");
        let aggregate = memory();
        connection
            .execute(
                "INSERT INTO resource_catalog_entry (kind, id, data) VALUES (?1, ?2, ?3)",
                params![
                    MEMORY,
                    aggregate.definition().id.as_str(),
                    serde_json::to_string(&aggregate).expect("serialize V1 aggregate")
                ],
            )
            .expect("seed V1 row");
        drop(connection);

        let upgraded = SqliteResourceStore::open(&path).expect("apply Registry V2");
        let stored = upgraded
            .load_memory_store("memory-cas")
            .expect("read upgraded row")
            .expect("V1 row survives in-place upgrade");
        assert_eq!(stored.revision, AggregateRevision::INITIAL);
        assert_eq!(stored.aggregate, aggregate);
        let applied = upgraded
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM resource_catalog_schema_migrations \
                 WHERE bundle_id = 'awaken.resource_catalog' AND version = 2",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("read Registry migration ledger");
        assert_eq!(applied, 1);
    }

    #[test]
    fn row_identity_mismatch_fails_closed() {
        // Corruption partition: valid aggregate JSON stored under a different
        // physical id must be rejected by both point lookup and inventory. The
        // domain validates aggregate invariants; the adapter owns row identity.
        let store = SqliteResourceStore::in_memory().expect("open Registry");
        let aggregate = memory();
        store
            .connection()
            .execute(
                "INSERT INTO resource_catalog_entry (kind, id, revision, data) \
                 VALUES (?1, ?2, 1, ?3)",
                params![
                    MEMORY,
                    "aliased-row",
                    serde_json::to_string(&aggregate).expect("serialize aggregate")
                ],
            )
            .expect("seed mismatched row");
        assert!(matches!(
            store.load_memory_store("aliased-row"),
            Err(RegistryRepositoryError::CorruptData(_))
        ));
        assert!(matches!(
            store.list_memory_stores("workspace"),
            Err(RegistryRepositoryError::CorruptData(_))
        ));
    }

    #[test]
    fn concurrent_replacements_have_one_linearizable_winner() {
        // State model: both writers observe revision 1; CAS(1,A) and CAS(1,B)
        // race. Exactly one advances to revision 2, the loser reports conflict,
        // and reopen exposes one complete aggregate rather than a torn merge.
        let directory = tempfile::tempdir().expect("Registry directory");
        let path = directory.path().join("resources.db");
        let seed = SqliteResourceStore::open(&path).expect("open Registry seed");
        assert_eq!(
            seed.insert_memory_store(&memory()).expect("insert seed"),
            InsertOutcome::Inserted
        );
        drop(seed);

        let left = SqliteResourceStore::open(&path).expect("open left writer");
        let right = SqliteResourceStore::open(&path).expect("open right writer");
        let mut left_state = left
            .load_memory_store("memory-cas")
            .expect("left load")
            .expect("left aggregate");
        let mut right_state = right
            .load_memory_store("memory-cas")
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
        let left_thread = std::thread::spawn(move || {
            left_barrier.wait();
            left.replace_memory_store(left_state.revision, &left_state.aggregate)
                .expect("left CAS")
        });
        let right_barrier = barrier.clone();
        let right_thread = std::thread::spawn(move || {
            right_barrier.wait();
            right
                .replace_memory_store(right_state.revision, &right_state.aggregate)
                .expect("right CAS")
        });
        barrier.wait();
        let outcomes = [
            left_thread.join().expect("left writer joins"),
            right_thread.join().expect("right writer joins"),
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
        let reopened = SqliteResourceStore::open(path).expect("reopen Registry");
        let durable = reopened
            .load_memory_store("memory-cas")
            .expect("durable load")
            .expect("durable aggregate");
        assert_eq!(durable.revision.get(), 2);
        assert!(matches!(
            durable.aggregate.definition().name.as_str(),
            "Left" | "Right"
        ));
    }
}
