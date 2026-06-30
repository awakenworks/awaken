//! In-memory reference implementation of the durable-ingress store.
//!
//! It mirrors the Postgres store's behaviour exactly so the worker and ingress
//! can be tested without a database (the same role [`MemoryCommitCoordinator`]
//! plays for the commit boundary). It is the executable specification of the
//! claim/lease/wake/recovery rules; the Postgres store must match it.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::resume::ResumeResult;

use crate::dispatch::{
    CasOutcome, Claimed, DispatchError, DispatchOutcome, Lease, PendingInbox, PendingInput,
    PendingRecord, RunDispatch,
};
use crate::request::RunExecutionRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Running,
    Parked,
}

#[derive(Debug, Clone)]
struct Row {
    request: RunExecutionRequest,
    status: Status,
    lease: Option<Lease>,
}

/// A pending input with its optimistic-concurrency revision.
#[derive(Debug, Clone)]
struct PendingRow {
    input: PendingInput,
    revision: u64,
}

#[derive(Debug, Default)]
struct State {
    /// Enqueue order, so claim is deterministic (oldest first).
    order: Vec<RunId>,
    rows: HashMap<RunId, Row>,
    /// Undelivered pending input, in arrival order.
    pending: Vec<PendingRow>,
}

/// In-memory durable-ingress store. Cloneable handles share one state.
#[derive(Debug, Default)]
pub struct MemoryDispatchStore {
    state: Mutex<State>,
}

impl MemoryDispatchStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live dispatch rows (test introspection).
    pub fn dispatch_count(&self) -> usize {
        self.state.lock().map(|s| s.rows.len()).unwrap_or(0)
    }

    /// Unconsumed pending inputs for a run (test introspection).
    pub fn pending_count(&self, run_id: &RunId) -> usize {
        self.state
            .lock()
            .map(|s| {
                s.pending
                    .iter()
                    .filter(|p| &p.input.run_id == run_id)
                    .count()
            })
            .unwrap_or(0)
    }
}

fn lock(state: &Mutex<State>) -> Result<std::sync::MutexGuard<'_, State>, DispatchError> {
    state
        .lock()
        .map_err(|_| DispatchError::Rejected("dispatch store poisoned".to_string()))
}

/// Pick the next runnable run, oldest-first within each priority band: reclaim an
/// expired lease (recovery), then wake a parked run with pending input, then a
/// fresh pending run. This is the claim policy the Postgres store must match.
fn select(state: &State, now_ms: u64) -> Option<RunId> {
    let runnable = |want: Status, run: &RunId, row: &Row| -> bool {
        match want {
            Status::Running => {
                row.status == Status::Running
                    && row.lease.as_ref().is_some_and(|l| l.expires_ms <= now_ms)
            }
            Status::Parked => {
                row.status == Status::Parked && state.pending.iter().any(|p| &p.input.run_id == run)
            }
            Status::Pending => row.status == Status::Pending,
        }
    };
    for band in [Status::Running, Status::Parked, Status::Pending] {
        for run in &state.order {
            if let Some(row) = state.rows.get(run)
                && runnable(band, run, row)
            {
                return Some(run.clone());
            }
        }
    }
    None
}

#[async_trait]
impl RunDispatch for MemoryDispatchStore {
    async fn enqueue(&self, request: RunExecutionRequest) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        let run_id = request.run_id().clone();
        // Idempotent: a re-enqueued run is a no-op (exactly-once effect).
        if state.rows.contains_key(&run_id) {
            return Ok(());
        }
        state.rows.insert(
            run_id.clone(),
            Row {
                request,
                status: Status::Pending,
                lease: None,
            },
        );
        state.order.push(run_id);
        Ok(())
    }

    async fn claim(
        &self,
        owner: &str,
        lease_ms: u64,
        now_ms: u64,
    ) -> Result<Option<Claimed>, DispatchError> {
        let mut state = lock(&self.state)?;

        let Some(run_id) = select(&state, now_ms) else {
            return Ok(None);
        };

        let lease = Lease {
            run_id: run_id.clone(),
            owner: owner.to_string(),
            expires_ms: now_ms + lease_ms,
        };
        let request = {
            let row = state.rows.get_mut(&run_id).expect("picked row exists");
            row.status = Status::Running;
            row.lease = Some(lease.clone());
            row.request.clone()
        };

        // Hand the run's current pending input to the worker. It is not removed
        // here: settle removes exactly what the worker reports it consumed, so a
        // crash before settle leaves the input to be re-derived (ADR-0010).
        let pending = state
            .pending
            .iter()
            .filter(|p| p.input.run_id == run_id)
            .map(|p| p.input.clone())
            .collect();

        Ok(Some(Claimed {
            request,
            lease,
            pending,
        }))
    }

    async fn settle(
        &self,
        run_id: &RunId,
        outcome: DispatchOutcome,
        consumed: &[String],
    ) -> Result<(), DispatchError> {
        let mut state = lock(&self.state)?;
        match outcome {
            DispatchOutcome::Done => {
                state.rows.remove(run_id);
                state.order.retain(|r| r != run_id);
                state.pending.retain(|p| &p.input.run_id != run_id);
            }
            DispatchOutcome::Parked => {
                if let Some(row) = state.rows.get_mut(run_id) {
                    row.status = Status::Parked;
                    row.lease = None;
                }
                // Drop only what the worker consumed; input that arrived during
                // the attempt stays for the next wake.
                state
                    .pending
                    .retain(|p| !consumed.contains(&p.input.message_id));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl PendingInbox for MemoryDispatchStore {
    async fn append(&self, input: PendingInput) -> Result<bool, DispatchError> {
        let mut state = lock(&self.state)?;
        if state
            .pending
            .iter()
            .any(|p| p.input.message_id == input.message_id)
        {
            return Ok(false);
        }
        state.pending.push(PendingRow { input, revision: 1 });
        Ok(true)
    }

    async fn list(&self, thread_id: &ThreadId) -> Result<Vec<PendingRecord>, DispatchError> {
        let state = lock(&self.state)?;
        Ok(state
            .pending
            .iter()
            .filter(|p| &p.input.thread_id == thread_id)
            .map(|p| PendingRecord {
                input: p.input.clone(),
                revision: p.revision,
            })
            .collect())
    }

    async fn retract(
        &self,
        message_id: &str,
        expected_revision: u64,
    ) -> Result<CasOutcome, DispatchError> {
        let mut state = lock(&self.state)?;
        let Some(pos) = state
            .pending
            .iter()
            .position(|p| p.input.message_id == message_id)
        else {
            return Ok(CasOutcome::NotFound);
        };
        if state.pending[pos].revision != expected_revision {
            return Ok(CasOutcome::RevisionMismatch);
        }
        state.pending.remove(pos);
        Ok(CasOutcome::Applied)
    }

    async fn edit(
        &self,
        message_id: &str,
        expected_revision: u64,
        result: ResumeResult,
    ) -> Result<CasOutcome, DispatchError> {
        let mut state = lock(&self.state)?;
        let Some(row) = state
            .pending
            .iter_mut()
            .find(|p| p.input.message_id == message_id)
        else {
            return Ok(CasOutcome::NotFound);
        };
        if row.revision != expected_revision {
            return Ok(CasOutcome::RevisionMismatch);
        }
        row.input.result = result;
        row.revision += 1;
        Ok(CasOutcome::Applied)
    }
}
