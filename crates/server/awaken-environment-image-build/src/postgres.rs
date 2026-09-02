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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use tokio::sync::Barrier;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_postgres_claim_reclaim_and_restart_preserve_one_exact_authority() {
        // Cause/effect graph: C1 one Pending row; C2 sixteen concurrent first
        // claimers; C3 lease reaches its exact expiry; C4 sixteen concurrent
        // reclaimers; C5 stale epoch completes; C6 current epoch completes; C7
        // the store reconnects. Effects: E1 exactly one epoch-1 winner; E2
        // exactly one epoch-2 winner; E3 stale completion is inert; E4 current
        // completion alone publishes Ready; E5 reconnect observes that same
        // durable fact. Decision rules: P1=C1+C2=>E1, P2=E1+C3+C4=>E2,
        // P3=E2+C5=>E3, P4=E2+C6=>E4, P5=E4+C7=>E5.
        const SCHEMA: &str = "t_environment_image_build_concurrency";
        let database_url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".into()
        });
        let Ok(admin) = PgPool::connect(&database_url).await else {
            eprintln!("[skip] no PostgreSQL reachable for Environment image-build concurrency");
            return;
        };
        admin
            .execute(format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE").as_str())
            .await
            .expect("P1 reset schema");
        admin
            .execute(format!("CREATE SCHEMA {SCHEMA}").as_str())
            .await
            .expect("P1 create schema");
        let pool = PgPoolOptions::new()
            .max_connections(24)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path = {SCHEMA}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("P1 schema pool");
        let bundle = environment_image_build_bundle().expect("P1 migration bundle");
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .expect("P1 migration runner")
            .run_bundle(&bundle)
            .await
            .expect("P1 migrate");
        let build_store = store(pool.clone());
        let demand = crate::test_demand();
        build_store
            .ensure(demand.clone(), 100)
            .await
            .expect("P1 pending row");

        async fn concurrent_claims(
            build_store: Arc<dyn EnvironmentImageBuildStore>,
            now_ms: u64,
        ) -> Vec<awaken_environment_realization_contract::EnvironmentImageBuildClaim> {
            const CLAIMERS: usize = 16;
            let barrier = Arc::new(Barrier::new(CLAIMERS));
            let mut tasks = Vec::with_capacity(CLAIMERS);
            for index in 0..CLAIMERS {
                let build_store = Arc::clone(&build_store);
                let barrier = Arc::clone(&barrier);
                tasks.push(tokio::spawn(async move {
                    barrier.wait().await;
                    build_store
                        .claim_next(&format!("worker-{index}"), now_ms, 10)
                        .await
                        .expect("claim")
                }));
            }
            let mut claims = Vec::new();
            for task in tasks {
                if let Some(claim) = task.await.expect("claim task") {
                    claims.push(claim);
                }
            }
            claims
        }

        let first = concurrent_claims(Arc::clone(&build_store), 100).await;
        assert_eq!(first.len(), 1, "P1/E1");
        assert_eq!(first[0].lease_epoch, 1, "P1/E1");
        let reclaimed = concurrent_claims(Arc::clone(&build_store), 110).await;
        assert_eq!(reclaimed.len(), 1, "P2/E2");
        assert_eq!(reclaimed[0].lease_epoch, 2, "P2/E2");
        assert!(
            !build_store
                .complete(&first[0], "image@sha256:stale", 111)
                .await
                .expect("P3 stale complete"),
            "P3/E3"
        );
        assert!(
            build_store
                .complete(&reclaimed[0], "image@sha256:ready", 111)
                .await
                .expect("P4 current complete"),
            "P4/E4"
        );

        pool.close().await;
        let separator = if database_url.contains('?') { '&' } else { '?' };
        let scoped_url = format!("{database_url}{separator}options=-c%20search_path%3D{SCHEMA}");
        let reopened = connect_existing_postgres_environment_image_build_store(&scoped_url)
            .await
            .expect("P5 reconnect");
        assert!(
            matches!(
                reopened.get(&demand.build_key).await.expect("P5 read").expect("P5 row").state,
                awaken_environment_realization_contract::EnvironmentImageBuildState::Ready {
                    ref image,
                    attempt: 2,
                    ..
                } if image == "image@sha256:ready"
            ),
            "P5/E5"
        );

        admin
            .execute(format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE").as_str())
            .await
            .expect("P5 cleanup schema");
        admin.close().await;
    }
}
