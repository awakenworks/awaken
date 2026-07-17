//! Backend-generic conformance for `DataSubjectRepo`, run against every backend
//! (in-memory + sqlite), plus a sqlite cross-restart durability check. The
//! captured-content erasure/TTL/Art.18 suite is likewise backend-generic, run over
//! the same body for in-memory, sqlite, and (feature-gated) Postgres.

use async_trait::async_trait;
use awaken_data_subject::{
    ConsentGrant, ConsentStatus, DataSubject, DataSubjectId, DataSubjectRepo,
    InMemoryCapturedContentStore, InMemoryDataSubjectRepo, LawfulBasis, Purpose,
    SqliteCapturedContentStore, SqliteDataSubjectRepo,
};
use awaken_runtime_contract::{CaptureSink, ContentEraser, ContentKind};

/// Backend-generic surface for the captured-content stores, so one erasure/TTL/
/// Art.18 conformance body runs over every backend. Each backend's inherent methods
/// (sync for sqlite/in-mem, async for pg) are adapted to this async trait; the
/// content-facing ports (`CaptureSink`/`ContentEraser`) are called through their
/// real trait impls.
#[async_trait]
trait CapturedContentBackend {
    async fn insert(&self, subject: &DataSubjectId, purpose: Purpose, content: &str, now: i64);
    async fn restrict(&self, subject: &DataSubjectId) -> usize;
    async fn release(&self, subject: &DataSubjectId) -> usize;
    async fn sweep_expired(&self, ttl_millis: i64, now: i64) -> usize;
    async fn erase_subject(&self, subject: &DataSubjectId) -> usize;
    async fn len(&self) -> usize;
    async fn record(&self, subject: &DataSubjectId, purpose: Purpose, content: &str);
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
        ContentEraser::erase_subject(self, subject).await
    }
    async fn len(&self) -> usize {
        InMemoryCapturedContentStore::len(self)
    }
    async fn record(&self, subject: &DataSubjectId, purpose: Purpose, content: &str) {
        CaptureSink::record(self, subject, purpose, ContentKind::InputMessages, content).await;
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
        ContentEraser::erase_subject(self, subject).await
    }
    async fn len(&self) -> usize {
        SqliteCapturedContentStore::len(self)
    }
    async fn record(&self, subject: &DataSubjectId, purpose: Purpose, content: &str) {
        CaptureSink::record(self, subject, purpose, ContentKind::InputMessages, content).await;
    }
}

/// The captured-content erasure/TTL/Art.18 contract, identical across backends:
/// keyed erasure removes exactly a subject's non-restricted rows, a TTL sweep
/// drops aged rows, and an Art.18-restricted row is exempt from both until released.
async fn captured_content_erasure_ttl_and_restriction(store: &dyn CapturedContentBackend) {
    let a = DataSubjectId("subj_a".into());
    let b = DataSubjectId("subj_b".into());
    let tc = Purpose::TelemetryContent;

    // Explicit-time inserts so the TTL math is deterministic.
    store.insert(&a, tc, "x1", 100).await;
    store.insert(&a, tc, "x2", 100).await;
    store.insert(&b, tc, "y1", 100).await;
    assert_eq!(store.len().await, 3);

    // Erasure removes exactly the subject's (non-restricted) rows.
    assert_eq!(store.erase_subject(&a).await, 2);
    assert_eq!(store.len().await, 1, "only b's row remains");

    // TTL sweep drops b's aged row (age 900 ≥ 50ms).
    assert_eq!(store.sweep_expired(50, 1_000).await, 1);
    assert_eq!(store.len().await, 0);

    // Art. 18: a restricted row is exempt from both erasure and the TTL sweep.
    store.insert(&b, tc, "y2", 100).await;
    assert_eq!(store.restrict(&b).await, 1);
    assert_eq!(store.erase_subject(&b).await, 0, "restricted → not erased");
    assert_eq!(
        store.sweep_expired(0, i64::MAX).await,
        0,
        "restricted → not swept"
    );
    assert_eq!(store.len().await, 1);
    // Released, it becomes erasable/sweepable again.
    assert_eq!(store.release(&b).await, 1);
    assert_eq!(store.sweep_expired(0, i64::MAX).await, 1);
    assert_eq!(store.len().await, 0);

    // The CaptureSink port records a row (wall-clock time).
    store.record(&a, tc, "live").await;
    assert_eq!(store.len().await, 1);
}

#[tokio::test]
async fn in_memory_captured_content_conforms() {
    captured_content_erasure_ttl_and_restriction(&InMemoryCapturedContentStore::new()).await;
}

#[tokio::test]
async fn sqlite_captured_content_conforms() {
    let store = SqliteCapturedContentStore::open_in_memory().unwrap();
    captured_content_erasure_ttl_and_restriction(&store).await;
}

fn subject(id: &str, org: &str) -> DataSubject {
    DataSubject::new(DataSubjectId(id.into()), org, 100)
}

async fn round_trip_and_scope_by_org(repo: &dyn DataSubjectRepo) {
    repo.put(subject("dsub_a", "org_1")).await.unwrap();
    repo.put(subject("dsub_b", "org_1")).await.unwrap();
    repo.put(subject("dsub_c", "org_2")).await.unwrap();

    assert_eq!(
        repo.get(&DataSubjectId("dsub_a".into())).await.unwrap().org,
        "org_1"
    );
    let org1 = repo.list("org_1").await.unwrap();
    assert_eq!(org1.len(), 2);
    assert_eq!(repo.list("org_2").await.unwrap().len(), 1);
}

async fn put_is_upsert_preserving_consents(repo: &dyn DataSubjectRepo) {
    let mut s = subject("dsub_a", "org_1");
    s.upsert_consent(ConsentGrant {
        purpose: Purpose::TelemetryContent,
        status: ConsentStatus::Granted,
        basis: LawfulBasis::Consent,
        granted_at: 1,
        version: "v1".into(),
    });
    repo.put(s).await.unwrap();
    let got = repo.get(&DataSubjectId("dsub_a".into())).await.unwrap();
    assert_eq!(got.consents.len(), 1);
    assert_eq!(
        got.consent_ceiling(Purpose::TelemetryContent),
        awaken_data_subject::ContentCapture::Full
    );
}

async fn missing_is_not_found_and_delete_is_idempotent(repo: &dyn DataSubjectRepo) {
    assert!(repo.get(&DataSubjectId("ghost".into())).await.is_err());
    // delete of an absent row is Ok.
    repo.delete(&DataSubjectId("ghost".into())).await.unwrap();
    repo.put(subject("dsub_x", "org_1")).await.unwrap();
    repo.delete(&DataSubjectId("dsub_x".into())).await.unwrap();
    assert!(repo.get(&DataSubjectId("dsub_x".into())).await.is_err());
}

async fn run_all(make: impl Fn() -> Box<dyn DataSubjectRepo>) {
    round_trip_and_scope_by_org(&*make()).await;
    put_is_upsert_preserving_consents(&*make()).await;
    missing_is_not_found_and_delete_is_idempotent(&*make()).await;
}

#[tokio::test]
async fn in_memory_repo_conforms() {
    run_all(|| Box::new(InMemoryDataSubjectRepo::new())).await;
}

#[tokio::test]
async fn sqlite_repo_conforms() {
    run_all(|| Box::new(SqliteDataSubjectRepo::open_in_memory().unwrap())).await;
}

#[tokio::test]
async fn sqlite_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("subjects.db");
    let path = path.to_str().unwrap();

    {
        let repo = SqliteDataSubjectRepo::open(path).unwrap();
        let mut s = subject("dsub_a", "org_1");
        s.upsert_consent(ConsentGrant {
            purpose: Purpose::TelemetryContent,
            status: ConsentStatus::Granted,
            basis: LawfulBasis::Consent,
            granted_at: 1,
            version: "v1".into(),
        });
        repo.put(s).await.unwrap();
    }
    // A fresh instance over the same file sees the committed subject + grant.
    let repo = SqliteDataSubjectRepo::open(path).unwrap();
    let got = repo.get(&DataSubjectId("dsub_a".into())).await.unwrap();
    assert_eq!(
        got.consent_ceiling(Purpose::TelemetryContent),
        awaken_data_subject::ContentCapture::Full
    );
}

/// Live Postgres conformance: the same `DataSubjectRepo` suites plus the
/// captured-content erasure/TTL/Art.18 semantics, each on a fresh schema so the
/// suites never see each other's rows. Skips when no Postgres is reachable
/// (`AWAKEN_TEST_DATABASE_URL`), proving the same portable bundle renders on both.
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_data_subject::{PgCapturedContentStore, PgDataSubjectRepo};
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

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
            ContentEraser::erase_subject(self, subject).await
        }
        async fn len(&self) -> usize {
            PgCapturedContentStore::len(self).await
        }
        async fn record(&self, subject: &DataSubjectId, purpose: Purpose, content: &str) {
            CaptureSink::record(self, subject, purpose, ContentKind::InputMessages, content).await;
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
            Err(err) => {
                println!("[skip] no Postgres reachable: {err}");
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
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    conn.execute(format!("SET search_path = {schema}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url())
            .await
            .ok()
    }

    async fn repo(schema: &'static str) -> Option<PgDataSubjectRepo> {
        Some(
            PgDataSubjectRepo::with_pool(schema_pool(schema).await?)
                .await
                .expect("store"),
        )
    }

    #[tokio::test]
    async fn postgres_repo_conforms() {
        let Some(r) = repo("t_ds_roundtrip").await else {
            return;
        };
        round_trip_and_scope_by_org(&r).await;
        put_is_upsert_preserving_consents(&repo("t_ds_upsert").await.unwrap()).await;
        missing_is_not_found_and_delete_is_idempotent(&repo("t_ds_missing").await.unwrap()).await;
    }

    #[tokio::test]
    async fn postgres_captured_content_erasure_ttl_and_restriction() {
        let Some(pool) = schema_pool("t_ds_captured").await else {
            return;
        };
        let store = PgCapturedContentStore::with_pool(pool).await.unwrap();
        // Same backend-generic body as in-mem/sqlite, over a fresh Postgres schema.
        captured_content_erasure_ttl_and_restriction(&store).await;
    }
}
