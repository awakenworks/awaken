//! Exact selection and failure semantics, through the production adapter.
use super::*;
use awaken_dispatch_contract::{
    ClaimedCheckpointTransport, CommitEpochGuard, DispatchCommitAuthority, DispatchError,
    SettleOutcome,
};
use awaken_dispatch_contract::{DispatchClaims, DispatchEnqueue};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug)]
enum Answer {
    Current,
    Fenced,
    Unavailable,
}
struct Authority {
    backend: Arc<MemoryDispatchStore>,
    claim: RunClaim,
    answer: Answer,
    guards: AtomicUsize,
    remote: AtomicUsize,
}
#[async_trait::async_trait]
impl DispatchCommitAuthority for Authority {
    async fn worker_owns_run(
        &self,
        _: &awaken_dispatch_contract::WorkerIdentity,
        _: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<RunClaim>, DispatchError> {
        panic!("checkpoint selection requires the exact epoch guard")
    }
    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        assert_eq!(claim, &self.claim);
        self.guards.fetch_add(1, Ordering::SeqCst);
        match self.answer {
            Answer::Current => self.backend.lock_commit_epoch(claim).await,
            Answer::Fenced => Ok(None),
            Answer::Unavailable => Err(DispatchError::Unavailable("guard unavailable".into())),
        }
    }
}
impl Authority {
    fn remote_answer(&self, claim: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        assert_eq!(claim, &self.claim);
        self.remote.fetch_add(1, Ordering::SeqCst);
        match self.answer {
            Answer::Current => Ok(SettleOutcome::Applied),
            Answer::Fenced => Ok(SettleOutcome::Fenced),
            Answer::Unavailable => Err(DispatchError::Unavailable("RPC unavailable".into())),
        }
    }
}
#[async_trait::async_trait]
impl ClaimedCheckpointTransport for Authority {
    async fn load_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<StreamCheckpoint>, DispatchError> {
        match self.remote_answer(claim)? {
            SettleOutcome::Applied => Ok(Some(checkpoint("remote"))),
            SettleOutcome::Fenced => Err(DispatchError::Rejected("remote claim fenced".into())),
        }
    }
    async fn put_stream_checkpoint(
        &self,
        claim: &RunClaim,
        value: StreamCheckpoint,
    ) -> Result<SettleOutcome, DispatchError> {
        assert_eq!(value.run_id, claim.run_id.0);
        self.remote_answer(claim)
    }
    async fn delete_stream_checkpoint(
        &self,
        claim: &RunClaim,
    ) -> Result<SettleOutcome, DispatchError> {
        self.remote_answer(claim)
    }
    async fn load_recovery_snapshot(
        &self,
        _: &RunClaim,
        _: Option<&awaken_worker_contract::SessionWorkAuthorityReceipt>,
    ) -> Result<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot, DispatchError>
    {
        panic!("checkpoint operations cannot select committed recovery")
    }
}

async fn authority(answer: Answer) -> Arc<Authority> {
    let backend = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    backend
        .enqueue(RunDispatch::new(activation("run-fenced-checkpoint")))
        .await
        .unwrap();
    let claimed = backend
        .claim("matrix-owner", 60_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    Arc::new(Authority {
        backend,
        claim: RunClaim::from(&claimed.lease),
        answer,
        guards: AtomicUsize::new(0),
        remote: AtomicUsize::new(0),
    })
}

#[tokio::test]
async fn local_checkpoint_selection_never_falls_back_to_remote_on_guard_failure() {
    // Complete decision table: guard current/fenced/unavailable x genuine
    // checkpoint present/absent x exact/foreign Run x get/put/delete = 36 rows.
    // Exact current requests use only the local store. Fenced/unavailable and
    // foreign requests leave every stored value unchanged. Unavailable reads
    // stay Unknown(Storage), distinct from an absent optional checkpoint.
    for answer in [Answer::Current, Answer::Fenced, Answer::Unavailable] {
        for enabled in [false, true] {
            for exact in [false, true] {
                let authority = authority(answer).await;
                let inner = Arc::new(MemoryStreamCheckpointStore::new());
                inner.put(checkpoint("retained")).await.unwrap();
                let store = FencedStreamCheckpointStore::new(
                    enabled.then(|| inner.clone() as Arc<dyn StreamCheckpointStore>),
                    authority.clone(),
                    authority.claim.clone(),
                );
                let run = if exact {
                    "run-fenced-checkpoint"
                } else {
                    "foreign"
                };
                let got = store.get(run).await;
                let mut write = checkpoint("changed");
                write.run_id = run.into();
                let put = store.put(write).await;
                let delete = store.delete(run).await;
                let admitted = exact && matches!(answer, Answer::Current);
                assert_eq!(put.is_ok(), admitted, "{answer:?}/{enabled}/{exact}: put");
                assert_eq!(
                    delete.is_ok(),
                    admitted,
                    "{answer:?}/{enabled}/{exact}: delete"
                );
                match (exact, answer, enabled, got) {
                    (false, _, _, Observed::Unknown(UnknownCause::Fenced(_)))
                    | (true, Answer::Fenced, _, Observed::Unknown(UnknownCause::Fenced(_)))
                    | (true, Answer::Unavailable, _, Observed::Unknown(UnknownCause::Storage(_)))
                    | (true, Answer::Current, false, Observed::Absent) => {}
                    (true, Answer::Current, true, Observed::Present(value)) => {
                        assert_eq!(value.partial_text, "retained")
                    }
                    other => panic!("unexpected observation: {other:?}"),
                }
                assert_eq!(
                    authority.guards.load(Ordering::SeqCst),
                    if exact { 3 } else { 0 }
                );
                assert_eq!(
                    authority.remote.load(Ordering::SeqCst),
                    0,
                    "no remote fallback even though the same fixture implements RPC"
                );
                assert_eq!(inner.get("foreign").await, Observed::Absent);
                if admitted && enabled {
                    assert_eq!(inner.get("run-fenced-checkpoint").await, Observed::Absent);
                } else {
                    assert_eq!(
                        present(inner.get("run-fenced-checkpoint").await).partial_text,
                        "retained",
                        "rejected/disabled paths preserve the complete stored frame"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn remote_checkpoint_selection_calls_only_the_exact_claim_bound_transport() {
    // RPC applied/fenced/unavailable x exact/foreign Run x all three verbs = 18
    // rows. Foreign requests stop before I/O; no remote path asks a local guard.
    for answer in [Answer::Current, Answer::Fenced, Answer::Unavailable] {
        for exact in [false, true] {
            let authority = authority(answer).await;
            let store =
                FencedStreamCheckpointStore::remote(authority.clone(), authority.claim.clone());
            let run = if exact {
                "run-fenced-checkpoint"
            } else {
                "foreign"
            };
            let got = store.get(run).await;
            let mut write = checkpoint("remote write");
            write.run_id = run.into();
            let admitted = exact && matches!(answer, Answer::Current);
            assert_eq!(store.put(write).await.is_ok(), admitted);
            assert_eq!(store.delete(run).await.is_ok(), admitted);
            if admitted {
                assert_eq!(present(got).partial_text, "remote");
            } else {
                assert!(
                    matches!(got, Observed::Unknown(_)),
                    "remote refusal is never absence"
                );
            }
            assert_eq!(authority.guards.load(Ordering::SeqCst), 0);
            assert_eq!(
                authority.remote.load(Ordering::SeqCst),
                if exact { 3 } else { 0 }
            );
        }
    }
}
