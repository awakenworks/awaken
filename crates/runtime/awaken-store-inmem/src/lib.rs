//! In-memory reference adapters for the durable and live ports.
//!
//! These are a small, dependency-free implementation of `CommitCoordinator`,
//! `StreamSink`, and the read stores for local runs and tests. They keep the
//! plane split honest: live stream events go to [`MemoryStreamSink`] and never
//! become truth, while durable facts/events/messages are written atomically by
//! [`MemoryCommitCoordinator`] and read back for replay (G1/G13).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::record::Record as EventRecord;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink as StreamSink};
use awaken_agent_contract::thread::commit::RunFact;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitReceipt,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource, RunResumeTicket,
};

/// Everything one commit made durable, materialized as read models. This is the
/// after-commit truth that replay and projection consume.
#[derive(Debug, Default, Clone)]
pub struct CommittedThread {
    pub thread_id: Option<ThreadId>,
    pub messages: Vec<Message>,
    pub run_facts: Vec<RunFact>,
    pub state: Vec<StateCommand>,
    pub events: Vec<EventRecord>,
    pub latest_run: Option<RunRecord>,
}

#[derive(Debug, Default)]
struct CommitState {
    sequence: u64,
    /// Per-thread committed truth, keyed by thread id, so two threads committed to
    /// one store stay isolated on the read ports (a thread reads only its own
    /// transcript/state/runs, and never leaks into another's). A single-thread store
    /// holds exactly one entry, so its behavior is byte-for-byte what it was before
    /// thread-keying.
    threads: HashMap<ThreadId, CommittedThread>,
    /// Threads in first-commit order, so the flattened [`Self::flatten`] view
    /// (`committed()`) concatenates them deterministically.
    order: Vec<ThreadId>,
    /// Active awaiting tickets keyed by run; present only while a run is awaiting,
    /// so a resume against a terminal/resumed run finds nothing (fail closed).
    resume_tickets: HashMap<RunId, ResumeTicket>,
    /// Durable in the reference store's lifetime; filesystem persistence replays
    /// operation log entries through this same map.
    receipts: HashMap<CommitOperationId, CommitReceipt>,
}

impl CommitState {
    /// The per-thread committed truth for `thread_id`, or an empty view when the
    /// thread has no commits.
    fn thread(&self, thread_id: &ThreadId) -> CommittedThread {
        self.threads.get(thread_id).cloned().unwrap_or_default()
    }

    /// A single flattened view across every thread, in first-commit order — the
    /// backward-compatible shape [`MemoryCommitCoordinator::committed`] exposes. For
    /// a single-thread store this equals the one thread's truth; the flat
    /// `latest_run`/`thread_id` follow the last thread to receive a commit, matching
    /// the pre-thread-keyed reference. Multi-thread consumers read the isolated
    /// ports instead, never this union.
    fn flatten(&self) -> CommittedThread {
        let mut flat = CommittedThread::default();
        for thread_id in &self.order {
            if let Some(thread) = self.threads.get(thread_id) {
                flat.thread_id = Some(thread_id.clone());
                flat.messages.extend(thread.messages.iter().cloned());
                flat.run_facts.extend(thread.run_facts.iter().cloned());
                flat.state.extend(thread.state.iter().cloned());
                flat.events.extend(thread.events.iter().cloned());
                if thread.latest_run.is_some() {
                    flat.latest_run = thread.latest_run.clone();
                }
            }
        }
        flat
    }
}

/// In-memory atomic commit boundary. Each `commit` appends messages, facts, and
/// committed event records under one lock, then bumps the sequence.
#[derive(Debug, Default, Clone)]
pub struct MemoryCommitCoordinator {
    state: Arc<Mutex<CommitState>>,
}

impl MemoryCommitCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of committed truth, for replay/projection assertions. A flattened
    /// view across every thread (in first-commit order); for a single-thread store
    /// — the common case for this reference and its fs consumer — it is exactly that
    /// thread's truth. Isolated per-thread reads go through the read ports.
    pub fn committed(&self) -> CommittedThread {
        self.state
            .lock()
            .map(|state| state.flatten())
            .unwrap_or_default()
    }

    /// Number of commits applied so far.
    pub fn commit_count(&self) -> u64 {
        self.state.lock().map(|state| state.sequence).unwrap_or(0)
    }

    /// The active awaiting ticket for a run, if it is currently awaiting.
    pub fn resume_ticket_for(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.resume_tickets.get(run_id).cloned())
    }

    /// Validate an operation without applying it. Used by the filesystem adapter
    /// while it holds its append lock so rejection always happens before fsync.
    pub fn inspect_operation(
        &self,
        operation: &CommitOperation,
    ) -> Result<Option<CommitReceipt>, Error> {
        operation
            .commit
            .validate()
            .map_err(|error| Error::Rejected(error.to_string()))?;
        let state = self
            .state
            .lock()
            .map_err(|_| Error::Rejected("commit store poisoned".to_string()))?;
        inspect_operation_locked(&state, operation)
    }
}

#[async_trait]
impl CommitCoordinator for MemoryCommitCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        commit
            .validate()
            .map_err(|e| Error::Rejected(e.to_string()))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Rejected("commit store poisoned".to_string()))?;
        validate_transition_locked(&state, &commit)?;
        Ok(apply_commit_locked(&mut state, commit))
    }
}

#[async_trait]
impl OperationCoordinator for MemoryCommitCoordinator {
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        operation
            .commit
            .validate()
            .map_err(|error| Error::Rejected(error.to_string()))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Rejected("commit store poisoned".to_string()))?;
        if let Some(receipt) = inspect_operation_locked(&state, &operation)? {
            return Ok(receipt);
        }
        let operation_id = operation.operation_id;
        let payload_hash = operation.payload_hash;
        let expected_thread_version = operation.expected_thread_version;
        let thread_version = expected_thread_version
            .checked_add(1)
            .ok_or_else(|| Error::Rejected("Thread version overflow".to_string()))?;
        let commit = operation.commit;
        let record = apply_commit_locked(&mut state, commit);
        let receipt = CommitReceipt {
            operation_id: operation_id.clone(),
            commit_sequence: record.sequence,
            thread_version,
            payload_hash,
            duplicate: false,
        };
        state.receipts.insert(operation_id, receipt.clone());
        Ok(receipt)
    }
}

fn inspect_operation_locked(
    state: &CommitState,
    operation: &CommitOperation,
) -> Result<Option<CommitReceipt>, Error> {
    if operation.operation_id.run_id != *operation.commit.run_id() {
        return Err(Error::Rejected(
            "commit operation run_id does not match ThreadCommit run_id".to_string(),
        ));
    }
    if let Some(existing) = state.receipts.get(&operation.operation_id) {
        if existing.payload_hash != operation.payload_hash {
            return Err(Error::Rejected(format!(
                "commit operation {}:{} was reused with another payload",
                operation.operation_id.run_id.0, operation.operation_id.ordinal
            )));
        }
        let mut duplicate = existing.clone();
        duplicate.duplicate = true;
        return Ok(Some(duplicate));
    }
    let thread = state.threads.get(&operation.commit.thread_id);
    let current_thread_version = thread.map_or(0, |thread| thread.run_facts.len() as u64);
    if current_thread_version != operation.expected_thread_version {
        return Err(Error::Rejected(format!(
            "thread version conflict: expected {}, current {}",
            operation.expected_thread_version, current_thread_version
        )));
    }
    let current_run_ordinal = thread.map_or(0, |thread| {
        thread
            .run_facts
            .iter()
            .filter(|fact| fact.run_id == operation.operation_id.run_id)
            .count() as u64
    });
    if current_run_ordinal != operation.operation_id.ordinal {
        return Err(Error::Rejected(format!(
            "commit operation ordinal conflict: expected {}, current {}",
            operation.operation_id.ordinal, current_run_ordinal
        )));
    }
    validate_transition_locked(state, &operation.commit)?;
    Ok(None)
}

fn validate_transition_locked(state: &CommitState, commit: &ThreadCommit) -> Result<(), Error> {
    let run_id = commit.run_id();
    if state
        .threads
        .get(&commit.thread_id)
        .and_then(|thread| {
            thread
                .run_facts
                .iter()
                .rev()
                .find(|fact| &fact.run_id == run_id)
        })
        .is_some_and(|fact| !fact.state.permits(&commit.run_state()))
    {
        return Err(Error::Rejected(format!(
            "run {} is already terminal; refusing post-terminal commit",
            run_id.0
        )));
    }
    Ok(())
}

fn apply_commit_locked(state: &mut CommitState, commit: ThreadCommit) -> CommitRecord {
    let next = state.sequence + 1;
    let run_id = commit.run_id().clone();
    let run_state = commit.run_state();
    let run_fact = commit.run_fact();
    let resume_ticket = commit.resume_ticket().cloned();

    match (&resume_ticket, &run_state) {
        (Some(ticket), RunState::Awaiting) => {
            state.resume_tickets.insert(run_id.clone(), ticket.clone());
        }
        _ => {
            state.resume_tickets.remove(&run_id);
        }
    }

    let thread_id = commit.thread_id.clone();
    if !state.threads.contains_key(&thread_id) {
        state.order.push(thread_id.clone());
    }
    let thread = state.threads.entry(thread_id.clone()).or_default();
    thread.thread_id = Some(thread_id.clone());
    thread.messages.extend(commit.messages);
    thread.state.extend(commit.state);
    for (offset, draft) in commit.events.into_iter().enumerate() {
        thread.events.push(EventRecord {
            sequence: next * 1_000 + offset as u64,
            run_id: run_id.clone(),
            kind: draft.kind,
            payload: draft.payload,
        });
    }
    thread.run_facts.push(run_fact);
    thread.latest_run = Some(RunRecord {
        id: run_id,
        thread_id,
        state: run_state,
    });

    state.sequence = next;
    CommitRecord { sequence: next }
}

/// Committed thread truth is readable for resume through the contract read port:
/// the transcript and the active awaiting ticket, never the live sink (G1/G13).
impl CommittedThreadView for MemoryCommitCoordinator {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.state
            .lock()
            .map(|state| state.thread(thread_id).messages)
            .unwrap_or_default()
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.resume_ticket_for(run_id)
    }

    fn run(&self, id: &RunId) -> Option<RunRecord> {
        let state = self.state.lock().ok()?;
        // A run lives in exactly one thread; find the most recent fact for it,
        // scanning each thread's own fact log so a run in thread B is never
        // shadowed by thread A's.
        for thread_id in &state.order {
            if let Some(thread) = state.threads.get(thread_id)
                && let Some(fact) = thread
                    .run_facts
                    .iter()
                    .rev()
                    .find(|fact| &fact.run_id == id)
            {
                return Some(RunRecord {
                    id: fact.run_id.clone(),
                    thread_id: thread_id.clone(),
                    state: fact.state.clone(),
                });
            }
        }
        None
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.state.lock().ok().and_then(|state| {
            state
                .threads
                .get(thread_id)
                .and_then(|thread| thread.latest_run.clone())
        })
    }

    fn committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::state::Command> {
        self.state
            .lock()
            .map(|state| state.thread(thread_id).state)
            .unwrap_or_default()
    }
}

/// The merged after-commit read repository (ADR-0039 D1): the run record and
/// committed-event reads, on top of [`CommittedThreadView`]. Reads derive from committed
/// facts, so a fresh reader over the same state resumes correctly (ADR-0039 D4).
impl CheckpointReader for MemoryCommitCoordinator {
    fn list_events(&self, scope: &EventScope, from: Option<u64>, limit: usize) -> Vec<EventRecord> {
        let after = from.unwrap_or(0);
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return Vec::new(),
        };
        let mut events: Vec<EventRecord> = match scope {
            EventScope::All => state
                .order
                .iter()
                .filter_map(|tid| state.threads.get(tid))
                .flat_map(|thread| thread.events.iter().cloned())
                .collect(),
            EventScope::Run(run_id) => state
                .order
                .iter()
                .filter_map(|tid| state.threads.get(tid))
                .flat_map(|thread| thread.events.iter())
                .filter(|event| &event.run_id == run_id)
                .cloned()
                .collect(),
            EventScope::Thread(tid) => state
                .threads
                .get(tid)
                .map(|thread| thread.events.clone())
                .unwrap_or_default(),
        };
        // Committed events carry a globally monotonic sequence; sort so a run whose
        // events span threads (never, in practice) or a thread scope still reads in
        // commit order.
        events.sort_by_key(|event| event.sequence);
        events
            .into_iter()
            .filter(|event| event.sequence > after)
            .take(limit)
            .collect()
    }
}

#[async_trait]
impl RunRecoverySource for MemoryCommitCoordinator {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        let state = self
            .state
            .lock()
            .map_err(|_| RecoveryError::Rejected("commit store poisoned".to_string()))?;
        let thread = state.threads.get(thread_id);
        let mut runs: Vec<RunRecord> = Vec::new();
        if let Some(thread) = thread {
            for fact in &thread.run_facts {
                let record = RunRecord {
                    id: fact.run_id.clone(),
                    thread_id: thread_id.clone(),
                    state: fact.state.clone(),
                };
                if let Some(existing) = runs.iter_mut().find(|run| run.id == fact.run_id) {
                    *existing = record;
                } else {
                    runs.push(record);
                }
            }
        }
        let messages = thread
            .map(|thread| thread.messages.clone())
            .unwrap_or_default();
        let committed_state = thread
            .map(|thread| thread.state.clone())
            .unwrap_or_default();
        let thread_version = thread
            .map(|thread| thread.run_facts.len() as u64)
            .unwrap_or(0);
        let next_commit_ordinal = thread
            .map(|thread| {
                thread
                    .run_facts
                    .iter()
                    .filter(|fact| &fact.run_id == claimed_run_id)
                    .count() as u64
            })
            .unwrap_or(0);
        let mut resume_tickets: Vec<RunResumeTicket> = state
            .resume_tickets
            .iter()
            .filter(|(_, ticket)| &ticket.thread_id == thread_id)
            .map(|(run_id, ticket)| RunResumeTicket {
                run_id: run_id.clone(),
                ticket: ticket.clone(),
            })
            .collect();
        resume_tickets.sort_by(|left, right| left.run_id.0.cmp(&right.run_id.0));
        Ok(RunRecoverySnapshot {
            thread_id: thread_id.clone(),
            claimed_run_id: claimed_run_id.clone(),
            runs,
            latest_run_id: thread
                .and_then(|thread| thread.latest_run.as_ref().map(|run| run.id.clone())),
            messages,
            state: committed_state,
            resume_tickets,
            thread_version,
            store_cursor: state.sequence,
            next_commit_ordinal,
        })
    }
}

/// In-memory live stream sink. Records emitted events in order so tests can
/// assert live ordering independently of committed truth.
#[derive(Debug, Default, Clone)]
pub struct MemoryStreamSink {
    events: Arc<Mutex<Vec<StreamEvent>>>,
}

impl MemoryStreamSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<StreamEvent> {
        self.events.lock().map(|e| e.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl StreamSink for MemoryStreamSink {
    async fn send(&self, event: StreamEvent) -> Result<(), SinkError> {
        self.events
            .lock()
            .map_err(|_| SinkError::Closed)?
            .push(event);
        Ok(())
    }
}

/// Rebuild the materialized [`StateStore`] from committed state commands, proving
/// state replay derives from durable truth and not the live store (G1/G13).
pub fn replay_state(committed: &CommittedThread) -> awaken_agent_contract::agent::state::Store {
    awaken_agent_contract::agent::state::Store::rebuild(&committed.state)
}

/// Reconstruct the latest run [`RunState`] from committed facts (not live events),
/// proving replay reads durable truth (G1).
pub fn replay_latest_state(committed: &CommittedThread, run_id: &RunId) -> Option<RunState> {
    committed
        .run_facts
        .iter()
        .rev()
        .find(|fact| &fact.run_id == run_id)
        .map(|fact| fact.state.clone())
}

/// In-memory [`StreamCheckpointStore`]: a `run_id`-keyed map of interrupted-stream
/// partials. Durable only within one process, so it recovers an interrupted step
/// across an in-process worker restart but not a process crash — a durable
/// backend (e.g. `awaken-store-fs`) is required for cross-process resume. Useful
/// as the default for a single long-lived process and as a test double.
#[derive(Debug, Default)]
pub struct MemoryStreamCheckpointStore {
    checkpoints: Mutex<HashMap<String, StreamCheckpoint>>,
}

impl MemoryStreamCheckpointStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StreamCheckpointStore for MemoryStreamCheckpointStore {
    async fn get(&self, run_id: &str) -> Option<StreamCheckpoint> {
        self.checkpoints.lock().ok()?.get(run_id).cloned()
    }

    async fn put(&self, checkpoint: StreamCheckpoint) {
        if let Ok(mut map) = self.checkpoints.lock() {
            map.insert(checkpoint.run_id.clone(), checkpoint);
        }
    }

    async fn delete(&self, run_id: &str) {
        if let Ok(mut map) = self.checkpoints.lock() {
            map.remove(run_id);
        }
    }
}
