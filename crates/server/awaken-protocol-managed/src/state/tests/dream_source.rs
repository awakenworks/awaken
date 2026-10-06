//! Dream Session source over durable Session roots.

use super::sample_persisted;
use crate::state::test_support::{RehydrateFake, ephemeral_session_repo};
use awaken_dream_application::DreamSessionSource as _;
use awaken_session_state_contract::SessionExecutionState;
use std::sync::Arc;

#[tokio::test]
async fn dream_scans_durable_roots_that_this_replica_never_cached() {
    // Cause/effect graph: C1 the Session root is durable but absent from
    // this replica's wire cache (warm only on another replica); C2 its
    // public status, visibility, origin, update time and Workspace.
    // Effects: E1 eligible, newest first; E2 excluded.
    // | Rule | Root | Effect |
    // |---|---|---|
    // | D1 | Idle, updated after cutoff, cold cache | E1 |
    // | D2 | second eligible root, newer | E1, ordered first |
    // | D3 | Running | E2 |
    // | D4 | Dream-origin metadata | E2 |
    // | D5 | updated at/before cutoff or unrecorded | E2 |
    // | D6 | Deleted (hidden) | E2 |
    // | D7 | another Workspace | E2 |
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(ephemeral_session_repo().await);
    let root = |id: &str, updated: Option<u64>| {
        let mut root = sample_persisted(id);
        root.execution = SessionExecutionState::Idle;
        root.updated_at_unix_ms = updated;
        root
    };
    let mut running = root("dream-d3-running", Some(3_000));
    running.execution = SessionExecutionState::Running;
    let mut dream = root("dream-d4-origin", Some(3_000));
    dream
        .metadata
        .insert("awaken.session.origin".into(), "dream".into());
    let mut deleted = root("dream-d6-deleted", Some(3_000));
    deleted.disposition = awaken_session_state_contract::SessionDisposition::Deleted;
    for (workspace, session) in [
        ("workspace-a", root("dream-d1-idle", Some(2_000))),
        ("workspace-a", root("dream-d2-newer", Some(4_000))),
        ("workspace-a", running),
        ("workspace-a", dream),
        ("workspace-a", root("dream-d5-cutoff", Some(1_000))),
        ("workspace-a", root("dream-d5-unrecorded", None)),
        ("workspace-a", deleted),
        ("workspace-b", root("dream-d7-foreign", Some(3_000))),
    ] {
        let id = session.session_id.clone();
        repo.create(
            workspace,
            session,
            awaken_session_state_contract::IdempotencyRecord {
                key: format!("dream-eligibility:{}", id.as_str()),
                payload_hash: "fixture".into(),
            },
            Vec::new(),
        )
        .await
        .expect("persist durable root");
    }
    let replica = crate::ManagedState::new(RehydrateFake::default())
        .await
        .with_session_repo(repo);
    assert!(replica.list_sessions().is_empty(), "C1 cold wire cache");

    let eligible = replica
        .eligible_sessions("workspace-a", 1_000, 10)
        .await
        .expect("durable scan");
    assert_eq!(
        eligible
            .iter()
            .map(awaken_session_types::SessionId::as_str)
            .collect::<Vec<_>>(),
        vec!["dream-d2-newer", "dream-d1-idle"],
        "D1-D7"
    );
    assert_eq!(
        replica
            .eligible_sessions("workspace-a", 1_000, 1)
            .await
            .expect("limited scan")
            .len(),
        1,
        "limit keeps only the newest"
    );
}
