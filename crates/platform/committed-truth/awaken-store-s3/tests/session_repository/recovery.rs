//! Recovery decisions follow the current root even when a historical hint disagrees.
use super::*;
use awaken_object_storage::{SessionObjectStore as _, SessionObjectUpdate};
use awaken_session_contract::{
    SessionExecutionState, SessionReconciliation as _, SessionRepositoryReads as _,
};
use serde_json::{Value, json};

async fn released_terminal(
    repo: &awaken_session_store::ObjectManagedSessionRepository,
    id: &str,
) -> awaken_session_contract::PersistedSession {
    let mut root =
        session_store::create_session(repo, "ws_a", session_store::session(id, id), vec![]).await;
    root.execution = SessionExecutionState::Terminated;
    root.resources
        .complete_terminal_release("fixture resource release");
    root.mcp.attachments[0].state = serde_json::from_value(json!("failed")).unwrap();
    session_store::replace_session(repo, "ws_a", root, "fixture:released", vec![]).await
}

async fn replace_wire(
    store: &awaken_store_s3::S3ComponentStore,
    id: &str,
    edit: impl FnOnce(&mut Value),
) {
    let observed = store.load(id).await.unwrap().unwrap();
    let mut wire: Value = serde_json::from_str(observed.payload.as_ref().unwrap()).unwrap();
    edit(&mut wire);
    assert!(
        store
            .replace(&observed, &SessionObjectUpdate::snapshot(wire.to_string()))
            .await
            .unwrap()
    );
}

async fn wire(store: &awaken_store_s3::S3ComponentStore, id: &str) -> Value {
    serde_json::from_str(
        store
            .load(id)
            .await
            .unwrap()
            .unwrap()
            .payload
            .as_ref()
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn legacy_recovery_hints_are_rebuilt_from_the_cold_root() {
    // R1 old false + unfinished cleanup -> selected; R2 old true + verified
    // completion -> absent. Neither repair changes the root or its revision.
    let (_, _, engine, _) = support::setup().await;
    let store = engine.component_store("managed_session").unwrap();
    let repo = repository(engine.clone());
    let mut root = released_terminal(&repo, "legacy-hint").await;
    replace_wire(&store, "legacy-hint", |wire| {
        wire["reconcilable"] = json!(false)
    })
    .await;
    let cold = repository(engine.clone());
    let scan = cold.reconciliation_scan(None).await.unwrap();
    assert_eq!(scan.sessions.len(), 1, "R1 old false cannot hide cleanup");
    assert_eq!(scan.sessions[0].session, root);
    assert_eq!(wire(&store, "legacy-hint").await["reconcilable"], true);
    assert_eq!(cold.get(&root.session_id).await.unwrap(), root);

    session_store::complete_terminal_cleanup(&mut root);
    root = session_store::replace_session(&cold, "ws_a", root, "fixture:cleaned", vec![]).await;
    assert!(!root.needs_reconciliation());
    replace_wire(&store, "legacy-hint", |wire| {
        wire["reconcilable"] = json!(true)
    })
    .await;
    let cold = repository(engine);
    assert!(
        cold.reconciliation_scan(None)
            .await
            .unwrap()
            .sessions
            .is_empty(),
        "R2"
    );
    assert_eq!(wire(&store, "legacy-hint").await["reconcilable"], false);
    assert_eq!(cold.get(&root.session_id).await.unwrap(), root);
}

#[tokio::test]
async fn a_false_hint_does_not_hide_corruption_or_prevent_quarantine_repair() {
    // R1 an undecodable live root is quarantined even with an old false hint;
    // R2 restoring its exact bytes clears quarantine and repairs the hint.
    let (_, _, engine, _) = support::setup().await;
    let store = engine.component_store("managed_session").unwrap();
    let repo = repository(engine.clone());
    let root = released_terminal(&repo, "quarantine-hint").await;
    let original = wire(&store, "quarantine-hint").await["root"].clone();
    replace_wire(&store, "quarantine-hint", |wire| {
        wire["root"] = json!("{invalid-root");
        wire["reconcilable"] = json!(false);
    })
    .await;
    let scan = repository(engine.clone())
        .reconciliation_scan(None)
        .await
        .unwrap();
    assert!(scan.sessions.is_empty());
    assert_eq!(scan.quarantined.len(), 1);
    assert_eq!(scan.quarantined[0].session_id, root.session_id);
    assert!(wire(&store, "quarantine-hint").await["quarantined"].is_string());

    replace_wire(&store, "quarantine-hint", |wire| wire["root"] = original).await;
    let scan = repository(engine).reconciliation_scan(None).await.unwrap();
    assert!(scan.quarantined.is_empty());
    assert_eq!(scan.sessions[0].session, root);
    let repaired = wire(&store, "quarantine-hint").await;
    assert!(repaired["quarantined"].is_null());
    assert_eq!(repaired["reconcilable"], true);
}

#[tokio::test]
async fn recovery_cursor_advances_across_a_full_page_without_candidates() {
    // One full physical page has no domain work; its cursor must still reach
    // the later candidate. Root-less creation intents do not become quarantine.
    let (_, _, engine, _) = support::setup().await;
    let repo = repository(engine);
    repo.record_creation_intent("ws_a", &"00-intent".into(), "resource", 100)
        .await
        .unwrap();
    for index in 0..256 {
        let id = format!("quiet-{index:03}");
        let mut root = session_store::session(&id, &id);
        root.resources = Default::default();
        root.mcp.attachments[0].state = serde_json::from_value(json!("failed")).unwrap();
        assert!(!root.needs_reconciliation());
        session_store::create_session(&repo, "ws_a", root, vec![]).await;
    }
    let pending = session_store::create_session(
        &repo,
        "ws_a",
        session_store::session("zz-pending", "pending"),
        vec![],
    )
    .await;
    let first = repo.reconciliation_scan(None).await.unwrap();
    assert!(first.sessions.is_empty());
    assert!(first.quarantined.is_empty());
    let cursor = first
        .next_cursor
        .expect("an empty physical page still advances");
    let second = repo.reconciliation_scan(Some(&cursor)).await.unwrap();
    assert_eq!(second.sessions.len(), 1);
    assert_eq!(second.sessions[0].session, pending);
    assert!(second.quarantined.is_empty());
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn repairing_a_hint_cannot_overwrite_concurrent_cleanup_completion() {
    // R1 the hint-repair CAS pauses; R2 real cleanup commits first; R3 the
    // repair retries from the new root and excludes the completed Session.
    use std::sync::atomic::Ordering;
    let (objects, _, engine, _) = support::setup().await;
    let store = engine.component_store("managed_session").unwrap();
    let repo = Arc::new(repository(engine));
    let mut root = released_terminal(repo.as_ref(), "repair-race").await;
    replace_wire(&store, "repair-race", |wire| {
        wire["reconcilable"] = json!(false)
    })
    .await;
    objects.hold_next_head.store(true, Ordering::SeqCst);
    let scanning = repo.clone();
    let scan = tokio::spawn(async move { scanning.reconciliation_scan(None).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        objects.head_entered.notified(),
    )
    .await
    .expect("hint repair reaches the real Head CAS");
    session_store::complete_terminal_cleanup(&mut root);
    root =
        session_store::replace_session(repo.as_ref(), "ws_a", root, "fixture:race-cleaned", vec![])
            .await;
    objects.head_resume.notify_one();
    assert!(scan.await.unwrap().unwrap().sessions.is_empty());
    assert_eq!(repo.get(&root.session_id).await.unwrap(), root);
    assert_eq!(wire(&store, "repair-race").await["reconcilable"], false);
}
