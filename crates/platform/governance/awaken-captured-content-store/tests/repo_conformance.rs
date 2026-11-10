//! Backend-generic captured-content contract over every durable adapter.

use async_trait::async_trait;
use awaken_captured_content_store::{InMemoryCapturedContentStore, SqliteCapturedContentStore};
use awaken_data_governance_contract::ContentEraser;
use awaken_runtime_contract::{CaptureError, CaptureSink, ContentKind, DataSubjectId, Purpose};

#[async_trait]
trait CapturedContentBackend {
    async fn insert(&self, subject: &DataSubjectId, purpose: Purpose, content: &str, now: i64);
    async fn restrict(&self, subject: &DataSubjectId) -> usize;
    async fn release(&self, subject: &DataSubjectId) -> usize;
    async fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize;
    async fn erase_subject(&self, subject: &DataSubjectId) -> usize;
    async fn len(&self) -> usize;
    async fn record(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        content: &str,
    ) -> Result<(), CaptureError>;
}

#[async_trait]
impl CapturedContentBackend for InMemoryCapturedContentStore {
    async fn insert(&self, subject: &DataSubjectId, purpose: Purpose, content: &str, now: i64) {
        InMemoryCapturedContentStore::insert(self, subject.clone(), purpose, content, now);
    }
    async fn restrict(&self, subject: &DataSubjectId) -> usize {
        InMemoryCapturedContentStore::restrict(self, subject)
    }
    async fn release(&self, subject: &DataSubjectId) -> usize {
        InMemoryCapturedContentStore::release(self, subject)
    }
    async fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize {
        InMemoryCapturedContentStore::sweep_expired(self, ttl_millis, now)
    }
    async fn erase_subject(&self, subject: &DataSubjectId) -> usize {
        ContentEraser::erase_subject(self, subject).await.unwrap()
    }
    async fn len(&self) -> usize {
        InMemoryCapturedContentStore::len(self)
    }
    async fn record(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        content: &str,
    ) -> Result<(), CaptureError> {
        CaptureSink::record(self, subject, purpose, ContentKind::InputMessages, content).await
    }
}

#[async_trait]
impl CapturedContentBackend for SqliteCapturedContentStore {
    async fn insert(&self, subject: &DataSubjectId, purpose: Purpose, content: &str, now: i64) {
        SqliteCapturedContentStore::insert(self, subject, purpose, content, now).await;
    }
    async fn restrict(&self, subject: &DataSubjectId) -> usize {
        SqliteCapturedContentStore::restrict(self, subject).await
    }
    async fn release(&self, subject: &DataSubjectId) -> usize {
        SqliteCapturedContentStore::release(self, subject).await
    }
    async fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize {
        SqliteCapturedContentStore::sweep_expired(self, ttl_millis, now).await
    }
    async fn erase_subject(&self, subject: &DataSubjectId) -> usize {
        ContentEraser::erase_subject(self, subject).await.unwrap()
    }
    async fn len(&self) -> usize {
        SqliteCapturedContentStore::len(self).await
    }
    async fn record(
        &self,
        subject: &DataSubjectId,
        purpose: Purpose,
        content: &str,
    ) -> Result<(), CaptureError> {
        CaptureSink::record(self, subject, purpose, ContentKind::InputMessages, content).await
    }
}

async fn captured_content_contract(store: &dyn CapturedContentBackend) {
    // Cause/effect decision table shared by every backend:
    // R1 subject A rows + erase A -> only A removed and fenced; retry -> same
    // durable receipt so an ambiguous remote success is safe to replay;
    // R2 expired unrestricted B -> TTL removes B;
    // R3 restricted B -> erase and TTL retain it; release -> TTL removes it;
    // R4 fresh unfenced C -> CaptureSink records it at the injected t=500;
    // R5 TTL=10 at t=509 retains it, at t=510 removes it. Ambient time
    // would stamp a different row and fail the exact boundary sweep.
    let a = DataSubjectId("subj_a".into());
    let b = DataSubjectId("subj_b".into());
    let tc = Purpose::TelemetryContent;

    store.insert(&a, tc, "x1", 100).await;
    store.insert(&a, tc, "x2", 100).await;
    store.insert(&b, tc, "y1", 100).await;
    assert_eq!(store.erase_subject(&a).await, 2, "R1");
    assert_eq!(store.erase_subject(&a).await, 2, "R1");
    assert_eq!(store.len().await, 1, "R1");
    assert_eq!(
        store.record(&a, tc, "late").await,
        Err(CaptureError::SubjectErased),
        "R1"
    );

    assert_eq!(store.sweep_expired(50, 1_000).await, 1, "R2");
    store.insert(&b, tc, "y2", 100).await;
    assert_eq!(store.restrict(&b).await, 1, "R3");
    assert_eq!(store.erase_subject(&b).await, 0, "R3");
    assert_eq!(store.sweep_expired(0, i64::MAX).await, 0, "R3");
    assert_eq!(store.release(&b).await, 1, "R3");
    assert_eq!(store.sweep_expired(0, i64::MAX).await, 1, "R3");

    store
        .record(&DataSubjectId("subj_c".into()), tc, "live")
        .await
        .unwrap();
    assert_eq!(store.len().await, 1, "R4");
    assert_eq!(store.sweep_expired(10, 509).await, 0, "R5 before expiry");
    assert_eq!(store.sweep_expired(10, 510).await, 1, "R5 at expiry");
    assert_eq!(store.len().await, 0, "R5 no surviving capture");
}

#[tokio::test]
async fn in_memory_captured_content_conforms() {
    captured_content_contract(&InMemoryCapturedContentStore::new(std::sync::Arc::new(
        awaken_lease_testkit::ManualClock::new(500),
    )))
    .await;
}

#[tokio::test]
async fn sqlite_captured_content_conforms() {
    captured_content_contract(
        &SqliteCapturedContentStore::open_in_memory(std::sync::Arc::new(
            awaken_lease_testkit::ManualClock::new(500),
        ))
        .await
        .unwrap(),
    )
    .await;
}

#[cfg(feature = "postgres")]
mod postgres {
    use awaken_captured_content_store::PgCapturedContentStore;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    use super::*;

    #[async_trait]
    impl CapturedContentBackend for PgCapturedContentStore {
        async fn insert(&self, subject: &DataSubjectId, purpose: Purpose, content: &str, now: i64) {
            PgCapturedContentStore::insert(self, subject, purpose, content, now).await;
        }
        async fn restrict(&self, subject: &DataSubjectId) -> usize {
            PgCapturedContentStore::restrict(self, subject).await
        }
        async fn release(&self, subject: &DataSubjectId) -> usize {
            PgCapturedContentStore::release(self, subject).await
        }
        async fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize {
            PgCapturedContentStore::sweep_expired(self, ttl_millis, now).await
        }
        async fn erase_subject(&self, subject: &DataSubjectId) -> usize {
            ContentEraser::erase_subject(self, subject).await.unwrap()
        }
        async fn len(&self) -> usize {
            PgCapturedContentStore::len(self).await
        }
        async fn record(
            &self,
            subject: &DataSubjectId,
            purpose: Purpose,
            content: &str,
        ) -> Result<(), CaptureError> {
            CaptureSink::record(self, subject, purpose, ContentKind::InputMessages, content).await
        }
    }

    async fn schema_pool() -> Option<(PgPool, String)> {
        let url = awaken_reliability_testkit::postgres_fixture_url()?;
        assert!(
            !url.trim().is_empty(),
            "explicit PostgreSQL fixture URL is empty"
        );
        let admin = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
            .expect("explicit PostgreSQL fixture is unavailable");
        let schema = format!("t_captured_content_{}", uuid::Uuid::new_v4().simple());
        // Only this fixed fixture prefix and a UUID's simple hexadecimal form
        // enter schema DDL; runtime values stay bound parameters.
        admin
            .execute(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .await
            .expect("create owned schema");
        admin.close().await;
        let owned_schema = schema.clone();
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(5))
            .after_connect(move |connection, _metadata| {
                let schema = owned_schema.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(schema)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("connect owned PostgreSQL schema");
        Some((pool, schema))
    }

    #[test]
    fn postgres_fixture_absence_and_configured_failure_have_distinct_outcomes() {
        // Child rule table against the actual CapturedContent backend: R1
        // optional missing => visible skip; R2 required missing => failure;
        // R3 required empty/unreachable => failure without skip; R4 required
        // live => success under the gate. Shared testkit isolates the parent's
        // inputs while this backend keeps ownership of its schema and rows.
        awaken_reliability_testkit::assert_postgres_fixture_child_contract(
            "postgres::postgres_captured_content_conforms",
        );
    }

    #[tokio::test]
    async fn postgres_captured_content_conforms() {
        // F3 each invocation owns a random schema: it never drops a shared
        // predecessor. The live contract executes in that schema; cleanup is
        // restricted to the exact name returned by this fixture owner.
        let Some((pool, schema)) = schema_pool().await else {
            return;
        };
        let store = PgCapturedContentStore::with_pool(
            pool.clone(),
            std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(500)),
        )
        .await
        .unwrap();
        captured_content_contract(&store).await;
        let (other_pool, other_schema) = schema_pool().await.expect("explicit second fixture");
        assert_ne!(
            schema, other_schema,
            "F3 concurrent consumers own different schemas"
        );
        let other = PgCapturedContentStore::with_pool(
            other_pool.clone(),
            std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(500)),
        )
        .await
        .unwrap();
        CaptureSink::record(
            &other,
            &DataSubjectId("isolated".into()),
            Purpose::TelemetryContent,
            ContentKind::InputMessages,
            "other fixture",
        )
        .await
        .unwrap();
        pool.execute(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .await
            .expect("drop owned schema");
        pool.close().await;
        assert_eq!(
            other.len().await,
            1,
            "F3 first cleanup cannot destroy the other schema"
        );
        other_pool
            .execute(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA {other_schema} CASCADE"
            )))
            .await
            .expect("drop second owned schema");
        other_pool.close().await;
    }
}
