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

use crate::dispatch::{
    Claimed, DispatchError, DispatchOutcome, Lease, PendingInbox, PendingInput, RunDispatch,
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

#[derive(Debug, Clone)]
struct Pend {
    input: PendingInput,
    frozen: bool,
}

#[derive(Debug, Default)]
struct State {
    /// Enqueue order, so claim is deterministic (oldest first).
    order: Vec<RunId>,
    rows: HashMap<RunId, Row>,
    pending: Vec<Pend>,
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
                row.status == Status::Parked
                    && state
                        .pending
                        .iter()
                        .any(|p| &p.input.run_id == run && !p.frozen)
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

        // Freeze any unconsumed pending input into this attempt.
        let mut pending = Vec::new();
        for pend in state.pending.iter_mut() {
            if pend.input.run_id == run_id && !pend.frozen {
                pend.frozen = true;
                pending.push(pend.input.clone());
            }
        }

        Ok(Some(Claimed {
            request,
            lease,
            pending,
        }))
    }

    async fn settle(&self, run_id: &RunId, outcome: DispatchOutcome) -> Result<(), DispatchError> {
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
                // Consumed (frozen) input is gone; input that arrived during the
                // attempt stays unfrozen for the next wake.
                state
                    .pending
                    .retain(|p| !(&p.input.run_id == run_id && p.frozen));
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
        state.pending.push(Pend {
            input,
            frozen: false,
        });
        Ok(true)
    }
}
