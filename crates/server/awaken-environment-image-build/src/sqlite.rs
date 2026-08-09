use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_environment_realization_contract::{
    EnvironmentImageBuildError, EnvironmentImageBuildRecord, EnvironmentImageBuildStore,
};
use rusqlite::{Connection, OptionalExtension, params};

use crate::durable::{DurableEnvironmentImageBuildStore, RecordBackend, VersionedRecord, storage};
use crate::schema::{NS, environment_image_build_bundle};

struct SqliteRecordBackend {
    connection: Mutex<Connection>,
}

pub fn open_sqlite_environment_image_build_store(
    path: impl AsRef<Path>,
) -> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    open(Connection::open(path).map_err(storage)?)
}

#[cfg(any(test, feature = "test-support"))]
pub fn open_in_memory_environment_image_build_store()
-> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    open(Connection::open_in_memory().map_err(storage)?)
}

fn open(
    connection: Connection,
) -> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    let bundle = environment_image_build_bundle().map_err(storage)?;
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(storage)?
        .run_bundle(&connection, &bundle)
        .map_err(storage)?;
    Ok(Arc::new(DurableEnvironmentImageBuildStore::new(
        SqliteRecordBackend {
            connection: Mutex::new(connection),
        },
    )))
}

#[async_trait]
impl RecordBackend for SqliteRecordBackend {
    async fn insert(
        &self,
        record: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let (demand, state, updated) = VersionedRecord::encode_record(record)?;
        let inserted = self
            .connection
            .lock()
            .map_err(|_| storage("Environment image-build SQLite mutex poisoned"))?
            .execute(
                "INSERT INTO environment_image_build_job \
                 (build_key, version, demand_json, state_kind, state_json, updated_at_ms) \
                 VALUES (?1, 0, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
                params![
                    record.demand.build_key,
                    demand,
                    record.state.kind(),
                    state,
                    updated
                ],
            )
            .map_err(storage)?;
        Ok(inserted == 1)
    }

    async fn get(
        &self,
        build_key: &str,
    ) -> Result<Option<VersionedRecord>, EnvironmentImageBuildError> {
        let row = self
            .connection
            .lock()
            .map_err(|_| storage("Environment image-build SQLite mutex poisoned"))?
            .query_row(
                "SELECT version, demand_json, state_json, updated_at_ms \
                 FROM environment_image_build_job WHERE build_key=?1",
                params![build_key],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(storage)?;
        decode_checked(build_key, row)
    }

    async fn candidates(&self) -> Result<Vec<VersionedRecord>, EnvironmentImageBuildError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| storage("Environment image-build SQLite mutex poisoned"))?;
        let mut statement = connection
            .prepare(
                "SELECT build_key, version, demand_json, state_json, updated_at_ms \
                 FROM environment_image_build_job WHERE state_kind <> 'ready' \
                 ORDER BY updated_at_ms, build_key",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(storage)?;
        rows.map(|row| {
            let (key, version, demand, state, updated) = row.map_err(storage)?;
            decode_checked(&key, Some((version, demand, state, updated)))?
                .ok_or_else(|| storage("candidate disappeared"))
        })
        .collect()
    }

    async fn compare_and_swap(
        &self,
        expected: &VersionedRecord,
        next: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let (demand, state, updated) = VersionedRecord::encode_record(next)?;
        let expected_version = i64::try_from(expected.version).map_err(storage)?;
        let next_version = expected_version
            .checked_add(1)
            .ok_or_else(|| storage("Environment image-build record version exhausted"))?;
        let updated_rows = self
            .connection
            .lock()
            .map_err(|_| storage("Environment image-build SQLite mutex poisoned"))?
            .execute(
                "UPDATE environment_image_build_job \
                 SET version=?1, demand_json=?2, state_kind=?3, state_json=?4, updated_at_ms=?5 \
                 WHERE build_key=?6 AND version=?7",
                params![
                    next_version,
                    demand,
                    next.state.kind(),
                    state,
                    updated,
                    next.demand.build_key,
                    expected_version
                ],
            )
            .map_err(storage)?;
        Ok(updated_rows == 1)
    }
}

fn decode_checked(
    build_key: &str,
    row: Option<(i64, String, String, i64)>,
) -> Result<Option<VersionedRecord>, EnvironmentImageBuildError> {
    row.map(|(version, demand, state, updated)| {
        let value = VersionedRecord::decode(version, &demand, &state, updated)?;
        if value.record.demand.build_key != build_key {
            return Err(storage("image-build key column does not match demand_json"));
        }
        Ok(value)
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use awaken_environment_contract::{
        EnvItem, EnvironmentConfig, EnvironmentPackages, EnvironmentRevision,
    };
    use awaken_environment_realization_contract::{
        EnvironmentImageBuildDemand, EnvironmentImageBuildState,
    };
    use awaken_executable_environment_contract::ExecutableEnvironmentRegistration;

    use super::*;

    fn demand() -> EnvironmentImageBuildDemand {
        let registration = ExecutableEnvironmentRegistration::new(
            EnvItem {
                id: "env-browser".into(),
                revision: EnvironmentRevision(1),
                name: "browser".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::Cloud {
                    networking: Default::default(),
                    packages: EnvironmentPackages {
                        npm: vec!["@playwright/mcp@latest".into()],
                        ..Default::default()
                    },
                },
                sandbox_policy: None,
                archived_at: None,
            },
            None,
        );
        EnvironmentImageBuildDemand::from_registration(&registration, "registry/base:1").unwrap()
    }

    #[tokio::test]
    async fn sqlite_reopens_ready_image_and_fences_stale_claims() {
        // Cause/effect decision table: R1 ensure persists Pending; R2 first claim
        // owns epoch 1; R3 expired lease is reclaimed at epoch 2 and stale epoch
        // 1 cannot complete; R4 epoch 2 completes Ready; R5 reopening the file
        // reconstructs the exact immutable image and does not expose new work.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("environment-images.db");
        let store = open_sqlite_environment_image_build_store(&path).unwrap();
        let demand = demand();
        store.ensure(demand.clone(), 100).await.unwrap();
        let stale = store
            .claim_next("worker-a", 100, 10)
            .await
            .unwrap()
            .unwrap();
        let current = store
            .claim_next("worker-b", 110, 10)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !store
                .complete(&stale, "image@sha256:stale", 111)
                .await
                .unwrap(),
            "R3"
        );
        assert!(
            store
                .complete(&current, "image@sha256:ready", 111)
                .await
                .unwrap(),
            "R4"
        );
        drop(store);

        let reopened = open_sqlite_environment_image_build_store(&path).unwrap();
        assert!(
            matches!(
                reopened.get(&demand.build_key).await.unwrap().unwrap().state,
                EnvironmentImageBuildState::Ready { ref image, .. } if image == "image@sha256:ready"
            ),
            "R5"
        );
        assert!(
            reopened
                .claim_next("worker-c", 1_000, 10)
                .await
                .unwrap()
                .is_none(),
            "R5"
        );
    }
}
