use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_environment_realization_contract::{
    EnvironmentImageBuildError, EnvironmentImageBuildRecord, EnvironmentImageBuildStore,
};
use awaken_sqlite_runtime::{SharedSqliteConnection, SqliteConnectionFactory};
use rusqlite::{Connection, OptionalExtension, params};

use crate::durable::{DurableEnvironmentImageBuildStore, RecordBackend, VersionedRecord, storage};
use crate::schema::{NS, environment_image_build_bundle};

struct SqliteRecordBackend {
    connection: SharedSqliteConnection,
}

pub fn open_sqlite_environment_image_build_store(
    path: impl AsRef<Path>,
) -> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    open(
        SqliteConnectionFactory::file(path)
            .open()
            .map_err(storage)?,
    )
}

#[cfg(any(test, feature = "test-support"))]
pub fn open_in_memory_environment_image_build_store()
-> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    open(SqliteConnectionFactory::memory().open().map_err(storage)?)
}

fn open(
    connection: Connection,
) -> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    Ok(Arc::new(DurableEnvironmentImageBuildStore::new(
        open_backend(connection)?,
    )))
}

fn open_backend(connection: Connection) -> Result<SqliteRecordBackend, EnvironmentImageBuildError> {
    let bundle = environment_image_build_bundle().map_err(storage)?;
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(storage)?
        .run_bundle(&connection, &bundle)
        .map_err(storage)?;
    Ok(SqliteRecordBackend {
        connection: SharedSqliteConnection::new(connection),
    })
}

#[async_trait]
impl RecordBackend for SqliteRecordBackend {
    async fn insert(
        &self,
        record: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let (demand, state, updated) = VersionedRecord::encode_record(record)?;
        let build_key = record.demand.build_key.clone();
        let state_kind = record.state.kind().to_owned();
        let inserted =
            awaken_sqlite_runtime::with_connection(self.connection.clone(), move |connection| {
                connection
                    .execute(
                        "INSERT INTO environment_image_build_job \
                         (build_key, version, demand_json, state_kind, state_json, updated_at_ms) \
                         VALUES (?1, 0, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
                        params![build_key, demand, state_kind, state, updated],
                    )
                    .map_err(storage)
            })
            .await
            .map_err(storage)??;
        Ok(inserted == 1)
    }

    async fn get(
        &self,
        build_key: &str,
    ) -> Result<Option<VersionedRecord>, EnvironmentImageBuildError> {
        let build_key = build_key.to_owned();
        awaken_sqlite_runtime::with_connection(self.connection.clone(), move |connection| {
            let row = connection
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
            decode_checked(&build_key, row)
        })
        .await
        .map_err(storage)?
    }

    async fn candidates(&self) -> Result<Vec<VersionedRecord>, EnvironmentImageBuildError> {
        awaken_sqlite_runtime::with_connection(self.connection.clone(), move |connection| {
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
        })
        .await
        .map_err(storage)?
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
        let state_kind = next.state.kind().to_owned();
        let build_key = next.demand.build_key.clone();
        let updated_rows = awaken_sqlite_runtime::with_connection(
            self.connection.clone(),
            move |connection| {
                connection
                    .execute(
                        "UPDATE environment_image_build_job \
                         SET version=?1, demand_json=?2, state_kind=?3, state_json=?4, updated_at_ms=?5 \
                         WHERE build_key=?6 AND version=?7",
                        params![
                            next_version,
                            demand,
                            state_kind,
                            state,
                            updated,
                            build_key,
                            expected_version
                        ],
                    )
                    .map_err(storage)
            },
        )
        .await
        .map_err(storage)??;
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
                description: None,
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

    /// Async-adapter cause/effect graph: C1 an image-build SQLite operation owns
    /// the connection; C2 two repository reads queue on a two-worker runtime;
    /// C3 the Session authority timer becomes ready first. Effects: E1 the timer
    /// fires before release; E2 both reads finish afterwards; E3 the adapter has
    /// no raw connection-mutex wait on a Tokio worker. Rule B1=C1+C2+C3=>E1-E3.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_build_contention_does_not_starve_authority_timers() {
        let backend = Arc::new(
            open_backend(SqliteConnectionFactory::memory().open().unwrap())
                .expect("B1 migrated backend"),
        );
        let held = backend.connection.clone();
        let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
        let holder = std::thread::spawn(move || {
            let _guard = held.lock().expect("B1 connection lock");
            held_tx.send(()).expect("B1 announce connection owner");
            std::thread::sleep(std::time::Duration::from_millis(250));
        });
        held_rx.recv().expect("B1 connection held");

        let left_backend = Arc::clone(&backend);
        let left = tokio::spawn(async move { left_backend.get("missing-left").await });
        let right_backend = Arc::clone(&backend);
        let right = tokio::spawn(async move { right_backend.get("missing-right").await });
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            tokio::time::sleep(std::time::Duration::from_millis(10)),
        )
        .await
        .expect("B1/E1 authority timer remains schedulable");

        holder.join().expect("B1 release connection");
        assert!(left.await.expect("B1 left task").expect("B1/E2").is_none());
        assert!(
            right
                .await
                .expect("B1 right task")
                .expect("B1/E2")
                .is_none()
        );
    }
}
