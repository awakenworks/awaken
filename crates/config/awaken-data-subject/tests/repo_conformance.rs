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
