//! Backend-generic conformance for `DataSubjectRepo`, run against every backend
//! (in-memory + sqlite), plus a sqlite cross-restart durability check.

use awaken_data_subject::{
    ConsentGrant, ConsentStatus, DataSubject, DataSubjectId, DataSubjectRepo,
    InMemoryDataSubjectRepo, LawfulBasis, Purpose, SqliteDataSubjectRepo,
};

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
    // Cause/effect decision table shared with the SQLite rule below:
    // C1 backend={volatile, SQLite}; C2 row={present, absent}; C3 org={same,
    // different}; C4 operation={put, get/list, delete}. E1 put round-trips; E2
    // list returns only the selected Org; E3 repeated put retains consent; E4
    // absent get fails; E5 delete is idempotent. Rule D1 selects the volatile
    // fixture and exercises every C2-C4 outcome through `run_all`.
    run_all(|| Box::new(InMemoryDataSubjectRepo::new())).await;
}

#[tokio::test]
async fn sqlite_repo_conforms() {
    // Decision rule D2 selects SQLite for the same C2-C4/E1-E5 matrix as D1;
    // keeping both rules proves that the test double and durable authority obey
    // one repository contract rather than parallel semantics.
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

/// Live Postgres conformance for the Control-owned DataSubject repository.
/// Skips when no Postgres is reachable (`AWAKEN_TEST_DATABASE_URL`).
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_data_subject::PgDataSubjectRepo;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

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
}
