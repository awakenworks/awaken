//! Backend-generic captured-content contract over every durable adapter.

use async_trait::async_trait;
use awaken_captured_content_store::{InMemoryCapturedContentStore, SqliteCapturedContentStore};
use awaken_runtime_contract::{
    CaptureError, CaptureSink, ContentEraser, ContentKind, DataSubjectId, Purpose,
};

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
        SqliteCapturedContentStore::insert(self, subject, purpose, content, now);
    }
    async fn restrict(&self, subject: &DataSubjectId) -> usize {
        SqliteCapturedContentStore::restrict(self, subject)
    }
    async fn release(&self, subject: &DataSubjectId) -> usize {
        SqliteCapturedContentStore::release(self, subject)
    }
    async fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize {
        SqliteCapturedContentStore::sweep_expired(self, ttl_millis, now)
    }
    async fn erase_subject(&self, subject: &DataSubjectId) -> usize {
        ContentEraser::erase_subject(self, subject).await.unwrap()
    }
    async fn len(&self) -> usize {
        SqliteCapturedContentStore::len(self)
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
    // R4 fresh unfenced C -> CaptureSink records it.
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
}

#[tokio::test]
async fn in_memory_captured_content_conforms() {
    captured_content_contract(&InMemoryCapturedContentStore::new()).await;
}

#[tokio::test]
async fn sqlite_captured_content_conforms() {
    captured_content_contract(&SqliteCapturedContentStore::open_in_memory().unwrap()).await;
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

    fn database_url() -> String {
        std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        })
    }

    async fn schema_pool(schema: &'static str) -> Option<PgPool> {
        let admin = match PgPool::connect(&database_url()).await {
            Ok(pool) => pool,
            Err(error) => {
                println!("[skip] no Postgres reachable: {error}");
                return None;
            }
        };
        let _ = admin
            .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
            .await;
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        admin.close().await;
        PgPoolOptions::new()
            .after_connect(move |connection, _metadata| {
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path = {schema}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url())
            .await
            .ok()
    }

    #[tokio::test]
    async fn postgres_captured_content_conforms() {
        let Some(pool) = schema_pool("t_captured_content").await else {
            return;
        };
        let store = PgCapturedContentStore::with_pool(pool).await.unwrap();
        captured_content_contract(&store).await;
    }
}
