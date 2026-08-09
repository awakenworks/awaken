//! Backend-generic conformance for `DataSubjectRepo`, run against every store backend
//! (in-memory + sqlite), plus a sqlite cross-restart durability check.

use awaken_data_subject_application::{
    ConsentGrant, ConsentStatus, DataSubject, DataSubjectId, DataSubjectRepo, ErasureJobRepo,
    ErasureProgress, ErasureTarget, LawfulBasis, Purpose,
};
use awaken_data_subject_store::{InMemoryDataSubjectRepo, SqliteDataSubjectRepo};

fn subject(id: &str, org: &str) -> DataSubject {
    DataSubject::new(DataSubjectId(id.into()), org, 100)
}

async fn round_trip_and_scope_by_org(repo: &dyn DataSubjectRepo) {
    repo.create(subject("dsub_a", "org_1")).await.unwrap();
    repo.create(subject("dsub_b", "org_1")).await.unwrap();
    repo.create(subject("dsub_c", "org_2")).await.unwrap();

    assert_eq!(
        repo.get(&DataSubjectId("dsub_a".into())).await.unwrap().org,
        "org_1"
    );
    let org1 = repo.list("org_1").await.unwrap();
    assert_eq!(org1.len(), 2);
    assert_eq!(repo.list("org_2").await.unwrap().len(), 1);
}

async fn compare_and_swap_preserves_consents(repo: &dyn DataSubjectRepo) {
    let mut s = subject("dsub_a", "org_1");
    s.upsert_consent(ConsentGrant {
        purpose: Purpose::TelemetryContent,
        status: ConsentStatus::Granted,
        basis: LawfulBasis::Consent,
        granted_at: 1,
        version: "v1".into(),
    });
    repo.create(s).await.unwrap();
    let mut s = repo.get(&DataSubjectId("dsub_a".into())).await.unwrap();
    let expected = s.revision;
    s.revision += 1;
    s.name = Some("updated".into());
    repo.compare_and_swap(expected, s).await.unwrap();
    let got = repo.get(&DataSubjectId("dsub_a".into())).await.unwrap();
    assert_eq!(got.consents.len(), 1);
    assert_eq!(
        got.consent_ceiling(Purpose::TelemetryContent),
        awaken_data_subject_application::ContentCapture::Full
    );
}

async fn missing_is_not_found(repo: &dyn DataSubjectRepo) {
    assert!(repo.get(&DataSubjectId("ghost".into())).await.is_err());
}

async fn create_and_cas_fence_stale_writers(repo: &dyn DataSubjectRepo) {
    let original = subject("dsub_cas", "org_1");
    repo.create(original.clone()).await.unwrap();
    assert!(matches!(
        repo.create(original.clone()).await,
        Err(awaken_data_subject_application::DataSubjectError::AlreadyExists(_))
    ));

    let mut first = original.clone();
    first.revision = 1;
    first.name = Some("first".into());
    repo.compare_and_swap(0, first).await.unwrap();

    let mut stale = original;
    stale.revision = 1;
    stale.external_id = Some("stale".into());
    assert!(matches!(
        repo.compare_and_swap(0, stale).await,
        Err(awaken_data_subject_application::DataSubjectError::Conflict(
            _
        ))
    ));
    let current = repo.get(&DataSubjectId("dsub_cas".into())).await.unwrap();
    assert_eq!(current.name.as_deref(), Some("first"));
    assert_eq!(current.external_id, None, "stale effect was not written");
}

async fn erasure_checkpoint_fences_stale_writers(repo: &dyn ErasureJobRepo) {
    let id = DataSubjectId("dsub_erasure".into());
    let first = ErasureProgress {
        completed_targets: vec![ErasureTarget::Coordinator],
        records_removed: 2,
        ..ErasureProgress::default()
    };
    repo.compare_and_swap_progress(&id, None, &first)
        .await
        .unwrap();
    assert!(matches!(
        repo.compare_and_swap_progress(&id, None, &first).await,
        Err(awaken_data_subject_application::DataSubjectError::Conflict(
            _
        ))
    ));

    let mut winner = first.clone();
    winner.revision = 1;
    winner.completed_targets.push(ErasureTarget::Resources);
    winner.records_removed = 5;
    repo.compare_and_swap_progress(&id, Some(0), &winner)
        .await
        .unwrap();

    let mut stale = first;
    stale.revision = 1;
    stale.accountability_stamped = true;
    assert!(matches!(
        repo.compare_and_swap_progress(&id, Some(0), &stale).await,
        Err(awaken_data_subject_application::DataSubjectError::Conflict(
            _
        ))
    ));
    assert_eq!(repo.load(&id).await.unwrap(), Some(winner));
}

async fn run_all(make: impl Fn() -> Box<dyn DataSubjectRepo>) {
    round_trip_and_scope_by_org(&*make()).await;
    compare_and_swap_preserves_consents(&*make()).await;
    missing_is_not_found(&*make()).await;
    create_and_cas_fence_stale_writers(&*make()).await;
}

#[tokio::test]
async fn in_memory_repo_conforms() {
    // Cause/effect decision table shared with the SQLite rule below:
    // C1 backend={volatile,SQLite}; C2 row={present,absent}; C3 org={same,
    // different}; C4 operation={create,get/list,CAS}; C5 writer={fresh,
    // duplicate,stale}. E1 create round-trips; E2 list is Org-scoped; E3 CAS
    // retains consent; E4 absent get fails; E5 duplicate create/stale CAS are
    // rejected without overwriting the committed row. Rule D1 selects volatile
    // and exercises every C2-C5 outcome through `run_all`.
    run_all(|| Box::new(InMemoryDataSubjectRepo::new())).await;
    // Erasure causes: C6 checkpoint={absent,revision 0}; C7 writer={winner,
    // duplicate create,stale revision}; effects: E6 only one create/CAS wins;
    // E7 the committed target set/count never regress. Rule D3=C6+C7 -> E6+E7.
    erasure_checkpoint_fences_stale_writers(&InMemoryDataSubjectRepo::new()).await;
}

#[tokio::test]
async fn sqlite_repo_conforms() {
    // Decision rule D2 selects SQLite for the same C2-C5/E1-E5 matrix as D1;
    // keeping both rules proves that the test double and durable authority obey
    // one repository contract rather than parallel semantics.
    run_all(|| Box::new(SqliteDataSubjectRepo::open_in_memory().unwrap())).await;
    erasure_checkpoint_fences_stale_writers(&SqliteDataSubjectRepo::open_in_memory().unwrap())
        .await;
}

#[tokio::test]
async fn sqlite_survives_reopen() {
    // Causes: C1 durable SQLite closes/reopens; C2 aggregate contains consent,
    // profile facts, and revision=1. Effects: E1 all facts survive together; E2
    // the revision fence remains usable after restart. Rule S1=C1+C2 -> E1+E2.
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
        repo.create(s).await.unwrap();
        let mut updated = repo.get(&DataSubjectId("dsub_a".into())).await.unwrap();
        updated.name = Some("durable".into());
        updated.revision = 1;
        repo.compare_and_swap(0, updated).await.unwrap();
    }
    // A fresh instance over the same file sees the committed subject + grant.
    let repo = SqliteDataSubjectRepo::open(path).unwrap();
    let got = repo.get(&DataSubjectId("dsub_a".into())).await.unwrap();
    assert_eq!(
        got.consent_ceiling(Purpose::TelemetryContent),
        awaken_data_subject_application::ContentCapture::Full
    );
    assert_eq!(got.name.as_deref(), Some("durable"));
    assert_eq!(got.revision, 1);
}

/// Live Postgres conformance for the Control-owned DataSubject repository.
/// Skips when no Postgres is reachable (`AWAKEN_TEST_DATABASE_URL`).
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_data_subject_store::PgDataSubjectRepo;
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
        compare_and_swap_preserves_consents(&repo("t_ds_upsert").await.unwrap()).await;
        missing_is_not_found(&repo("t_ds_missing").await.unwrap()).await;
        create_and_cas_fence_stale_writers(&repo("t_ds_cas").await.unwrap()).await;
        erasure_checkpoint_fences_stale_writers(&repo("t_ds_erasure_cas").await.unwrap()).await;
    }
}
