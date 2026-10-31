// @formal entrypoint environment_image_build.single_claim_exact_reclaim_and_stale_fence
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_executable_environment_contract::{
    EnvironmentImageBuildError, EnvironmentImageBuildRecord, EnvironmentImageBuildStore,
};
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;

use crate::durable::{DurableEnvironmentImageBuildStore, RecordBackend, VersionedRecord, storage};
use crate::schema::{NS, environment_image_build_bundle};

struct SqliteRecordBackend {
    pool: EmbeddedPool,
}

pub async fn open_sqlite_environment_image_build_store(
    path: impl AsRef<Path>,
) -> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    open(EmbeddedSqlite::file(path)).await
}

#[cfg(any(test, feature = "test-support"))]
pub async fn open_in_memory_environment_image_build_store()
-> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    open(EmbeddedSqlite::memory()).await
}

async fn open(
    location: EmbeddedSqlite,
) -> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    Ok(Arc::new(DurableEnvironmentImageBuildStore::new(
        open_backend(location).await?,
    )))
}

async fn open_backend(
    location: EmbeddedSqlite,
) -> Result<SqliteRecordBackend, EnvironmentImageBuildError> {
    let pool = location.open().await.map_err(storage)?;
    let bundle = environment_image_build_bundle().map_err(storage)?;
    awaken_sqlite_pool::held(
        SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
            .map_err(storage)?
            .run_bundle(&bundle),
    )
    .await
    .map_err(storage)?;
    Ok(SqliteRecordBackend { pool })
}

#[async_trait]
impl RecordBackend for SqliteRecordBackend {
    async fn insert(
        &self,
        record: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let (demand, state, updated) = VersionedRecord::encode_record(record)?;
        let inserted = sqlx::query(
            "INSERT INTO environment_image_build_job \
             (build_key, version, demand_json, state_kind, state_json, updated_at_ms) \
             VALUES (?1, 0, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
        )
        .bind(&record.demand.build_key)
        .bind(demand)
        .bind(record.state.kind())
        .bind(state)
        .bind(updated)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(inserted.rows_affected() == 1)
    }

    async fn get(
        &self,
        build_key: &str,
    ) -> Result<Option<VersionedRecord>, EnvironmentImageBuildError> {
        let row = sqlx::query_as::<_, (i64, String, String, i64)>(
            "SELECT version, demand_json, state_json, updated_at_ms \
             FROM environment_image_build_job WHERE build_key=?1",
        )
        .bind(build_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        decode_checked(build_key, row)
    }

    async fn candidates(&self) -> Result<Vec<VersionedRecord>, EnvironmentImageBuildError> {
        let rows = sqlx::query_as::<_, (String, i64, String, String, i64)>(
            "SELECT build_key, version, demand_json, state_json, updated_at_ms \
             FROM environment_image_build_job WHERE state_kind <> 'ready' \
             ORDER BY updated_at_ms, build_key",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|(key, version, demand, state, updated)| {
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
        let updated_rows = sqlx::query(
            "UPDATE environment_image_build_job \
             SET version=?1, demand_json=?2, state_kind=?3, state_json=?4, updated_at_ms=?5 \
             WHERE build_key=?6 AND version=?7",
        )
        .bind(next_version)
        .bind(demand)
        .bind(next.state.kind())
        .bind(state)
        .bind(updated)
        .bind(&next.demand.build_key)
        .bind(expected_version)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(updated_rows.rows_affected() == 1)
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
    use awaken_executable_environment_contract::EnvironmentImageBuildState;

    use super::*;

    #[tokio::test]
    async fn sqlite_reopens_ready_image_and_fences_stale_claims() {
        // Cause/effect decision table: R1 ensure persists Pending; R2 first claim
        // owns epoch 1; R3 expired lease is reclaimed at epoch 2 and stale epoch
        // 1 cannot complete; R4 epoch 2 completes Ready; R5 reopening the file
        // reconstructs the exact immutable image and does not expose new work.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("environment-images.db");
        let store = open_sqlite_environment_image_build_store(&path)
            .await
            .unwrap();
        let demand = crate::durable::tests::lifecycle(store.as_ref()).await;
        drop(store);

        let reopened = open_sqlite_environment_image_build_store(&path)
            .await
            .unwrap();
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
    /// fires before release; E2 both reads finish afterwards; E3 waiting for the
    /// connection occupies no Tokio worker. Rule B1=C1+C2+C3=>E1-E3.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_build_contention_does_not_starve_authority_timers() {
        let backend = Arc::new(
            open_backend(EmbeddedSqlite::memory())
                .await
                .expect("B1 migrated backend"),
        );
        let held = backend.pool.acquire().await.expect("B1 connection held");

        let left_backend = Arc::clone(&backend);
        let left = tokio::spawn(async move { left_backend.get("missing-left").await });
        let right_backend = Arc::clone(&backend);
        let right = tokio::spawn(async move { right_backend.get("missing-right").await });
        // awaken-allow(wait): B1 measures the timer itself; its prompt firing proves runtime workers are not blocked.
        let authority_timer = tokio::time::sleep(std::time::Duration::from_millis(10));
        tokio::time::timeout(std::time::Duration::from_millis(100), authority_timer)
            .await
            .expect("B1/E1 authority timer remains schedulable");

        drop(held);
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
