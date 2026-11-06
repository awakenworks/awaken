// @formal surface exclude=test_only
// @formal owner crates/platform/protocols/awaken-protocol-managed/src/state/rehydration.rs
// @formal reason: Deterministic cold-cache publication and authorization regressions.
//! Ownership and content must become visible through the same cache entry.

use super::*;
use crate::state::test_support::{
    ConflictInjectingRepo, ReadControl, RehydrateFake, create_session_fixture,
    ephemeral_session_repo,
};
use crate::state::tests::sample_persisted;
use awaken_runtime_contract::runs::RunErrorKind;
use awaken_session_application::SessionAccessError;
use awaken_session_types::SessionId;
use std::time::Duration;

fn cold_state(repo: Arc<dyn awaken_session_application::SessionStore>) -> ManagedState {
    let clock = awaken_time::system_clock();
    crate::state::test_support::managed_state_from_parts(
        Arc::new(RehydrateFake::default()),
        None,
        repo,
        awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
        clock,
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
}

#[tokio::test]
async fn a_warmed_private_root_is_absent_from_the_workspace_cache_collection() {
    // The same native repository supplies the private binding and wire content.
    // Warming a private root through internal recovery cannot turn it public.
    let repo = Arc::new(ephemeral_session_repo().await);
    let private = awaken_session_contract::SessionPrivateBinding::new(
        "org-a".into(),
        "desk".into(),
        "alice".into(),
        "workspace-a".into(),
        1,
    )
    .unwrap()
    .bind_fresh(sample_persisted("private-cache-root"))
    .unwrap();
    create_session_fixture(repo.as_ref(), "workspace-a", private).await;
    create_session_fixture(
        repo.as_ref(),
        "workspace-a",
        sample_persisted("ordinary-cache-root"),
    )
    .await;
    let state = cold_state(repo);
    for id in ["private-cache-root", "ordinary-cache-root"] {
        state
            .hydrate_session_record(id, SessionRecoveryPurpose::CollectionRead)
            .await
            .unwrap();
    }
    let visible = state.list_sessions_scoped("workspace-a").unwrap();
    assert_eq!(
        visible
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["ordinary-cache-root"]
    );
}

#[tokio::test]
async fn cold_publication_exposes_owner_before_the_first_hydrator_returns() {
    // C1 purpose is Interactive, CollectionRead or FrozenControl; C2 the first
    // hydrator published its entry but cannot return; C3 a competing hydrator
    // takes the warm fast path. Every row requires a complete record + owner,
    // successful owned access, exact scoped list and foreign/missing parity.
    // The pause is after the entry write, with no lock across the await. A
    // split owners write after this observation makes the warm caller fail.
    for purpose in [
        SessionRecoveryPurpose::Interactive,
        SessionRecoveryPurpose::CollectionRead,
        SessionRecoveryPurpose::FrozenControl,
    ] {
        let id = "cold-atomic-owner";
        let repo = Arc::new(ephemeral_session_repo().await);
        create_session_fixture(repo.as_ref(), "workspace-a", sample_persisted(id)).await;
        let state = cold_state(repo);
        let (published, observed) = tokio::sync::oneshot::channel();
        let (resume, release) = tokio::sync::oneshot::channel();
        *state.cold_publication_pause.lock().unwrap() = Some(ColdPublicationPause {
            published,
            resume: release,
        });
        let first = state.hydrate_session_record(id, purpose);
        tokio::pin!(first);
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                result = &mut first => panic!("first hydrator returned before observation: {result:?}"),
                seen = observed => seen.expect("entry published"),
            }
        }).await.expect("first hydrator must reach publication");
        tokio::time::timeout(
            Duration::from_secs(10),
            state.hydrate_session_record(id, purpose),
        )
        .await
        .expect("the warm reader must not wait for the first hydrator")
        .unwrap();
        assert_eq!(state.owner_scope(id).as_deref(), Some("workspace-a"));
        let (_, owner) = crate::state::sessions::owned_session_access(&state, id)
            .await
            .unwrap();
        assert_eq!(owner, "workspace-a");
        let visible = state.list_sessions_scoped("workspace-a").unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, id);
        assert!(
            state
                .list_sessions_scoped("workspace-b")
                .unwrap()
                .is_empty()
        );
        for denied_id in [id, "absent-session"] {
            assert!(matches!(
                state
                    .roles
                    .roots
                    .authorize("workspace-b", &SessionId::from(denied_id))
                    .await,
                Err(SessionAccessError::NotFound)
            ));
        }
        resume.send(()).unwrap();
        first.await.unwrap();
        assert_eq!(state.owner_scope(id).as_deref(), Some("workspace-a"));
    }
}

#[tokio::test]
async fn racing_cold_candidate_preserves_the_already_published_complete_entry() {
    // C1 A waits in its durable root read; C2 B publishes and gains a new
    // cache revision; C3 A resumes with an older candidate. R1 C1+C2+C3 =>
    // preserve B's entire frame, including its owner, and publish no fragments.
    let id = "cold-winner";
    let inner = Arc::new(ephemeral_session_repo().await);
    create_session_fixture(inner.as_ref(), "workspace-a", sample_persisted(id)).await;
    let reads = ReadControl::default();
    let repo = Arc::new(ConflictInjectingRepo::new(inner, 0, reads.clone()));
    let state = cold_state(repo);
    let (entered, release) = reads.pause_next("session");
    let first = state.hydrate_session_record(id, SessionRecoveryPurpose::FrozenControl);
    tokio::pin!(first);
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            result = &mut first => panic!("read did not pause: {result:?}"),
            () = entered.notified() => {},
        }
    })
    .await
    .expect("first durable read must pause");
    state
        .hydrate_session_record(id, SessionRecoveryPurpose::CollectionRead)
        .await
        .unwrap();
    let winner = {
        let mut sessions = state.sessions.lock().unwrap();
        let record = sessions.get_mut(id).unwrap();
        record.advance_cache_revision().unwrap();
        record.clone()
    };
    release.notify_one();
    first.await.unwrap();
    let sessions = state.sessions.lock().unwrap();
    crate::state::session_record::assert_record_frame_eq(sessions.get(id).unwrap(), &winner);
    assert_eq!(sessions.get(id).unwrap().owner_scope(), "workspace-a");
    assert_eq!(reads.count("session"), 2, "one read per cold candidate");
}

#[tokio::test]
async fn failed_cold_reads_never_publish_content_or_an_orphan_owner() {
    // Each purpose crosses an unavailable durable root. R1 => no entry, owner,
    // or scoped list. R2 the same state after authority recovers => exact owner
    // and content together; a failed candidate cannot poison the warm fast path.
    for purpose in [
        SessionRecoveryPurpose::Interactive,
        SessionRecoveryPurpose::CollectionRead,
        SessionRecoveryPurpose::FrozenControl,
    ] {
        let id = "cold-failed-read";
        let inner = Arc::new(ephemeral_session_repo().await);
        create_session_fixture(inner.as_ref(), "workspace-a", sample_persisted(id)).await;
        let reads = ReadControl::default();
        reads.fail("session", true);
        let repo = Arc::new(ConflictInjectingRepo::new(inner, 0, reads.clone()));
        let state = cold_state(repo);
        assert!(matches!(
            state.hydrate_session_record(id, purpose).await,
            Err(StateError::Run(error)) if error.kind == RunErrorKind::Unavailable
        ));
        assert!(state.sessions.lock().unwrap().is_empty());
        assert!(state.owner_scope(id).is_none());
        assert!(
            state
                .list_sessions_scoped("workspace-a")
                .unwrap()
                .is_empty()
        );
        reads.fail("session", false);
        state.hydrate_session_record(id, purpose).await.unwrap();
        assert_eq!(state.owner_scope(id).as_deref(), Some("workspace-a"));
        assert_eq!(state.list_sessions_scoped("workspace-a").unwrap()[0].id, id);
    }
}

#[tokio::test]
async fn eviction_removes_only_the_disposable_owner_with_its_record() {
    // Fresh publication has both fields. Eviction loses both together; durable
    // authority still resolves the owner, and cold recovery reconstructs both.
    let state = ManagedState::new(RehydrateFake::default()).await;
    let created = state
        .create_session(
            serde_json::from_value(
                serde_json::json!({"agent":"coder", "environment_id":"env_local"}),
            )
            .unwrap(),
            "workspace-a",
        )
        .await
        .unwrap();
    assert_eq!(
        state.owner_scope(&created.id).as_deref(),
        Some("workspace-a")
    );
    assert_eq!(state.list_sessions_scoped("workspace-a").unwrap().len(), 1);
    assert!(
        state
            .list_sessions_scoped("workspace-b")
            .unwrap()
            .is_empty()
    );
    state.sessions.lock().unwrap().remove(&created.id).unwrap();
    assert!(
        state.owner_scope(&created.id).is_none(),
        "no orphan owner after eviction"
    );
    assert!(
        state
            .list_sessions_scoped("workspace-a")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        state.resolve_owner(&created.id).await.unwrap().as_deref(),
        Some("workspace-a")
    );
    assert!(
        state.owner_scope(&created.id).is_none(),
        "owner lookup does not create a cache entry"
    );
    state
        .hydrate_session_record(&created.id, SessionRecoveryPurpose::CollectionRead)
        .await
        .unwrap();
    assert_eq!(
        state.owner_scope(&created.id).as_deref(),
        Some("workspace-a")
    );
    assert_eq!(
        state.list_sessions_scoped("workspace-a").unwrap()[0].id,
        created.id
    );
}
