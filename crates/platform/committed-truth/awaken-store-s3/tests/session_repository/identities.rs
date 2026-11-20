//! Existing colon-bearing identities and new UUID identities share the same layout.
use super::*;
use awaken_session_contract::{SessionRepositoryReads as _, SessionScanQuery};
use futures::TryStreamExt as _;

#[tokio::test]
async fn legacy_and_uuid_session_ids_reopen_without_rewriting_scope_or_identity() {
    let transport = support::object_transport();
    let clock = Arc::new(ManualClock::new(1_786_000_000_000));
    let engine = S3Storage::new(
        transport.clone(),
        "identities",
        clock.clone(),
        Limits::default(),
    )
    .unwrap();
    let repo = repository(engine);
    let identities = [
        ("org:legacy", "sesn_fnv1a64:0123456789abcdef".to_owned()),
        ("org:legacy", "sesn_fnv1a64_0123456789abcdef".to_owned()),
        ("ws_b", format!("sesn_{}", uuid::Uuid::now_v7().simple())),
    ];
    let mut roots = Vec::new();
    for (scope, id) in &identities {
        roots.push(
            session_store::create_session(&repo, scope, session_store::session(id, id), vec![])
                .await,
        );
    }
    drop(repo);
    let cold = repository(
        S3Storage::new(transport.clone(), "identities", clock, Limits::default()).unwrap(),
    );
    for ((scope, id), root) in identities.iter().zip(&roots) {
        assert_eq!(cold.get(&id.as_str().into()).await.unwrap(), *root);
        assert_eq!(cold.owner(&id.as_str().into()).await.unwrap(), *scope);
    }
    let legacy = cold
        .scan(SessionScanQuery::Workspace {
            workspace_id: "org:legacy",
        })
        .await
        .unwrap();
    assert_eq!(legacy.sessions.len(), 2);
    let foreign = cold
        .scan(SessionScanQuery::Workspace {
            workspace_id: "ws_b",
        })
        .await
        .unwrap();
    assert_eq!(foreign.sessions.len(), 1);
    assert_eq!(
        foreign.sessions[0].session.session_id.as_str(),
        identities[2].1
    );

    let objects = transport.list(None).try_collect::<Vec<_>>().await.unwrap();
    assert!(
        objects.iter().any(|object| object
            .location
            .as_ref()
            .contains("v1/scopes/org:legacy/sessions/sesn_fnv1a64:0123456789abcdef/")),
        "legacy IDs retain their literal path and are not rewritten"
    );
}
