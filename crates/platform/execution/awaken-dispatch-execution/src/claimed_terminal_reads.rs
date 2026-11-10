// @formal evidence background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
//! Committed truth a database-less Worker may read while it delivers one
//! claimed Run's committed terminal.
//!
//! A Worker's recovery projection is a process-local cache and never committed
//! authority. A terminal observer that must decide from committed truth reads
//! it instead from the Coordinator, under the claim that is delivering the
//! observed Run's terminal. The Dispatch Worker delivers terminal observers
//! after committed truth is visible and before it settles that claim, so the
//! claim is live for the whole delivery; outside a delivery there is no claim
//! to read under and the read is refused.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::{Claimed, ClaimedCheckpointTransport};
use awaken_sync::LeafLock;
use awaken_worker_contract::RunClaim;

/// One claimed terminal delivery in progress on this Worker.
struct Delivering {
    registration: u64,
    claimed: Claimed,
    transport: Arc<dyn ClaimedCheckpointTransport>,
}

/// Claim-bound committed reads for the terminal observers of one Session's
/// Dispatch Worker. Cloning shares the same deliveries.
#[derive(Clone, Default)]
pub struct ClaimedTerminalReads {
    delivering: Arc<LeafLock<BTreeMap<RunId, Delivering>>>,
    registrations: Arc<AtomicU64>,
}

/// Removes exactly its own delivery when the observers have been delivered.
pub(crate) struct DeliveringTerminal {
    reads: ClaimedTerminalReads,
    run_id: RunId,
    registration: u64,
}

impl Drop for DeliveringTerminal {
    fn drop(&mut self) {
        let mut delivering = self.reads.delivering.lock();
        if delivering
            .get(&self.run_id)
            .is_some_and(|current| current.registration == self.registration)
        {
            delivering.remove(&self.run_id);
        }
    }
}

/// Whether a committed read answered for exactly the delivering claim.
///
/// Decision table (rows R2/R5 of the remote Task control design):
/// | Rule | read Run is the claimed Run | read Thread is the claimed Thread | requested Thread is the claimed Thread | effect |
/// |---|---|---|---|---|
/// | R2 | yes | yes | yes | admitted |
/// | R5 | any other combination | | | refused |
#[must_use]
const fn claimed_terminal_read_admitted(
    run_matches: bool,
    thread_matches: bool,
    requested_thread_matches: bool,
) -> bool {
    run_matches && thread_matches && requested_thread_matches
}

impl ClaimedTerminalReads {
    /// Record that this Worker is delivering the committed terminal of
    /// `claimed`'s Run. The read is available until the returned value drops.
    pub(crate) fn deliver(
        &self,
        claimed: &Claimed,
        transport: Arc<dyn ClaimedCheckpointTransport>,
    ) -> DeliveringTerminal {
        let registration = self.registrations.fetch_add(1, Ordering::Relaxed);
        let run_id = claimed.lease.run_id().clone();
        self.delivering.lock().insert(
            run_id.clone(),
            Delivering {
                registration,
                claimed: claimed.clone(),
                transport,
            },
        );
        DeliveringTerminal {
            reads: self.clone(),
            run_id,
            registration,
        }
    }

    /// The Coordinator's committed State of `thread_id`, read under the claim
    /// that is delivering `run_id`'s committed terminal (rows R2-R5).
    pub async fn committed_state(
        &self,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<Vec<StateCommand>, String> {
        let delivering = self
            .delivering
            .lock()
            .get(run_id)
            .map(|delivering| (delivering.claimed.clone(), delivering.transport.clone()));
        let Some((claimed, transport)) = delivering else {
            return Err(format!(
                "no claim of Run {} is delivering its committed terminal on this Worker",
                run_id.0
            ));
        };
        let claim = RunClaim::from(&claimed.lease);
        let snapshot = transport
            .load_recovery_snapshot(&claim, claimed.session_work_authority.as_ref())
            .await
            .map_err(|error| format!("claim-bound committed read refused: {error}"))?;
        if !claimed_terminal_read_admitted(
            snapshot.claimed_run_id == claim.run_id,
            &snapshot.thread_id == claimed.request.thread_id(),
            thread_id == claimed.request.thread_id(),
        ) {
            return Err(format!(
                "claim-bound committed read for Run {} Thread {} does not match the delivering claim",
                run_id.0, thread_id.0
            ));
        }
        Ok(snapshot.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::stream::checkpoint::StreamCheckpoint;
    use awaken_agent_contract::thread::read::recovery::{
        LoadedRunRecovery, RunRecoverySnapshot, ThreadPrefixRef,
    };
    use awaken_dispatch::MemoryDispatchStore;
    use awaken_dispatch_contract::{DispatchClaims, DispatchEnqueue, DispatchError, SettleOutcome};
    use awaken_runtime_contract::ExecutableAgentSnapshot;
    use awaken_runtime_contract::activation::RunActivation;
    use std::sync::Mutex;

    /// The Coordinator's claim-guarded recovery read, scripted.
    struct ScriptedRecoveryRead {
        answer: Result<RunRecoverySnapshot, String>,
        claims: Mutex<Vec<RunClaim>>,
    }

    #[async_trait::async_trait]
    impl ClaimedCheckpointTransport for ScriptedRecoveryRead {
        async fn load_stream_checkpoint(
            &self,
            _claim: &RunClaim,
        ) -> Result<Option<StreamCheckpoint>, DispatchError> {
            unreachable!("committed reads never load a stream checkpoint")
        }

        async fn put_stream_checkpoint(
            &self,
            _claim: &RunClaim,
            _checkpoint: StreamCheckpoint,
        ) -> Result<SettleOutcome, DispatchError> {
            unreachable!("committed reads never write")
        }

        async fn delete_stream_checkpoint(
            &self,
            _claim: &RunClaim,
        ) -> Result<SettleOutcome, DispatchError> {
            unreachable!("committed reads never write")
        }

        async fn load_recovery(
            &self,
            claim: &RunClaim,
            _session_work_authority: Option<
                &awaken_session_application_contract::SessionWorkAuthorityReceipt,
            >,
            _cached: Option<ThreadPrefixRef>,
        ) -> Result<LoadedRunRecovery, DispatchError> {
            self.claims.lock().unwrap().push(claim.clone());
            self.answer
                .clone()
                .map(LoadedRunRecovery::full)
                .map_err(DispatchError::Rejected)
        }
    }

    fn snapshot(run: &str, thread: &str, state: Vec<StateCommand>) -> RunRecoverySnapshot {
        RunRecoverySnapshot {
            thread_id: ThreadId(thread.into()),
            claimed_run_id: RunId(run.into()),
            runs: Vec::new(),
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            commit_timestamps: Default::default(),
            state,
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 0,
            next_commit_ordinal: 0,
        }
    }

    async fn claimed(run: &str, thread: &str) -> Claimed {
        let store = MemoryDispatchStore::new(awaken_time::system_clock());
        store
            .enqueue(crate::RunDispatch::new(RunActivation::new(
                RunId(run.into()),
                ThreadId(thread.into()),
                ExecutableAgentSnapshot::builder("claimed-terminal-read-snapshot")
                    .model(
                        awaken_runtime_contract::resolved::ModelBinding::parse(
                            "test", "test", "native",
                        )
                        .expect("complete model binding"),
                    )
                    .build(),
                Vec::new(),
            )))
            .await
            .expect("enqueue");
        store
            .claim("worker-a", 30_000, &Default::default())
            .await
            .expect("claim")
            .expect("runnable")
    }

    fn transport(answer: Result<RunRecoverySnapshot, String>) -> Arc<ScriptedRecoveryRead> {
        Arc::new(ScriptedRecoveryRead {
            answer,
            claims: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn claimed_terminal_read_admits_only_the_delivering_claims_identity() {
        // R2 admits only when all three identities match; R5 every other row.
        for mask in 0u8..8 {
            assert_eq!(
                claimed_terminal_read_admitted(mask & 1 == 0, mask & 2 == 0, mask & 4 == 0),
                mask == 0,
                "mask={mask}"
            );
        }
    }

    #[tokio::test]
    async fn committed_state_is_read_from_the_coordinator_only_while_a_claim_delivers() {
        let claimed = claimed("run-a", "thread-a").await;
        let thread = ThreadId("thread-a".into());
        let run = RunId("run-a".into());
        let reads = ClaimedTerminalReads::default();
        let coordinator = transport(Ok(snapshot("run-a", "thread-a", Vec::new())));

        // R3: no delivery in progress, so no read reaches the Coordinator.
        assert!(reads.committed_state(&thread, &run).await.is_err(), "R3");
        assert!(coordinator.claims.lock().unwrap().is_empty(), "R3");

        // R2: the delivering claim, exactly, authorizes the Coordinator read.
        let delivering = reads.deliver(&claimed, coordinator.clone());
        assert_eq!(
            reads.committed_state(&thread, &run).await,
            Ok(Vec::new()),
            "R2"
        );
        assert_eq!(
            coordinator.claims.lock().unwrap().as_slice(),
            &[RunClaim::from(&claimed.lease)],
            "R2 reads under the exact delivering claim"
        );
        // R3: another Run has no delivering claim.
        assert!(
            reads
                .committed_state(&thread, &RunId("run-b".into()))
                .await
                .is_err(),
            "R3 other Run"
        );
        // R5: the requested Thread is not the claimed Run's.
        assert!(
            reads
                .committed_state(&ThreadId("thread-b".into()), &run)
                .await
                .is_err(),
            "R5 requested Thread"
        );

        // R3: after the delivery the claim settles; the read is gone.
        drop(delivering);
        assert!(
            reads.committed_state(&thread, &run).await.is_err(),
            "R3 settled"
        );
    }

    #[tokio::test]
    async fn a_fenced_or_foreign_coordinator_answer_is_refused() {
        let claimed = claimed("run-a", "thread-a").await;
        let thread = ThreadId("thread-a".into());
        let run = RunId("run-a".into());
        for (row, answer) in [
            ("R4", Err("claim fenced".to_owned())),
            ("R5 Run", Ok(snapshot("run-b", "thread-a", Vec::new()))),
            ("R5 Thread", Ok(snapshot("run-a", "thread-b", Vec::new()))),
        ] {
            let reads = ClaimedTerminalReads::default();
            let _delivering = reads.deliver(&claimed, transport(answer));
            assert!(reads.committed_state(&thread, &run).await.is_err(), "{row}");
        }
    }

    #[tokio::test]
    async fn a_finished_delivery_never_removes_a_newer_delivery_of_the_same_run() {
        let claimed = claimed("run-a", "thread-a").await;
        let thread = ThreadId("thread-a".into());
        let run = RunId("run-a".into());
        let reads = ClaimedTerminalReads::default();
        let older = reads.deliver(
            &claimed,
            transport(Err("older claim is superseded".to_owned())),
        );
        let newer = reads.deliver(
            &claimed,
            transport(Ok(snapshot("run-a", "thread-a", Vec::new()))),
        );
        drop(older);
        assert_eq!(
            reads.committed_state(&thread, &run).await,
            Ok(Vec::new()),
            "the newer delivery remains"
        );
        drop(newer);
        assert!(reads.committed_state(&thread, &run).await.is_err());
    }
}
