//! Both combined constructors preserve the real operation owner before erasure.

use super::*;
use crate::worker::WorkerCommit;
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, commit_payload_hash,
};
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch_contract::ClaimedCommitCommand;

async fn prove_original_operation<S: awaken_dispatch_erasure::Dispatch + 'static>(
    worker: DispatchWorker<S>,
    committed: &MemoryCommitCoordinator,
    claim: RunClaim,
) {
    let snapshot = committed
        .recovery_snapshot(&ThreadId("thread".into()), &claim.run_id)
        .await
        .unwrap();
    worker
        .recovery_projection
        .as_ref()
        .unwrap()
        .install(&claim.run_id, &ThreadId("thread".into()), snapshot)
        .unwrap();
    let context = worker
        .execution_context_with(&claim, None, &None, &[], &[], None)
        .unwrap();
    let commit = ThreadCommit::assemble(
        ThreadId("thread".into()),
        RunDisposition::running(claim.run_id.clone()),
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    context
        .commit
        .as_ref()
        .unwrap()
        .commit(commit.clone())
        .await
        .unwrap();
    let WorkerCommit::Versioned(writer) = &worker.claimed_commit else {
        panic!("combined Worker must use a versioned writer");
    };
    let receipt = writer
        .commit_operation(ClaimedCommitCommand {
            claim: claim.clone(),
            operation: CommitOperation {
                operation_id: CommitOperationId::new(claim.run_id, 0),
                expected_thread_version: 0,
                payload_hash: commit_payload_hash(&commit).unwrap(),
                commit,
            },
        })
        .await
        .unwrap();
    assert!(
        receipt.duplicate,
        "combined constructor retained the real receipt owner"
    );
    assert_eq!(receipt.commit_sequence, 1);
    assert_eq!(receipt.thread_version, 1);
    assert_eq!(
        committed.commit_count(),
        1,
        "exact replay has no second append"
    );
}

#[tokio::test]
async fn combined_worker_constructors_keep_original_operation_authority_with_projection() {
    // Causes: C1 new/new_bound selects the combined local authority; C2 one
    // commit crosses its installed projection; C3 the exact operation is retried.
    // Effect E1 both constructors return the original duplicate receipt and
    // append once. Rules W1=new+C2+C3=>E1; W2=new_bound+C2+C3=>E1.
    // A plain fallback would append twice and cannot set duplicate=true.
    for bound in [false, true] {
        let dispatch = Arc::new(MemoryDispatchStore::new(Arc::new(
            awaken_lease_testkit::ManualClock::new(0),
        )));
        let committed = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        dispatch
            .enqueue(crate::RunDispatch::new(RunActivation::new(
                RunId("run".into()),
                ThreadId("thread".into()),
                ExecutableAgentSnapshot::builder("snapshot")
                    .model(
                        awaken_runtime_contract::resolved::ModelBinding::parse(
                            "test", "test", "native",
                        )
                        .expect("complete fixture model binding"),
                    )
                    .build(),
                Vec::new(),
            )))
            .await
            .unwrap();
        let claimed = dispatch
            .claim("worker", 1_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        let claim = RunClaim::from(&claimed.lease);
        if bound {
            let owner = Arc::new(
                awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(dispatch),
            );
            let worker = DispatchWorker::new_bound(
                Arc::new(Runtime::new()),
                awaken_dispatch_erasure::DispatchBinding::local(owner),
                committed.clone(),
                "worker",
            );
            prove_original_operation(worker, committed.as_ref(), claim).await;
        } else {
            let worker = DispatchWorker::new(
                Arc::new(Runtime::new()),
                dispatch,
                committed.clone(),
                "worker",
            );
            prove_original_operation(worker, committed.as_ref(), claim).await;
        }
    }
}
