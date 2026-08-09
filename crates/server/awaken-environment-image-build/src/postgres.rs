use std::sync::Arc;

use async_trait::async_trait;
use awaken_environment_realization_contract::{
    EnvironmentImageBuildError, EnvironmentImageBuildRecord, EnvironmentImageBuildStore,
};
use sqlx::postgres::PgPool;

use crate::durable::{DurableEnvironmentImageBuildStore, RecordBackend, VersionedRecord, storage};
use crate::schema::{NS, environment_image_build_bundle};

struct PostgresRecordBackend {
    pool: PgPool,
}

pub async fn connect_postgres_environment_image_build_store(
    url: &str,
) -> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    let pool = PgPool::connect(url).await.map_err(storage)?;
    let bundle = environment_image_build_bundle().map_err(storage)?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(storage)?
        .run_bundle(&bundle)
        .await
        .map_err(storage)?;
    Ok(store(pool))
}

pub async fn connect_existing_postgres_environment_image_build_store(
    url: &str,
) -> Result<Arc<dyn EnvironmentImageBuildStore>, EnvironmentImageBuildError> {
    let pool = PgPool::connect(url).await.map_err(storage)?;
    let bundle = environment_image_build_bundle().map_err(storage)?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(storage)?
        .verify_bundle(&bundle)
        .await
        .map_err(storage)?;
    Ok(store(pool))
}

fn store(pool: PgPool) -> Arc<dyn EnvironmentImageBuildStore> {
    Arc::new(DurableEnvironmentImageBuildStore::new(
        PostgresRecordBackend { pool },
    ))
}

#[async_trait]
impl RecordBackend for PostgresRecordBackend {
    async fn insert(
        &self,
        record: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let (demand, state, updated) = VersionedRecord::encode_record(record)?;
        let inserted = sqlx::query(
            "INSERT INTO environment_image_build_job \
             (build_key, version, demand_json, state_kind, state_json, updated_at_ms) \
             VALUES ($1, 0, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(&record.demand.build_key)
        .bind(demand)
        .bind(record.state.kind())
        .bind(state)
        .bind(updated)
        .execute(&self.pool)
        .await
        .map_err(storage)?
        .rows_affected();
        Ok(inserted == 1)
    }

    async fn get(
        &self,
        build_key: &str,
    ) -> Result<Option<VersionedRecord>, EnvironmentImageBuildError> {
        let row: Option<(i64, String, String, i64)> = sqlx::query_as(
            "SELECT version, demand_json, state_json, updated_at_ms \
             FROM environment_image_build_job WHERE build_key=$1",
        )
        .bind(build_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        decode_checked(build_key, row)
    }

    async fn candidates(&self) -> Result<Vec<VersionedRecord>, EnvironmentImageBuildError> {
        let rows: Vec<(String, i64, String, String, i64)> = sqlx::query_as(
            "SELECT build_key, version, demand_json, state_json, updated_at_ms \
             FROM environment_image_build_job WHERE state_kind <> 'ready' \
             ORDER BY updated_at_ms, build_key LIMIT 256",
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
             SET version=$1, demand_json=$2, state_kind=$3, state_json=$4, updated_at_ms=$5 \
             WHERE build_key=$6 AND version=$7",
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
        .map_err(storage)?
        .rows_affected();
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
