//! The original storage-fault decorator retains the actual operation authority.

use super::*;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_dispatch::{ClaimedCommitCommand, ClaimedRunCommit, GuardedRunCommit, RunClaim};

#[tokio::test]
async fn claimed_operation_lost_reply_replays_the_original_receipt_once() {
    // Causes: C1 the real store rejects before append or persists then loses
    // response; C2 caller retries the identical operation; C3 the exact claim
    // remains current. Rules: L1=reject=>zero append; L2=lost response=>one
    // durable append but error; L3=L2+C2+C3=>original receipt, duplicate=true,
    // no second append. The shared original fault decorator applies to both
    // ordinary and versioned paths; no fixture fabricates a receipt.
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let dispatch = Arc::new(MemoryDispatchStore::new(clock.clock()));
    dispatch
        .enqueue(RunDispatch::new(activation("run-1")))
        .await
        .unwrap();
    let claimed = dispatch
        .claim("worker", LEASE, &Default::default())
        .await
        .unwrap()
        .unwrap();
    let committed = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let fault = Arc::new(FailingCommit::new(committed.clone(), true));
    let service = GuardedRunCommit::new(fault.clone(), dispatch);
    let commit = ThreadCommit::assemble(
        ThreadId(harness::THREAD.into()),
        RunDisposition::running(claimed.request.run_id().clone()),
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let request = ClaimedCommitCommand {
        claim: RunClaim::from(&claimed.lease),
        operation: CommitOperation {
            operation_id: CommitOperationId::new(claimed.request.run_id().clone(), 0),
            expected_thread_version: 0,
            payload_hash: commit_payload_hash(&commit).unwrap(),
            commit,
        },
    };
    let rejected = service.commit_operation(request.clone()).await.unwrap_err();
    assert!(
        rejected.to_string().contains("injected commit failure"),
        "L1"
    );
    assert_eq!(committed.commit_count(), 0, "L1");
    fault.set_failing(false);
    fault.lose_next_response();
    let lost = service.commit_operation(request.clone()).await.unwrap_err();
    assert!(
        lost.to_string().contains("injected lost commit response"),
        "L2"
    );
    assert_eq!(committed.commit_count(), 1, "L2");
    let receipt = service.commit_operation(request.clone()).await.unwrap();
    assert!(receipt.duplicate, "L3");
    assert_eq!(receipt.commit_sequence, 1, "L3 original persisted sequence");
    assert_eq!(receipt.thread_version, 1, "L3 original persisted version");
    assert_eq!(
        receipt.operation_id, request.operation.operation_id,
        "L3 exact identity"
    );
    assert_eq!(
        receipt.payload_hash, request.operation.payload_hash,
        "L3 exact hash"
    );
    assert_eq!(committed.commit_count(), 1, "L3 never append again");
}

use awaken_dispatch_contract::{DispatchClaims as _, DispatchEnqueue as _};
