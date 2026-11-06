//! T14-C02 (并发替换): two replacements of the same Session prepare DIFFERENT
//! targets from the same root; only the root CAS decides the current one.
//!
//! Oracles (groups JSON order):
//! - O0 only the root authority decides the current references;
//! - O1 the reference index is repairable;
//! - O2 the loser cannot reclaim the winner's resource.
//!
//! Negative control: treating the reference index as an independent truth must
//! fail. It is executed: the loser publishes its edge, loses the CAS and its
//! repair fails, leaving the index claiming the loser's target and not the
//! winner's. An `index_blockers` verdict computed from that index disagrees
//! with the production root-backed purge guard on both targets.
//!
//! | Step | Effect |
//! |---|---|
//! | C1 | winner (old -> winner) commits through the bracket |
//! | C2 | loser (old -> loser, prepared from the same root) publishes, then is held at its CAS |
//! | C3 | repair is made to fail, loser released | Conflict; root keeps winner; index diverged |
//! | C4 | purge guard | winner target blocked by the Session, loser target free |
//! | C5 | later repair | index = {old, winner} |
use super::*;

const SESSION: &str = "reference-concurrent-replacement";

fn target(id: &str) -> awaken_resource_contract::ResourceTarget {
    awaken_resource_contract::ResourceTarget::new(
        "workspace",
        awaken_resource_contract::ResourceKind::Skill,
        id,
    )
}

/// NC mutant: blockers read from the disposable index as if it were truth.
fn index_blockers(
    references: &RecordingReferenceIndex,
    target: &awaken_resource_contract::ResourceTarget,
) -> Vec<awaken_resource_contract::ResourceReference> {
    references
        .records
        .lock()
        .unwrap()
        .iter()
        .filter(|record| &record.target == target)
        .map(|record| record.reference.clone())
        .collect()
}

#[tokio::test]
async fn concurrent_replacement_follows_the_root_and_never_frees_the_winner() {
    let durable = sqlite_session_repository("concurrent replacement repository").await;
    let repo = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let mut app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let references = Arc::new(RecordingReferenceIndex::default());
    app.set_resource_reference_authority(references.clone(), Arc::new(UnusedFileCatalog));
    let app = Arc::new(app);
    let id = SessionId::from(SESSION);
    let mut original = persisted(SESSION, false, "idle");
    original.resources =
        awaken_session_contract::SessionResourceState::from_active(skill_resources("old"));
    create(durable.as_ref(), original).await;

    // Both candidates come from the same original root.
    let mut loser = app.mutations.session_aggregate(&id).await.unwrap();
    loser.resources().prepare(skill_resources("loser")).unwrap();
    let mut winner = app.mutations.session_aggregate(&id).await.unwrap();
    winner
        .resources()
        .prepare(skill_resources("winner"))
        .unwrap();

    // C1: the winner commits.
    let _ = bracket(&app)
        .commit("workspace", winner, "replacement-winner", Vec::new())
        .await
        .expect("C1 winner commits")
        .into_session();
    let committed = durable.get(&id).await.unwrap();

    // C2: the loser publishes its edge and is held at its root CAS.
    let pause = repo.pause_next_mutation("replacement-loser");
    let subject = app.clone();
    let attempt = tokio::spawn(async move {
        bracket(&subject)
            .commit("workspace", loser, "replacement-loser", Vec::new())
            .await
            .map(crate::SnapshotCommit::into_session)
    });
    tokio::time::timeout(Duration::from_secs(5), pause.entered.notified())
        .await
        .expect("C2 the loser reaches its root CAS after publication");
    assert_eq!(
        index_blockers(&references, &target("loser")).len(),
        1,
        "C2 the loser's edge was published before its CAS"
    );

    // C3: its repair fails; the loser loses the CAS.
    references.fail_replace.store(true, Ordering::SeqCst);
    pause.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .expect("C3 conflict and failed repair terminate inline")
        .expect("C3 command did not panic");
    assert!(
        matches!(result, Err(SessionMutationError::Conflict)),
        "C3 the loser conflicts: {result:?}"
    );
    assert_eq!(
        durable.get(&id).await.unwrap(),
        committed,
        "C3/O0 root keeps the winner"
    );
    assert!(
        index_blockers(&references, &target("winner")).is_empty(),
        "C3 precondition: the diverged index lost the winner's edge"
    );

    // C4/O0/O2: the production purge guard reads root truth.
    let guard = crate::SessionResourcePurgeGuard::new(durable.clone(), Arc::new(UnusedFileCatalog));
    let session_blocker = vec![awaken_resource_contract::ResourceReference {
        kind: awaken_resource_contract::ResourceReferenceKind::SessionBinding,
        reference_id: SESSION.to_string(),
    }];
    let winner_blockers =
        awaken_resource_contract::ResourcePurgeGuard::blockers(&guard, &target("winner"), None, 0)
            .await
            .expect("C4 winner scan");
    let loser_blockers =
        awaken_resource_contract::ResourcePurgeGuard::blockers(&guard, &target("loser"), None, 0)
            .await
            .expect("C4 loser scan");
    assert_eq!(
        winner_blockers, session_blocker,
        "C4/O2 the winner stays protected"
    );
    assert!(
        loser_blockers.is_empty(),
        "C4/O0 the loser is not a current reference"
    );

    // NC: the index-as-truth verdict disagrees on both targets.
    assert_ne!(
        index_blockers(&references, &target("winner")),
        winner_blockers,
        "NC the index would free the winner"
    );
    assert_ne!(
        index_blockers(&references, &target("loser")),
        loser_blockers,
        "NC the index would keep the loser"
    );

    // C5/O1: a later repair restores the root projection.
    references.fail_replace.store(false, Ordering::SeqCst);
    bracket(&app).repair("workspace", &id).await;
    assert_eq!(
        references
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.target.resource_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["old".to_string(), "winner".to_string()]),
        "C5/O1 the index converges to root truth"
    );
    assert_eq!(
        durable.get(&id).await.unwrap(),
        committed,
        "C5 repair never writes root"
    );
}
