//! Shared laws for the production Worker commit port, independent of storage media.
//! Backends supply a real accepted Dispatch claim; these tests never substitute a writer.
use awaken_agent_contract::{
    agent::message::{Id as MessageId, Message, Role},
    thread::{
        commit::operation::commit_payload_hash,
        read::recovery::{RunRecoverySnapshot, RunRecoverySource},
    },
};
use awaken_dispatch_contract::{
    COMMIT_CONFLICT, COMMIT_FENCED, COMMIT_IDENTITY_MISMATCH, ClaimedCommitCommand,
    ClaimedRunCommit,
};
use awaken_runtime_contract::runs::{RunError, RunErrorKind};
use awaken_worker_contract::RunClaim;

/// Run over a fresh admitted claim. Rejections must preserve the full committed snapshot.
pub async fn exact_replay_and_rejections(
    writer: &dyn ClaimedRunCommit<RunError>,
    recovery: &dyn RunRecoverySource,
    command: ClaimedCommitCommand,
) {
    // C1 valid fresh request; C2 exact retry; C3 a canonical different payload at
    // the same identity; C4 stale Thread version; C5 gapped ordinal; C6 forged
    // body under the old hash. R1 C1 applies once, R2 C2 returns the same receipt,
    // R3 C3/C6 reports identity mismatch, R4 C4/C5 reports retryable conflict.
    // Every rejection is a stutter over messages, state, events, times and receipts.
    let thread = &command.operation.commit.thread_id;
    let run = &command.claim.run_id;
    let first = writer.commit_operation(command.clone()).await.unwrap();
    assert!(!first.duplicate);
    let before = recovery.recovery_snapshot(thread, run).await.unwrap();
    let mut retry = command.clone();
    retry.operation.expected_thread_version = u64::MAX;
    let replay = writer.commit_operation(retry).await.unwrap();
    let mut expected = first.clone();
    expected.duplicate = true;
    assert_eq!(replay, expected, "exact replay precedes current CAS");

    let mut different = command.clone();
    different.operation.commit.messages.push(Message::text(
        MessageId("different".into()),
        Role::Assistant,
        "different payload",
    ));
    different.operation.payload_hash = commit_payload_hash(&different.operation.commit).unwrap();
    assert_error(
        writer,
        different,
        RunErrorKind::BadRequest,
        COMMIT_IDENTITY_MISMATCH,
    )
    .await;

    let mut stale = command.clone();
    stale.operation.operation_id.ordinal += 1;
    assert_error(writer, stale, RunErrorKind::Conflict, COMMIT_CONFLICT).await;

    let mut gap = command.clone();
    gap.operation.operation_id.ordinal += 2;
    gap.operation.expected_thread_version = first.thread_version;
    assert_error(writer, gap, RunErrorKind::Conflict, COMMIT_CONFLICT).await;

    let mut forged = command.clone();
    forged.operation.commit.messages.push(Message::text(
        MessageId("forged".into()),
        Role::Assistant,
        "body does not match hash",
    ));
    assert_error(
        writer,
        forged,
        RunErrorKind::BadRequest,
        COMMIT_IDENTITY_MISMATCH,
    )
    .await;
    let after = recovery.recovery_snapshot(thread, run).await.unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap(),
        "all rejections preserve exact durable prefix"
    );
}
/// Two independently polled submissions compete for one exact operation identity.
pub async fn concurrent_operation_has_one_winner(
    writer: &dyn ClaimedRunCommit<RunError>,
    recovery: &dyn RunRecoverySource,
    command: ClaimedCommitCommand,
) {
    // C1 same claim/id/version; C2 different valid canonical bodies. R1 exactly
    // one may publish; R2 the other observes the winner's retained identity.
    let mut other = command.clone();
    other.operation.commit.messages.push(Message::text(
        MessageId("racing".into()),
        Role::Assistant,
        "racing payload",
    ));
    other.operation.payload_hash = commit_payload_hash(&other.operation.commit).unwrap();
    let (left, right) = tokio::join!(
        writer.commit_operation(command.clone()),
        writer.commit_operation(other)
    );
    assert_eq!(left.is_ok() as u8 + right.is_ok() as u8, 1);
    let loser = if let Err(error) = left {
        error
    } else {
        right.unwrap_err()
    };
    assert_eq!(loser.kind, RunErrorKind::BadRequest);
    assert_eq!(loser.code, COMMIT_IDENTITY_MISMATCH);
    let snapshot = recovery
        .recovery_snapshot(&command.operation.commit.thread_id, &command.claim.run_id)
        .await
        .unwrap();
    assert_eq!(
        snapshot.thread_version,
        command.operation.expected_thread_version + 1
    );
    assert_eq!(
        snapshot.next_commit_ordinal,
        command.operation.operation_id.ordinal + 1
    );
}
/// A retired claimant cannot append or borrow a receipt after a real takeover.
pub async fn stale_claim_cannot_publish(
    writer: &dyn ClaimedRunCommit<RunError>,
    recovery: &dyn RunRecoverySource,
    stale: ClaimedCommitCommand,
    current: ClaimedCommitCommand,
) {
    let before = recovery
        .recovery_snapshot(&stale.operation.commit.thread_id, &stale.claim.run_id)
        .await
        .unwrap();
    assert_error(
        writer,
        stale.clone(),
        RunErrorKind::BadRequest,
        COMMIT_FENCED,
    )
    .await;
    assert_eq!(
        serde_json::to_value(
            recovery
                .recovery_snapshot(&stale.operation.commit.thread_id, &stale.claim.run_id)
                .await
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(before).unwrap()
    );
    writer.commit_operation(current).await.unwrap();
    assert_error(writer, stale, RunErrorKind::BadRequest, COMMIT_FENCED).await;
}
async fn assert_error(
    writer: &dyn ClaimedRunCommit<RunError>,
    command: ClaimedCommitCommand,
    kind: RunErrorKind,
    code: &str,
) {
    let error = writer.commit_operation(command).await.unwrap_err();
    assert_eq!(error.kind, kind, "{}", error.message);
    assert_eq!(error.code, code, "{}", error.message);
}

/// Observe a committed operation whose acknowledgement the caller did not receive.
/// The backend supplies the failure boundary and the real Dispatch takeover.
pub async fn lost_acknowledgement_and_takeover(
    writer: &dyn ClaimedRunCommit<RunError>,
    recovery: &dyn RunRecoverySource,
    mut command: ClaimedCommitCommand,
    discard_reply: impl AsyncFnOnce(ClaimedCommitCommand),
    take_over: impl AsyncFnOnce(&RunClaim) -> RunClaim,
) -> serde_json::Value {
    use serde_json::json;

    fn point(
        kind: &str,
        claim: &RunClaim,
        snapshot: &RunRecoverySnapshot,
        receipts: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "kind": kind,
            "epoch": claim.epoch,
            "state": {
                "version": snapshot.thread_version,
                "ordinal": snapshot.next_commit_ordinal,
                "cursor": snapshot.store_cursor,
                "messages": snapshot.messages.len(),
            },
            "receipts": receipts,
        })
    }

    let thread = command.operation.commit.thread_id.clone();
    let run = command.claim.run_id.clone();
    let recover = || recovery.recovery_snapshot(&thread, &run);
    let initial = recover().await.unwrap();
    let expected_version = command.operation.expected_thread_version;

    discard_reply(command.clone()).await;
    let committed = recover().await.unwrap();
    assert_eq!(committed.thread_version, initial.thread_version + 1);
    assert_eq!(
        committed.next_commit_ordinal,
        initial.next_commit_ordinal + 1
    );
    assert!(committed.store_cursor > initial.store_cursor);
    assert_eq!(
        &committed.messages[initial.messages.len()..],
        command.operation.commit.messages.as_slice()
    );
    let expected = serde_json::to_value(&committed).unwrap();
    let old_claim = command.claim.clone();

    command.operation.expected_thread_version = u64::MAX;
    let replay = writer.commit_operation(command.clone()).await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.operation_id, command.operation.operation_id);
    assert_eq!(replay.payload_hash, command.operation.payload_hash);
    assert_eq!(replay.thread_version, committed.thread_version);
    assert_eq!(replay.commit_sequence, committed.store_cursor);
    let replayed = recover().await.unwrap();
    assert_eq!(serde_json::to_value(&replayed).unwrap(), expected);

    let successor = take_over(&command.claim).await;
    assert_eq!(successor.run_id, command.claim.run_id);
    assert!(successor.epoch > old_claim.epoch);
    let taken_over = recover().await.unwrap();
    assert_eq!(serde_json::to_value(&taken_over).unwrap(), expected);
    assert_error(
        writer,
        command.clone(),
        RunErrorKind::BadRequest,
        COMMIT_FENCED,
    )
    .await;
    let fenced = recover().await.unwrap();
    assert_eq!(serde_json::to_value(&fenced).unwrap(), expected);

    command.claim = successor;
    let mut different = command.clone();
    different.operation.expected_thread_version = committed.thread_version;
    different.operation.commit.messages.push(Message::text(
        MessageId("takeover-conflict".into()),
        Role::Assistant,
        "different payload after takeover",
    ));
    let conflict_hash = commit_payload_hash(&different.operation.commit).unwrap();
    different.operation.payload_hash = conflict_hash.clone();
    assert_error(
        writer,
        different,
        RunErrorKind::BadRequest,
        COMMIT_IDENTITY_MISMATCH,
    )
    .await;
    let conflict = recover().await.unwrap();
    assert_eq!(serde_json::to_value(&conflict).unwrap(), expected);

    let successor_replay = writer.commit_operation(command.clone()).await.unwrap();
    assert_eq!(successor_replay, replay);
    let final_state = recover().await.unwrap();
    assert_eq!(serde_json::to_value(&final_state).unwrap(), expected);

    json!({
        "input": {
            "operation": command.operation.operation_id,
            "hash": command.operation.payload_hash,
            "conflict_hash": conflict_hash,
            "version": expected_version,
            "messages": command.operation.commit.messages.len(),
        },
        "observations": [
            point("initial", &old_claim, &initial, json!([])),
            point("unknown", &old_claim, &committed, json!([])),
            point("replayed", &old_claim, &replayed, json!([replay])),
            point("takeover", &command.claim, &taken_over, json!([])),
            point("fenced", &old_claim, &fenced, json!([])),
            point("identity-conflict", &command.claim, &conflict, json!([])),
            point("replayed", &command.claim, &final_state, json!([successor_replay])),
        ]
    })
}

/// A scheduling/control commit cannot consume an interrupted model response.
pub async fn control_commit_preserves_interrupted_stream(
    writer: &dyn ClaimedRunCommit<RunError>,
    checkpoints: &dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore,
    mut command: ClaimedCommitCommand,
) {
    use awaken_agent_contract::{
        observation::Observed,
        stream::checkpoint::StreamCheckpoint,
        thread::commit::staged::{RunDisposition, ThreadCommit},
    };
    let mut checkpoint = StreamCheckpoint {
        run_id: command.claim.run_id.0.clone(),
        thread_id: command.operation.commit.thread_id.0.clone(),
        model: "model".into(),
        partial_text: "confirmed prefix ".into(),
        partial_tools: vec![],
        retry_count: 1,
    };
    checkpoints.put(checkpoint.clone()).await.unwrap();
    command.operation.commit = ThreadCommit::rescheduled(
        command.operation.commit.thread_id.clone(),
        RunDisposition::running(command.claim.run_id.clone()),
        command.claim.epoch,
    );
    command.operation.payload_hash = commit_payload_hash(&command.operation.commit).unwrap();
    let first = writer.commit_operation(command.clone()).await.unwrap();
    assert!(!first.duplicate);
    let Observed::Present(actual) = checkpoints.get(&checkpoint.run_id).await else {
        panic!("a control receipt discarded an interrupted stream");
    };
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(&checkpoint).unwrap()
    );
    checkpoint.partial_text.push_str("newer");
    checkpoint.retry_count += 1;
    checkpoints.put(checkpoint.clone()).await.unwrap();
    let replay = writer.commit_operation(command).await.unwrap();
    let mut expected = first;
    expected.duplicate = true;
    assert_eq!(replay, expected);
    let Observed::Present(actual) = checkpoints.get(&checkpoint.run_id).await else {
        panic!("an exact control replay discarded a newer checkpoint");
    };
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(checkpoint).unwrap()
    );
}
