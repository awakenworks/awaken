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
    // Cause/effect decision table: C1 row={absent,present}; C2 caller expected
    // revision={current,stale,future}; C3 candidate revision={exact successor,
    // equal,jump}. E1 only present+current+successor replaces the whole row;
    // E2 every other rule returns Conflict and preserves the exact durable row.
    // The backend wrappers below apply the same rules to memory, SQLite and PG.
    let mut missing = subject("dsub_cas_missing", "org_1");
    missing.revision = 1;
    assert!(matches!(
        repo.compare_and_swap(0, missing).await,
        Err(awaken_data_subject_application::DataSubjectError::Conflict(
            _
        ))
    ));
    assert!(
        repo.get(&DataSubjectId("dsub_cas_missing".into()))
            .await
            .is_err()
    );

    let original = subject("dsub_cas", "org_1");
    repo.create(original.clone()).await.unwrap();
    assert!(matches!(
        repo.create(original.clone()).await,
        Err(awaken_data_subject_application::DataSubjectError::AlreadyExists(_))
    ));

    for (expected, revision) in [(0, 0), (0, 2), (1, 2)] {
        let mut rejected = original.clone();
        rejected.revision = revision;
        rejected.external_id = Some(format!("rejected-{expected}-{revision}"));
        assert!(matches!(
            repo.compare_and_swap(expected, rejected).await,
            Err(awaken_data_subject_application::DataSubjectError::Conflict(
                _
            ))
        ));
        assert_eq!(
            repo.get(&DataSubjectId("dsub_cas".into())).await.unwrap(),
            original
        );
    }

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

async fn erasure_checkpoint_preserves_completed_targets(repo: &dyn ErasureJobRepo) {
    // Causes: C1 old/expected presence; C2 expected exact/stale/future; C3 new
    // revision exact/equal/jump/wrap; C4 targets superset/regressive. Rules:
    // P1 absent+None+zero creates; P2 presence mismatch rejects; P3 exact
    // successor+superset accepts; P4 stale/equal/jump/exhaustion rejects;
    // P5 fresh-but-regressive rejects. Every rejection compares the entire row,
    // and P3 compares the exact candidate. P6 duplicate/reordered targets use
    // set membership; count/flags may differ and are not silently normalized.
    let id = DataSubjectId("dsub_erasure_monotonic".into());
    let first = ErasureProgress {
        completed_targets: vec![ErasureTarget::Coordinator, ErasureTarget::Resources],
        records_removed: 9,
        accountability_stamped: true,
        complete: true,
        ..ErasureProgress::default()
    };
    let mut candidate = first.clone();
    candidate.revision = 1;
    assert!(matches!(
        repo.compare_and_swap_progress(&id, Some(0), &candidate)
            .await,
        Err(awaken_data_subject_application::DataSubjectError::Conflict(
            _
        ))
    ));
    assert_eq!(repo.load(&id).await.unwrap(), None, "P2 absent frame");
    assert!(matches!(
        repo.compare_and_swap_progress(&id, None, &candidate).await,
        Err(awaken_data_subject_application::DataSubjectError::Conflict(
            _
        ))
    ));
    assert_eq!(repo.load(&id).await.unwrap(), None, "P1 requires zero");
    repo.compare_and_swap_progress(&id, None, &first)
        .await
        .unwrap();
    for (expected, revision, targets) in [
        (None, 0, first.completed_targets.clone()),
        (Some(1), 2, first.completed_targets.clone()),
        (Some(0), 0, first.completed_targets.clone()),
        (Some(0), 2, first.completed_targets.clone()),
        (Some(u64::MAX), 0, first.completed_targets.clone()),
        (Some(0), 1, vec![]),
        (Some(0), 1, vec![ErasureTarget::Coordinator]),
        (Some(0), 1, vec![ErasureTarget::Resources]),
        (
            Some(0),
            1,
            vec![ErasureTarget::Coordinator, ErasureTarget::Coordinator],
        ),
    ] {
        let regressive = ErasureProgress {
            revision,
            completed_targets: targets,
            records_removed: 0,
            accountability_stamped: false,
            complete: false,
        };
        assert!(
            matches!(
                repo.compare_and_swap_progress(&id, expected, &regressive)
                    .await,
                Err(awaken_data_subject_application::DataSubjectError::Conflict(
                    _
                ))
            ),
            "P2/P4/P5"
        );
        assert_eq!(
            repo.load(&id).await.unwrap(),
            Some(first.clone()),
            "rejected full frame"
        );
    }
    candidate.completed_targets.reverse();
    candidate.completed_targets.push(ErasureTarget::Resources);
    candidate.records_removed = 0;
    candidate.accountability_stamped = false;
    candidate.complete = false;
    repo.compare_and_swap_progress(&id, Some(0), &candidate)
        .await
        .unwrap();
    assert_eq!(
        repo.load(&id).await.unwrap(),
        Some(candidate.clone()),
        "P3/P6 exact accepted frame"
    );
    let mut next = first;
    next.revision = 2;
    // Removing a duplicate occurrence is legal; removing its target is not.
    repo.compare_and_swap_progress(&id, Some(1), &next)
        .await
        .unwrap();
    assert_eq!(
        repo.load(&id).await.unwrap(),
        Some(next.clone()),
        "P6 set semantics"
    );
    assert!(matches!(
        repo.compare_and_swap_progress(&id, Some(0), &candidate)
            .await,
        Err(awaken_data_subject_application::DataSubjectError::Conflict(
            _
        ))
    ));
    assert_eq!(
        repo.load(&id).await.unwrap(),
        Some(next),
        "P4 stale full frame"
    );
}

async fn erasure_checkpoint_competing_candidates(
    left: &dyn ErasureJobRepo,
    right: &dyn ErasureJobRepo,
) -> ErasureProgress {
    // Causes: two independent connections submit the same absent/exact base;
    // candidates have distinguishable complete payloads and preserve old targets.
    // R1 absent -> one insert winner; R2 exact -> one update winner; R3 loser
    // reload -> exact winner, never a mixture. This is finite database executable
    // evidence; engine atomicity/all schedules are not a production Kani proof.
    let id = DataSubjectId("dsub_erasure_race".into());
    for (expected, revision, targets) in [
        (None, 0, vec![ErasureTarget::Coordinator]),
        (
            Some(0),
            1,
            vec![ErasureTarget::Coordinator, ErasureTarget::Resources],
        ),
    ] {
        let first = ErasureProgress {
            revision,
            completed_targets: targets.clone(),
            records_removed: 11,
            accountability_stamped: false,
            complete: false,
        };
        let second = ErasureProgress {
            revision,
            completed_targets: targets,
            records_removed: 13,
            accountability_stamped: false,
            complete: false,
        };
        let (a, b) = tokio::join!(
            left.compare_and_swap_progress(&id, expected, &first),
            right.compare_and_swap_progress(&id, expected, &second),
        );
        let winner = match (a, b) {
            (Ok(()), Err(awaken_data_subject_application::DataSubjectError::Conflict(_))) => first,
            (Err(awaken_data_subject_application::DataSubjectError::Conflict(_)), Ok(())) => second,
            other => panic!("R1/R2 expected one winner and one conflict: {other:?}"),
        };
        assert_eq!(
            left.load(&id).await.unwrap(),
            Some(winner.clone()),
            "R3 left full frame"
        );
        assert_eq!(
            right.load(&id).await.unwrap(),
            Some(winner),
            "R3 right full frame"
        );
    }
    left.load(&id).await.unwrap().unwrap()
}

#[tokio::test]
async fn sqlite_erasure_independent_connections_preserve_winner_after_reopen() {
    // Apply R1-R3 above to independent file connections, then R4 close/reopen:
    // the entire winning checkpoint must persist, not just its revision/count.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("erasure.db");
    let path = path.to_str().unwrap();
    let winner = {
        let left = SqliteDataSubjectRepo::open(path).unwrap();
        let right = SqliteDataSubjectRepo::open(path).unwrap();
        erasure_checkpoint_competing_candidates(&left, &right).await
    };
    let reopened = SqliteDataSubjectRepo::open(path).unwrap();
    assert_eq!(
        reopened
            .load(&DataSubjectId("dsub_erasure_race".into()))
            .await
            .unwrap(),
        Some(winner)
    );
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
    erasure_checkpoint_preserves_completed_targets(&InMemoryDataSubjectRepo::new()).await;
}

#[tokio::test]
async fn sqlite_repo_conforms() {
    // Decision rule D2 selects SQLite for the same C2-C5/E1-E5 matrix as D1;
    // keeping both rules proves that the test double and durable authority obey
    // one repository contract rather than parallel semantics.
    run_all(|| Box::new(SqliteDataSubjectRepo::open_in_memory().unwrap())).await;
    erasure_checkpoint_fences_stale_writers(&SqliteDataSubjectRepo::open_in_memory().unwrap())
        .await;
    erasure_checkpoint_preserves_completed_targets(
        &SqliteDataSubjectRepo::open_in_memory().unwrap(),
    )
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
        erasure_checkpoint_preserves_completed_targets(
            &repo("t_ds_erasure_monotonic").await.unwrap(),
        )
        .await;
        let pool = schema_pool("t_ds_erasure_race").await.unwrap();
        let left = PgDataSubjectRepo::with_pool(pool.clone()).await.unwrap();
        let right = PgDataSubjectRepo::with_pool(pool.clone()).await.unwrap();
        erasure_checkpoint_competing_candidates(&left, &right).await;
        println!(
            "[executed] PostgreSQL erasure checkpoint conformance and independent transaction race"
        );
    }
}
