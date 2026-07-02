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
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Phase, Record as RunRecord};
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::commit::coordinator::{Coordinator as CommitCoordinator, Error};
use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::event::record::Record as EventRecord;
use awaken_agent_contract::fact::run::Fact as RunFact;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_agent_contract::stream::event::Event as StreamEvent;
use awaken_agent_contract::stream::sink::{Error as SinkError, Sink as StreamSink};

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
    thread: CommittedThread,
    /// Active waiting tickets keyed by run; present only while a run is parked,
    /// so a resume against a terminal/resumed run finds nothing (fail closed).
    waiting: HashMap<RunId, WaitingTicket>,
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

    /// Snapshot of committed truth, for replay/projection assertions.
    pub fn committed(&self) -> CommittedThread {
        self.state
            .lock()
            .map(|state| state.thread.clone())
            .unwrap_or_default()
    }

    /// Number of commits applied so far.
    pub fn commit_count(&self) -> u64 {
        self.state.lock().map(|state| state.sequence).unwrap_or(0)
    }

    /// The active waiting ticket for a run, if it is currently parked.
    pub fn waiting_for(&self, run_id: &RunId) -> Option<WaitingTicket> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.waiting.get(run_id).cloned())
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

        let next = state.sequence + 1;
        let run_id = commit.run_fact.run_id.clone();
        let phase = commit.run_fact.phase.clone();

        // Park or clear the waiting ticket atomically with the checkpoint: a
        // `Some` ticket parks the run; any ended phase clears it so a
        // resumed/terminal run can no longer be resumed (G5).
        match (&commit.waiting, &phase) {
            (Some(ticket), Phase::Waiting) => {
                state.waiting.insert(run_id.clone(), ticket.clone());
            }
            _ => {
                state.waiting.remove(&run_id);
            }
        }

        let thread = &mut state.thread;
        thread.thread_id = Some(commit.thread_id.clone());
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
        thread.run_facts.push(commit.run_fact);
        thread.latest_run = Some(RunRecord {
            id: run_id,
            thread_id: commit.thread_id,
            phase,
        });

        state.sequence = next;
        Ok(CommitRecord { sequence: next })
    }
}

/// The committed run is readable through the contract read port — the same
/// after-commit truth a server projection would consume, never the live sink.
impl RunStore for MemoryCommitCoordinator {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        self.committed()
            .latest_run
            .filter(|record| &record.id == id)
    }
}

/// Committed thread truth is readable for resume through the contract read port:
/// the transcript and the active waiting ticket, never the live sink (G1/G13).
impl ThreadReader for MemoryCommitCoordinator {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        let committed = self.committed();
        match committed.thread_id {
            Some(id) if &id == thread_id => committed.messages,
            _ => Vec::new(),
        }
    }

    fn waiting_ticket(&self, run_id: &RunId) -> Option<WaitingTicket> {
        self.waiting_for(run_id)
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

/// Reconstruct the latest run [`Phase`] from committed facts (not live events),
/// proving replay reads durable truth (G1).
pub fn replay_latest_phase(committed: &CommittedThread, run_id: &RunId) -> Option<Phase> {
    committed
        .run_facts
        .iter()
        .rev()
        .find(|fact| &fact.run_id == run_id)
        .map(|fact| fact.phase.clone())
}
