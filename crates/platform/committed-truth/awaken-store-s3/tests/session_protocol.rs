// @formal evidence store_history.s3_sealed_collection_preserves_reachable_objects
mod support;
use awaken_agent_contract::{
    agent::{run::Id as RunId, thread::Id as ThreadId},
    observation::Observed,
    stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore},
    thread::{
        commit::operation::commit_payload_hash,
        read::{recovery::RunRecoverySource, transcript::*},
    },
};
use awaken_store_s3::{Error, SessionKey, StreamCursor};
use bytes::Bytes;
use std::{num::NonZeroUsize, sync::atomic::Ordering};
use support::*;

fn page(
    thread: &str,
    snapshot: Option<TranscriptSnapshotRef>,
    start: u64,
    limit: usize,
) -> TranscriptPageRequest {
    TranscriptPageRequest {
        thread_id: ThreadId(thread.into()),
        view: TranscriptView::RawCommitted,
        snapshot,
        start,
        limit: NonZeroUsize::new(limit).unwrap(),
    }
}
#[tokio::test]
async fn create_admit_replay_and_exact_identity() {
    let (_, _, _, session) = setup().await;
    session.create().await.unwrap();
    let request = dispatch("run", "session", "session");
    session.accept(request.clone()).await.unwrap();
    session.accept(request.clone()).await.unwrap();
    assert_eq!(session.activation("run").await.unwrap(), request);
    let mut changed = request;
    changed.traceparent = Some("changed".into());
    assert!(matches!(
        session.accept(changed).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(session.head().await.unwrap().commit_sequence, 0);
}
#[tokio::test]
async fn atomic_commit_reopens_and_exact_retry_keeps_time_and_receipt() {
    let (store, clock, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    let operation = operation("run", "session", 0, 0, "hello");
    let receipt = session.commit(&claim, operation.clone()).await.unwrap();
    clock.advance(50_000);
    let cold = engine(store, clock).session(session.key().clone());
    let duplicate = cold.commit(&claim, operation).await.unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.commit_sequence, receipt.commit_sequence);
    let snapshot = cold
        .recovery_snapshot(&ThreadId("session".into()), &RunId("run".into()))
        .await
        .unwrap();
    assert_eq!(snapshot.messages[0].text_content(), "hello");
    assert_eq!(snapshot.thread_version, 1);
    assert_eq!(
        snapshot
            .commit_timestamps
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![1_000]
    );
}
#[tokio::test]
async fn lost_success_and_unapplied_failure_are_reconciled_by_same_operation() {
    for fault in [1, 2] {
        let (store, _, _, session) = setup().await;
        let claim = activate(&session, "run", "session").await.run_claim();
        let operation = operation("run", "session", 0, 0, "once");
        store.fault.store(fault, Ordering::SeqCst);
        assert!(matches!(
            session.commit(&claim, operation.clone()).await,
            Err(Error::OutcomeUnknown(_))
        ));
        let retry = session.commit(&claim, operation).await.unwrap();
        assert_eq!(retry.duplicate, fault == 1);
        assert_eq!(session.head().await.unwrap().commit_sequence, 1);
    }
}
#[tokio::test]
async fn stale_writer_and_predecessor_physical_attempt_are_independently_fenced() {
    let (_, clock, _, session) = setup().await;
    let old = activate(&session, "run", "session").await.run_claim();
    session.begin_attempt(&old).await.unwrap();
    clock.advance(1_000);
    let new = session
        .claim("run", "worker-two", 1_000)
        .await
        .unwrap()
        .run_claim();
    assert!(matches!(
        session.begin_attempt(&new).await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        session
            .commit(&old, operation("run", "session", 0, 0, "stale"))
            .await,
        Err(Error::Fenced)
    ));
    assert!(matches!(session.renew(&old, 10).await, Err(Error::Fenced)));
    session.end_attempt(&old).await.unwrap();
    session.begin_attempt(&new).await.unwrap();
    session
        .commit(&new, operation("run", "session", 0, 0, "new"))
        .await
        .unwrap();
    assert!(matches!(
        session.end_attempt(&old).await,
        Err(Error::Fenced)
    ));
}
#[tokio::test]
async fn same_thread_cas_has_one_winner_and_parallel_threads_both_commit() {
    let (_, _, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    let (a, b) = tokio::join!(
        session.commit(&claim, operation("run", "session", 0, 0, "a")),
        session.commit(&claim, operation("run", "session", 0, 0, "b"))
    );
    assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1);
    let other = activate(&session, "other", "child").await.run_claim();
    let (a, b) = tokio::join!(
        session.commit(&claim, operation("run", "session", 1, 1, "next")),
        session.commit(&other, operation("other", "child", 0, 0, "child"))
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(session.head().await.unwrap().commit_sequence, 3);
}
#[tokio::test]
async fn bad_hash_and_stale_version_are_atomic_stutters() {
    let (_, _, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    let mut bad = operation("run", "session", 0, 0, "x");
    bad.payload_hash.0 = "incorrect".into();
    assert!(session.commit(&claim, bad).await.is_err());
    assert!(
        session
            .commit(&claim, operation("run", "session", 0, 1, "x"))
            .await
            .is_err()
    );
    assert_eq!(session.head().await.unwrap().commit_sequence, 0);
}
#[tokio::test]
async fn frozen_pages_nested_fork_and_source_append_are_isolated() {
    let (_, _, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    session
        .commit(&claim, operation("run", "session", 0, 0, "one"))
        .await
        .unwrap();
    let frozen = session
        .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    session
        .fork(&frozen, ThreadId("child".into()))
        .await
        .unwrap();
    let child = activate(&session, "child-run", "child").await.run_claim();
    session
        .commit(&child, operation("child-run", "child", 0, 0, "two"))
        .await
        .unwrap();
    let child_frozen = session
        .freeze(&ThreadId("child".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    session
        .fork(&child_frozen, ThreadId("grandchild".into()))
        .await
        .unwrap();
    session
        .commit(&claim, operation("run", "session", 1, 1, "parent-later"))
        .await
        .unwrap();
    let original = session
        .read_page(page("session", Some(frozen), 0, 10))
        .await
        .unwrap();
    assert_eq!(original.messages.len(), 1);
    let grand = session
        .read_page(page("grandchild", None, 0, 10))
        .await
        .unwrap();
    assert_eq!(
        grand
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>(),
        vec!["one", "two"]
    );
    let recovered = session
        .recovery_snapshot(&ThreadId("child".into()), &RunId("child-run".into()))
        .await
        .unwrap();
    assert_eq!(recovered.messages.len(), 2);
    assert_eq!(recovered.message_commit_cursors, vec![0, 2]);
}
#[tokio::test]
async fn scope_and_thread_references_never_escape_partition() {
    let (_, _, engine, session) = setup().await;
    for value in ["", "../bad", "a/b", "a\\b", "a%2fb", "a?b", "a#b", "a\0b"] {
        assert!(
            SessionKey::new(value, "session").is_err(),
            "scope {value:?}"
        );
        assert!(
            SessionKey::new("tenant", value).is_err(),
            "Session {value:?}"
        );
        assert!(
            serde_json::from_value::<SessionKey>(serde_json::json!({
                "scope": "tenant", "session": value,
            }))
            .is_err(),
            "persisted Session {value:?}"
        );
    }
    let claim = activate(&session, "run", "session").await.run_claim();
    session
        .commit(&claim, operation("run", "session", 0, 0, "private"))
        .await
        .unwrap();
    let frozen = session
        .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    let other = engine.session(SessionKey::new("other", "session").unwrap());
    other.create().await.unwrap();
    assert!(
        other
            .read_page(page("session", Some(frozen), 0, 10))
            .await
            .is_err()
    );
    let blob = session
        .streams()
        .upload_blob(Bytes::from_static(b"secret"), "text/plain")
        .await
        .unwrap();
    assert!(matches!(
        other.streams().read_blob(&blob).await,
        Err(Error::Invalid(_))
    ));
}
#[tokio::test]
async fn packed_frames_coalesce_and_later_pages_use_prefetch_cache() {
    let (store, clock, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    for n in 0..6 {
        session
            .commit(&claim, operation("run", "session", n, n, &format!("m{n}")))
            .await
            .unwrap();
    }
    assert_eq!(session.pack().await.unwrap(), 6);
    let cold = engine(store.clone(), clock).session(session.key().clone());
    let frozen = cold
        .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    store.ranges.lock().clear();
    let first = cold
        .read_page(page("session", Some(frozen.clone()), 0, 1))
        .await
        .unwrap();
    assert_eq!(first.messages[0].text_content(), "m0");
    for n in 1..6 {
        let next = cold
            .read_page(page("session", Some(frozen.clone()), n, 1))
            .await
            .unwrap();
        assert_eq!(next.messages[0].text_content(), format!("m{n}"));
    }
    let ranges = store.ranges.lock();
    assert_eq!(
        ranges
            .iter()
            .filter(|(path, _)| path.contains("/packs/"))
            .count(),
        1
    );
}
#[tokio::test]
async fn checkpoint_and_binary_stream_recover_then_commit_retires_checkpoint() {
    let (store, clock, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    let cursor = StreamCursor {
        run: "run".into(),
        step: 0,
        response: 0,
        assistant_step: None,
        attempt: 0,
        next_frame: 0,
    };
    let bytes = Bytes::from_static(&[0, 255, 12, 8, 0]);
    let after = session
        .streams()
        .append_stream(&claim, cursor.clone(), bytes.clone())
        .await
        .unwrap();
    assert_eq!(
        session
            .streams()
            .append_stream(&claim, cursor.clone(), bytes.clone())
            .await
            .unwrap(),
        after
    );
    assert!(
        session
            .streams()
            .append_stream(&claim, cursor.clone(), Bytes::from_static(b"different"))
            .await
            .is_err()
    );
    let checkpoint = StreamCheckpoint {
        run_id: "run".into(),
        thread_id: "session".into(),
        model: "m".into(),
        partial_text: "partial".into(),
        partial_tools: vec![],
        retry_count: 2,
    };
    session
        .streams()
        .put_checkpoint(&claim, checkpoint.clone())
        .await
        .unwrap();
    let cold = engine(store, clock).session(session.key().clone());
    assert_eq!(
        cold.streams().checkpoint("run").await,
        awaken_agent_contract::observation::Observed::Present(checkpoint)
    );
    assert_eq!(
        cold.streams().read_stream(&cursor, 10).await.unwrap()[0].bytes,
        bytes
    );
    cold.commit(&claim, operation("run", "session", 0, 0, "complete"))
        .await
        .unwrap();
    assert!(matches!(
        cold.streams().checkpoint("run").await,
        awaken_agent_contract::observation::Observed::Absent
    ));
    assert!(
        cold.streams()
            .append_stream(&claim, after, Bytes::new())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn checkpoint_read_error_is_unknown_and_old_claim_cannot_delete_new_checkpoint() {
    let (store, clock, _, session) = setup().await;
    let old = activate(&session, "run", "session").await.run_claim();
    let bound = session.streams().checkpoints(old.clone());
    store.fault.store(3, Ordering::SeqCst);
    assert!(matches!(bound.get("run").await, Observed::Unknown(_)));
    store.fault.store(0, Ordering::SeqCst);
    clock.advance(1_000);
    let new = session
        .claim("run", "two", 1_000)
        .await
        .unwrap()
        .run_claim();
    session
        .streams()
        .put_checkpoint(
            &new,
            StreamCheckpoint {
                run_id: "run".into(),
                thread_id: "session".into(),
                model: "m".into(),
                partial_text: "new".into(),
                partial_tools: vec![],
                retry_count: 0,
            },
        )
        .await
        .unwrap();
    assert!(bound.delete("run").await.is_err());
    let awaken_agent_contract::observation::Observed::Present(checkpoint) =
        session.streams().checkpoint("run").await
    else {
        panic!("replacement checkpoint must be present");
    };
    assert_eq!(checkpoint.partial_text, "new");
}
#[tokio::test]
async fn garbage_collection_retains_fork_roots_and_rejects_old_epoch_upload() {
    let (_, _, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    session
        .commit(&claim, operation("run", "session", 0, 0, "kept"))
        .await
        .unwrap();
    let frozen = session
        .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    session
        .fork(&frozen, ThreadId("child".into()))
        .await
        .unwrap();
    let orphan = session
        .streams()
        .upload_blob(Bytes::from_static(b"orphan"), "text/plain")
        .await
        .unwrap();
    session.pack().await.unwrap();
    let report = session.collect_garbage().await.unwrap();
    assert!(report.deleted > 0);
    assert!(session.streams().pin_blob("old", orphan).await.is_err());
    assert_eq!(
        session
            .read_page(page("child", None, 0, 10))
            .await
            .unwrap()
            .messages[0]
            .text_content(),
        "kept"
    );
    session
        .commit(&claim, operation("run", "session", 1, 1, "after-gc"))
        .await
        .unwrap();
}
#[tokio::test]
async fn subscription_replays_and_repairs_lost_cross_instance_wake() {
    let (store, clock, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    let mut subscription = session.subscribe(None).await.unwrap();
    let other = engine(store, clock).session(session.key().clone());
    other
        .commit(&claim, operation("run", "session", 0, 0, "one"))
        .await
        .unwrap();
    let records = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session.next_commits(&mut subscription, 1),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(subscription.cursor().after, 1);
    let mut resumed = session
        .subscribe(Some(subscription.cursor()))
        .await
        .unwrap();
    other
        .commit(&claim, operation("run", "session", 1, 1, "two"))
        .await
        .unwrap();
    assert_eq!(
        session.next_commits(&mut resumed, 10).await.unwrap()[0]
            .receipt
            .commit_sequence,
        2
    );
}
#[tokio::test]
async fn terminal_run_cannot_append_and_receipt_replay_survives_reclaim() {
    let (_, clock, _, session) = setup().await;
    let old = activate(&session, "run", "session").await.run_claim();
    let mut op = operation("run", "session", 0, 0, "done");
    op.commit.run = awaken_agent_contract::thread::commit::staged::RunDisposition::ended(
        RunId("run".into()),
        awaken_agent_contract::agent::run::EndCause::Indeterminate,
    );
    op.payload_hash = commit_payload_hash(&op.commit).unwrap();
    session.commit(&old, op.clone()).await.unwrap();
    clock.advance(1_000);
    let new = session.claim("run", "two", 1000).await.unwrap().run_claim();
    assert!(session.commit(&new, op).await.unwrap().duplicate);
    assert!(
        session
            .commit(&new, operation("run", "session", 1, 1, "bad"))
            .await
            .is_err()
    );
    session.settle(&new).await.unwrap();
    assert!(session.claim("run", "three", 1000).await.is_err());
}

#[tokio::test]
async fn rollback_and_compaction_keep_raw_history_and_frozen_views() {
    let (_, _, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    session
        .commit(&claim, operation("run", "session", 0, 0, "one"))
        .await
        .unwrap();
    let first = session
        .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    session
        .commit(&claim, operation("run", "session", 1, 1, "two"))
        .await
        .unwrap();
    assert!(
        session
            .rollback(&ThreadId("session".into()), &first)
            .await
            .is_err()
    );
    session.relinquish(&claim).await.unwrap();
    session
        .rollback(&ThreadId("session".into()), &first)
        .await
        .unwrap();
    let mut request = page("session", None, 0, 10);
    request.view = TranscriptView::ContextEffective;
    assert_eq!(
        session
            .read_page(request.clone())
            .await
            .unwrap()
            .messages
            .len(),
        1
    );
    let summary = awaken_agent_contract::agent::message::Message::text(
        awaken_agent_contract::agent::message::Id("summary".into()),
        awaken_agent_contract::agent::message::Role::System,
        "summary",
    );
    let compact = session
        .compact(&ThreadId("session".into()), vec![summary])
        .await
        .unwrap();
    let new = session
        .claim("run", "next", 1000)
        .await
        .unwrap()
        .run_claim();
    session
        .commit(&new, operation("run", "session", 2, 2, "three"))
        .await
        .unwrap();
    assert_eq!(
        session
            .read_page(page("session", None, 0, 10))
            .await
            .unwrap()
            .messages
            .len(),
        3
    );
    let effective = session.read_page(request).await.unwrap();
    assert_eq!(
        effective
            .messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>(),
        vec!["summary", "three"]
    );
    let mut old = page("session", Some(compact), 0, 10);
    old.view = TranscriptView::ContextEffective;
    assert_eq!(session.read_page(old).await.unwrap().messages.len(), 1);
}
#[tokio::test]
async fn attachments_publish_with_receipt_and_survive_collection() {
    let (store, clock, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    let blob = session
        .streams()
        .upload_blob(Bytes::from_static(b"long tool result"), "text/plain")
        .await
        .unwrap();
    let op = operation("run", "session", 0, 0, "archived result");
    let receipt = session
        .commit_with_blobs(&claim, op.clone(), std::slice::from_ref(&blob))
        .await
        .unwrap();
    session.collect_garbage().await.unwrap();
    let cold = engine(store, clock).session(session.key().clone());
    assert_eq!(
        cold.streams().read_blob(&blob).await.unwrap(),
        Bytes::from_static(b"long tool result")
    );
    assert!(
        cold.commit_with_blobs(&claim, op.clone(), std::slice::from_ref(&blob))
            .await
            .unwrap()
            .duplicate
    );
    let other = cold
        .streams()
        .upload_blob(Bytes::from_static(b"other"), "text/plain")
        .await
        .unwrap();
    assert!(cold.commit_with_blobs(&claim, op, &[other]).await.is_err());
    assert_eq!(
        cold.head().await.unwrap().commit_sequence,
        receipt.commit_sequence
    );
}
#[tokio::test]
async fn cross_session_copy_has_independent_roots() {
    let (_, _, engine, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    session
        .commit(&claim, operation("run", "session", 0, 0, "copied"))
        .await
        .unwrap();
    let snapshot = session
        .freeze(&ThreadId("session".into()), TranscriptView::RawCommitted)
        .await
        .unwrap();
    let other = engine.session(SessionKey::new("tenant", "destination").unwrap());
    other.create().await.unwrap();
    other
        .copy_from(&session, &snapshot, ThreadId("copied".into()))
        .await
        .unwrap();
    session.release_snapshot(&snapshot).await.unwrap();
    session.pack().await.unwrap();
    session.collect_garbage().await.unwrap();
    assert_eq!(
        other
            .read_page(page("copied", None, 0, 10))
            .await
            .unwrap()
            .messages[0]
            .text_content(),
        "copied"
    );
}
#[tokio::test]
async fn bound_runtime_commit_and_checkpoint_ports_use_the_same_authority() {
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    let (_, _, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    let bound = session.bind(claim).await.unwrap();
    bound
        .commit_operation(operation("run", "session", 0, 0, "through runtime port"))
        .await
        .unwrap();
    assert_eq!(
        bound.committed_messages(&ThreadId("session".into()))[0].text_content(),
        "through runtime port"
    );
}

#[tokio::test]
async fn garbage_collection_between_upload_and_publication_forces_restage() {
    let (store, clock, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    store.hold_next_head.store(true, Ordering::SeqCst);
    let writer = session.clone();
    let operation = operation("run", "session", 0, 0, "must restage");
    let pending = tokio::spawn(async move { writer.commit(&claim, operation).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.head_entered.notified(),
    )
    .await
    .unwrap();
    let maintenance = engine(store.clone(), clock.clone()).session(session.key().clone());
    assert!(maintenance.collect_garbage().await.unwrap().deleted > 0);
    store.head_resume.notify_one();
    pending.await.unwrap().unwrap();
    let cold = engine(store, clock).session(session.key().clone());
    assert_eq!(
        cold.read_page(page("session", None, 0, 10))
            .await
            .unwrap()
            .messages[0]
            .text_content(),
        "must restage"
    );
}
#[tokio::test]
async fn takeover_by_another_run_fences_the_expired_thread_writer() {
    let (_, clock, _, session) = setup().await;
    let old = activate(&session, "old", "session").await.run_claim();
    session
        .accept(dispatch("new", "session", "session"))
        .await
        .unwrap();
    clock.advance(1_000);
    let new = session
        .claim("new", "new-worker", 1_000)
        .await
        .unwrap()
        .run_claim();
    assert!(matches!(
        session.begin_attempt(&old).await,
        Err(Error::Fenced)
    ));
    assert!(matches!(
        session
            .commit(&old, operation("old", "session", 0, 0, "stale"))
            .await,
        Err(Error::Fenced)
    ));
    session.begin_attempt(&new).await.unwrap();
}
#[tokio::test]
async fn immutable_payload_corruption_fails_closed_after_cold_reopen() {
    use object_store::{ObjectStoreExt, path::Path};
    let (store, clock, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    session
        .commit(&claim, operation("run", "session", 0, 0, "original"))
        .await
        .unwrap();
    let head: serde_json::Value = serde_json::from_slice(
        &store
            .inner
            .get(&Path::from(
                "test/v1/scopes/tenant/sessions/session/head.json",
            ))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    let key = head["recent"][0]["body"]["key"].as_str().unwrap();
    store
        .inner
        .put(
            &Path::from(format!("test/{key}")),
            Bytes::from_static(b"tampered").into(),
        )
        .await
        .unwrap();
    let cold = engine(store, clock).session(session.key().clone());
    assert!(
        cold.recovery_snapshot(&ThreadId("session".into()), &RunId("run".into()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn renewal_keeps_the_existing_deadline_and_cancellation_is_absorbing() {
    use awaken_agent_contract::{agent::run::EndCause, thread::commit::staged::RunDisposition};
    let (_, clock, _, session) = setup().await;
    let claim = activate(&session, "run", "session").await.run_claim();
    clock.advance(100);
    session.renew(&claim, 10).await.unwrap();
    clock.advance(20);
    assert!(session.claim("run", "other", 1000).await.is_err());
    session.cancel("run").await.unwrap();
    assert!(
        session
            .commit(&claim, operation("run", "session", 0, 0, "running"))
            .await
            .is_err()
    );
    let mut terminal = operation("run", "session", 0, 0, "ended");
    terminal.commit.run = RunDisposition::ended(RunId("run".into()), EndCause::Indeterminate);
    terminal.payload_hash = commit_payload_hash(&terminal.commit).unwrap();
    assert!(session.commit(&claim, terminal.clone()).await.is_err());
    terminal.commit.run = RunDisposition::ended(RunId("run".into()), EndCause::Cancelled);
    terminal.payload_hash = commit_payload_hash(&terminal.commit).unwrap();
    session.commit(&claim, terminal).await.unwrap();
}

#[tokio::test]
async fn stream_index_folds_without_growing_the_session_head_and_pages_after_reopen() {
    use object_store::{ObjectStoreExt, path::Path};
    let (store, clock, _, session) = setup().await;
    let claim = activate(&session, "stream-index", "session")
        .await
        .run_claim();
    let initial = awaken_store_s3::StreamCursor {
        run: "stream-index".into(),
        step: 0,
        response: 0,
        assistant_step: None,
        attempt: 0,
        next_frame: 0,
    };
    let mut next = initial.clone();
    for n in 0..4_100_u64 {
        next = session
            .streams()
            .append_stream(&claim, next, Bytes::from(n.to_be_bytes().to_vec()))
            .await
            .unwrap();
    }
    let head = store
        .inner
        .get(&Path::from(
            "test/v1/scopes/tenant/sessions/session/head.json",
        ))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(
        head.len() < 40_000,
        "stream watermark/frontier must stay bounded: {}",
        head.len()
    );
    let cold = engine(store, clock).session(session.key().clone());
    let mut start = initial;
    start.next_frame = 4_091;
    let page = cold.streams().read_stream(&start, 20).await.unwrap();
    assert_eq!(page.len(), 9);
    for (offset, batch) in page.iter().enumerate() {
        assert_eq!(batch.bytes.as_ref(), &(4_091 + offset as u64).to_be_bytes());
    }
    cold.collect_garbage().await.unwrap();
    assert_eq!(
        cold.streams().read_stream(&start, 20).await.unwrap().len(),
        9
    );
}

#[tokio::test]
async fn page_pins_coalesce_expire_and_keep_fork_and_explicit_freeze_roots() {
    use awaken_agent_contract::thread::read::transcript::{TranscriptPageRequest, TranscriptView};
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    let store = Arc::new(support::FaultStore::default());
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1000));
    let engine = awaken_store_s3::S3Storage::new(
        store,
        "page-pins",
        clock.clone(),
        awaken_store_s3::Limits {
            page_snapshot_ttl_ms: 10,
            ..Default::default()
        },
    )
    .unwrap();
    let session = engine.session(awaken_store_s3::SessionKey::new("tenant", "session").unwrap());
    session.create().await.unwrap();
    let thread = awaken_agent_contract::ThreadId("session".into());
    let request = TranscriptPageRequest {
        thread_id: thread.clone(),
        view: TranscriptView::RawCommitted,
        snapshot: None,
        start: 0,
        limit: NonZeroUsize::new(10).unwrap(),
    };
    let empty = session.read_page(request.clone()).await.unwrap();
    assert!(
        empty.messages.is_empty(),
        "a new Session has an empty root transcript"
    );
    let explicit = session
        .freeze(&thread, TranscriptView::RawCommitted)
        .await
        .unwrap();
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    session
        .commit(
            &claim,
            support::operation("run", "session", 0, 0, "retained"),
        )
        .await
        .unwrap();
    let page = session.read_page(request.clone()).await.unwrap();
    for _ in 0..32 {
        assert_eq!(
            session.read_page(request.clone()).await.unwrap().snapshot,
            page.snapshot,
            "repeated initial history reads reuse a stable pin"
        );
    }
    session
        .fork(
            &page.snapshot,
            awaken_agent_contract::ThreadId("child".into()),
        )
        .await
        .unwrap();
    clock.advance(10);
    assert!(
        session
            .read_page(TranscriptPageRequest {
                snapshot: Some(page.snapshot),
                ..request.clone()
            })
            .await
            .is_err()
    );
    session.collect_garbage().await.unwrap();
    assert!(
        session
            .read_page(TranscriptPageRequest {
                snapshot: Some(explicit),
                ..request.clone()
            })
            .await
            .unwrap()
            .messages
            .is_empty(),
        "explicit freezes remain caller-owned"
    );
    let inherited = session
        .read_page(TranscriptPageRequest {
            thread_id: awaken_agent_contract::ThreadId("child".into()),
            ..request
        })
        .await
        .unwrap();
    assert_eq!(
        inherited.messages.len(),
        1,
        "fork roots survive expiry and collection of the originating page pin"
    );
}

#[tokio::test]
async fn collection_fences_old_blob_pins_while_the_bytes_still_exist() {
    use std::time::Duration;
    use tokio::time::timeout;

    let (objects, clock, _, session) = setup().await;
    let cold = || {
        engine(objects.clone(), clock.clone())
            .session(session.key().clone())
            .streams()
    };
    let streams = session.streams();
    let old = streams
        .upload_blob(Bytes::from_static(b"old"), "text/plain")
        .await
        .unwrap();

    objects.hold_next_delete.store(true, Ordering::SeqCst);
    let collector = session.clone();
    let collection = tokio::spawn(async move { collector.collect_garbage().await });
    timeout(Duration::from_secs(10), objects.delete_entered.notified())
        .await
        .expect("collector reached deletion after sealing and marking");

    let old_read_before = cold().read_blob(&old).await.unwrap();
    let sealed_pin_rejected = matches!(
        streams.pin_blob("old", old.clone()).await,
        Err(Error::Conflict(message)) if message == "upload epoch sealed; upload again"
    );
    let fresh = streams
        .upload_blob(Bytes::from_static(b"fresh"), "text/plain")
        .await
        .unwrap();
    let fresh_pin = streams.pin_blob("fresh", fresh.clone()).await;

    objects.delete_resume.notify_one();
    let report = timeout(Duration::from_secs(10), collection)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let old_read_after = cold().read_blob(&old).await;
    session.collect_garbage().await.unwrap();
    let fresh_read_after = cold().read_blob(&fresh).await.unwrap();

    awaken_reliability_testkit::write_observation_trace(
        "s3-sealed-collection",
        "sealed-collection",
        serde_json::json!([{
            "rule": "sealed-collection",
            "trace": {
                "old_generation": old.object.generation,
                "fresh_generation": fresh.object.generation,
                "cutoff": report.sealed_generation,
                "deleted": report.deleted,
                "old_read_before": old_read_before == Bytes::from_static(b"old"),
                "sealed_pin_rejected": sealed_pin_rejected,
                "fresh_pin_admitted": fresh_pin.is_ok(),
                "old_read_after_failed": old_read_after.is_err(),
                "fresh_read_after": fresh_read_after == Bytes::from_static(b"fresh"),
            }
        }]),
    );
    assert_eq!(old_read_before, Bytes::from_static(b"old"));
    assert!(sealed_pin_rejected);
    fresh_pin.unwrap();
    assert!(report.deleted > 0);
    assert!(old_read_after.is_err());
    assert_eq!(fresh_read_after, Bytes::from_static(b"fresh"));
}
