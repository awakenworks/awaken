// @formal entrypoint store_history.s3_publication_refines_committed_history
//! A deterministic read projection over immutable commits. This is never a live persistence backend.
use crate::{Error, Result};
use awaken_agent_contract::{
    agent::{
        awaiting::ResumeTicket,
        message::Message,
        run::{Id as RunId, Record as RunRecord},
        state::Command,
        thread::Id as ThreadId,
    },
    audit::record::Record as EventRecord,
    thread::{
        commit::{
            RunFact,
            operation::{
                CommitOperation, CommitOperationId, CommitReceipt, admit_commit_operation_append,
                applied_commit_receipt, replay_commit_operation_receipt,
                require_commit_operation_payload_hash,
            },
        },
        read::{
            checkpoint::{CheckpointReader, EventScope},
            committed_thread_view::{CommittedThreadView, select_open_wait},
            lifecycle::encode_run_lifecycle_cursor,
            recovery::{
                RecoveryError, RunRecoverySnapshot, RunRecoverySource, RunResumeTicket,
                project_committed_run_records,
            },
        },
    },
};
use std::collections::HashMap;
#[derive(Clone, Default)]
struct ThreadData {
    messages: Vec<Message>,
    message_cursors: Vec<u64>,
    times: std::collections::BTreeMap<u64, u64>,
    state: Vec<Command>,
    state_cursors: Vec<u64>,
    facts: Vec<RunFact>,
    events: Vec<EventRecord>,
}
/// Process-local execution view rebuilt from one captured authoritative head.
#[derive(Clone, Default)]
pub struct CommittedProjection {
    sequence: u64,
    threads: HashMap<ThreadId, ThreadData>,
    receipts: HashMap<CommitOperationId, (ThreadId, CommitReceipt)>,
    tickets: HashMap<RunId, ResumeTicket>,
}
impl CommittedProjection {
    pub fn commit_count(&self) -> u64 {
        self.sequence
    }
    pub(crate) fn inspect_operation(
        &self,
        operation: &CommitOperation,
    ) -> Result<Option<CommitReceipt>> {
        self.inspect(operation, true)
    }
    fn inspect(&self, operation: &CommitOperation, fresh: bool) -> Result<Option<CommitReceipt>> {
        operation.commit.validate().map_err(conflict)?;
        if let Some(receipt) = replay_commit_operation_receipt(
            operation,
            self.receipts
                .get(&operation.operation_id)
                .map(|(thread, receipt)| (thread, receipt)),
        )
        .map_err(Error::Admission)?
        {
            return Ok(Some(receipt));
        }
        if fresh {
            require_commit_operation_payload_hash(operation).map_err(conflict)?;
        }
        let thread = self.threads.get(&operation.commit.thread_id);
        admit_commit_operation_append(
            operation,
            thread.map_or(0, |t| t.facts.len() as u64),
            thread.map_or(0, |t| {
                t.facts
                    .iter()
                    .filter(|f| &f.run_id == operation.commit.run_id())
                    .count() as u64
            }),
        )
        .map_err(Error::Admission)?;
        if thread
            .and_then(|t| {
                t.facts
                    .iter()
                    .rev()
                    .find(|f| &f.run_id == operation.commit.run_id())
            })
            .is_some_and(|fact| !fact.state.permits(&operation.commit.run_state()))
        {
            return Err(Error::Conflict(
                "Run is already terminal; refusing post-terminal commit".into(),
            ));
        }
        let next = crate::model::next(self.sequence)?;
        for offset in 0..operation.commit.events.len() {
            encode_run_lifecycle_cursor(next, offset).map_err(conflict)?;
        }
        Ok(None)
    }
    pub(crate) fn apply(
        &mut self,
        operation: CommitOperation,
        at_ms: u64,
        fresh: bool,
    ) -> Result<CommitReceipt> {
        if let Some(receipt) = self.inspect(&operation, fresh)? {
            return Ok(receipt);
        }
        let sequence = crate::model::next(self.sequence)?;
        let thread_version = operation
            .expected_thread_version
            .checked_add(1)
            .ok_or_else(|| Error::Capacity("Thread version".into()))?;
        let receipt = applied_commit_receipt(
            operation.operation_id.clone(),
            operation.payload_hash.clone(),
            sequence,
            thread_version,
        );
        let commit = operation.commit;
        let run = commit.run_id().clone();
        let fact = commit.run_fact();
        if let Some(ticket) = commit.resume_ticket() {
            self.tickets.insert(run.clone(), ticket.clone());
        } else {
            self.tickets.remove(&run);
        }
        let thread = self.threads.entry(commit.thread_id.clone()).or_default();
        thread.times.insert(sequence, at_ms);
        thread
            .message_cursors
            .extend(std::iter::repeat_n(sequence, commit.messages.len()));
        thread.messages.extend(commit.messages);
        thread
            .state_cursors
            .extend(std::iter::repeat_n(sequence, commit.state.len()));
        thread.state.extend(commit.state);
        thread.facts.push(fact);
        for (offset, draft) in commit.events.into_iter().enumerate() {
            thread.events.push(EventRecord {
                sequence: encode_run_lifecycle_cursor(sequence, offset)
                    .map_err(conflict)?
                    .0,
                run_id: run.clone(),
                kind: draft.kind,
                payload: draft.payload,
            });
        }
        self.receipts
            .insert(operation.operation_id, (commit.thread_id, receipt.clone()));
        self.sequence = sequence;
        Ok(receipt)
    }
}
fn conflict(error: impl std::fmt::Display) -> Error {
    Error::Conflict(error.to_string())
}
impl CommittedThreadView for CommittedProjection {
    fn committed_messages(&self, t: &ThreadId) -> Vec<Message> {
        self.threads
            .get(t)
            .map(|t| t.messages.clone())
            .unwrap_or_default()
    }
    fn run(&self, run: &RunId) -> Option<RunRecord> {
        self.threads.iter().find_map(|(thread, t)| {
            t.facts
                .iter()
                .rev()
                .find(|f| &f.run_id == run)
                .map(|fact| RunRecord {
                    id: run.clone(),
                    thread_id: thread.clone(),
                    state: fact.state.clone(),
                })
        })
    }
    fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord> {
        self.threads.get(thread)?.facts.last().map(|f| RunRecord {
            id: f.run_id.clone(),
            thread_id: thread.clone(),
            state: f.state.clone(),
        })
    }
    fn resume_ticket(&self, run: &RunId) -> Option<ResumeTicket> {
        self.tickets.get(run).cloned()
    }
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        let latest = self.latest_run(thread)?;
        select_open_wait(thread, Some(&latest), self.tickets.get(&latest.id))
    }
    fn committed_state(&self, thread: &ThreadId) -> Vec<Command> {
        self.threads
            .get(thread)
            .map(|t| t.state.clone())
            .unwrap_or_default()
    }
}
#[async_trait::async_trait]
impl CheckpointReader for CommittedProjection {
    async fn list_events(
        &self,
        scope: &EventScope,
        from: Option<u64>,
        limit: usize,
    ) -> std::result::Result<Vec<EventRecord>, String> {
        let mut events = self
            .threads
            .iter()
            .filter(|(thread, _)| !matches!(scope,EventScope::Thread(id) if id!=*thread))
            .flat_map(|(_, t)| t.events.iter())
            .filter(|event| {
                event.sequence > from.unwrap_or(0)
                    && !matches!(scope,EventScope::Run(id) if id!=&event.run_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        events.sort_by_key(|e| e.sequence);
        events.truncate(limit);
        Ok(events)
    }
}
#[async_trait::async_trait]
impl RunRecoverySource for CommittedProjection {
    async fn recovery_snapshot(
        &self,
        thread: &ThreadId,
        run: &RunId,
    ) -> std::result::Result<RunRecoverySnapshot, RecoveryError> {
        let t = self.threads.get(thread).cloned().unwrap_or_default();
        let (runs, latest_run_id) = project_committed_run_records(
            thread,
            t.facts
                .iter()
                .map(|f| Ok::<_, RecoveryError>((f.run_id.clone(), f.state.clone()))),
        )?;
        let mut resume_tickets = self
            .tickets
            .iter()
            .filter(|(_, t)| &t.thread_id == thread)
            .map(|(run, t)| RunResumeTicket {
                run_id: run.clone(),
                ticket: t.clone(),
            })
            .collect::<Vec<_>>();
        resume_tickets.sort_by(|a, b| a.run_id.0.cmp(&b.run_id.0));
        Ok(RunRecoverySnapshot {
            thread_id: thread.clone(),
            claimed_run_id: run.clone(),
            runs,
            latest_run_id,
            next_commit_ordinal: t.facts.iter().filter(|f| &f.run_id == run).count() as u64,
            thread_version: t.facts.len() as u64,
            store_cursor: self.sequence,
            commit_timestamps: t.times,
            messages: t.messages,
            message_commit_cursors: t.message_cursors,
            state: t.state,
            state_commit_cursors: t.state_cursors,
            events: t.events,
            resume_tickets,
        })
    }
}
