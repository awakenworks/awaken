// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Read-only evidence gathering moved from the Managed committed projector behind the
// @formal reason: SessionProjectionSource port: a root prefix before retained terminal Outcome queries,
// @formal reason: that root lent to coordinated-link projection, child prefixes, then the ordered
// @formal reason: merge of their lifecycle (the merge itself lives in awaken-managed-session-events). It
// @formal reason: publishes nothing; the Managed projector still re-reads and compares the same
// @formal reason: prefixes before its CAS publication. Warm/cold, read-order and prefix-race projection
// @formal reason: tests cover behavior; reducer TCB linked to the paired-publication obligation.
//! Read-only committed evidence for protocol Session projections.
//!
//! The Managed adapter used to gather this itself, straight from the Runtime.
//! It now reads through the Session application, which in turn sees only the
//! [`SessionProjectionSource`] port: committed Thread prefixes, coordinated
//! links and terminal Outcomes, never an execution context or Environment.

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_session_error_contract::RunError;
use awaken_session_event_contract::{SessionEventBatch, SessionEventCommand};
use awaken_managed_session_events::{
    ProjectionThreadSnapshots, SessionProjectionEvidence, SessionProjectionReadTimings,
    committed_lifecycle_prefix,
};
use awaken_session_execution_contract::{CoordinatedThreadLink, SessionProjectionSource};
use awaken_session_state_contract::PersistedSession;

use super::SessionApplication;

impl SessionApplication {
    fn projection_source(&self) -> &(impl SessionProjectionSource + ?Sized) {
        self.runtime()
    }

    /// Read one logical Thread prefix through the parent Session partition.
    /// A prefix for a different Thread is a Runtime fault, never data.
    pub async fn projection_thread_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<Option<RunRecoverySnapshot>, RunError> {
        let snapshot = self
            .projection_source()
            .projection_thread_snapshot(session_id, thread_id)
            .await?;
        if snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.thread_id.0 != thread_id)
        {
            return Err(RunError::internal(
                "Runtime returned a recovery snapshot for a different Session Thread",
            ));
        }
        Ok(snapshot)
    }

    /// Collect one deterministic coordinated-Thread prefix. Callers compare two
    /// collections around projection reads so independent child commits cannot
    /// form a mixed public prefix. Each link's latest Run is the one its own
    /// snapshot committed.
    pub async fn projection_coordinated_prefix(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&RunRecoverySnapshot>,
    ) -> Result<(Vec<CoordinatedThreadLink>, ProjectionThreadSnapshots), RunError> {
        let mut links = self
            .projection_source()
            .projection_coordinated_threads(session_id, observed_root)
            .await?;
        let mut snapshots = ProjectionThreadSnapshots::new();
        for link in &links {
            // A link may commit before its child has a first Run.
            if let Some(snapshot) = self
                .projection_thread_snapshot(session_id, &link.thread_id.0)
                .await?
            {
                snapshots.insert(link.thread_id.0.clone(), snapshot);
            }
        }
        for link in &mut links {
            if let Some(snapshot) = snapshots.get(&link.thread_id.0) {
                link.latest_run_id = snapshot.latest_run_id.clone();
            }
        }
        links.sort_by(|left, right| left.thread_id.0.cmp(&right.thread_id.0));
        Ok((links, snapshots))
    }

    /// Read the complete Runtime evidence of one projection candidate after
    /// the caller has read the durable Session root (so a Worker-side
    /// realization mutation joins the same candidate) and taken any
    /// process-local publication token it fences on.
    pub async fn read_projection_evidence(
        &self,
        persisted: &PersistedSession,
        started: awaken_lease_contract::mono::Stopwatch,
    ) -> Result<SessionProjectionEvidence, RunError> {
        let session_id = &persisted.session_id;
        // Read the root Thread first. A terminal Outcome query from before a
        // concurrent commit must never be paired with a newer root prefix that
        // already makes later facts listable.
        let root = self
            .projection_thread_snapshot(session_id, session_id.as_str())
            .await?;
        let mut outcome_terminals = Vec::new();
        let mut outcome_query_incomplete = false;
        for outcome_id in retained_outcome_ids(&persisted.event_batches) {
            let terminal = self
                .projection_source()
                .projection_outcome(session_id.as_str(), &outcome_id)
                .await?;
            let root_terminal = root
                .as_ref()
                .map(|snapshot| {
                    awaken_managed_session_events::outcome_terminal_commit_cursor(&outcome_id, snapshot)
                        .map_err(|error| RunError::internal(error.to_string()))
                })
                .transpose()?
                .flatten()
                .is_some();
            // A lagging terminal query must not let newer root facts pass it.
            // An ahead query has no anchor in this root and stays withheld;
            // the trailing root fence retries if it moved during the read.
            if root_terminal && terminal.is_none() {
                outcome_query_incomplete = true;
            }
            if let Some(terminal) = terminal {
                outcome_terminals.push((outcome_id, terminal));
            }
        }
        let outcomes = started.elapsed();
        // Lend that atomic root prefix to the
        // relationship projection: it includes every link committed at or
        // before this fence, so an unknown lifecycle event is classifiable
        // without guessing from its Thread id.
        let (links, children) = self
            .projection_coordinated_prefix(session_id, root.as_ref())
            .await?;
        let snapshots = started.elapsed();
        // Lifecycle is derived from these exact atomic prefixes; reopening the
        // global feed would scan unrelated Sessions and add an unfenced read.
        let lifecycle = committed_lifecycle_prefix(root.iter().chain(children.values()))
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok(SessionProjectionEvidence {
            outcome_terminals,
            outcome_query_incomplete,
            root,
            links,
            children,
            lifecycle,
            timings: SessionProjectionReadTimings {
                outcomes,
                snapshots,
                lifecycle: started.elapsed(),
            },
        })
    }
}

/// Outcome ids named by retained `DefineOutcome` provenance, first-seen order.
fn retained_outcome_ids(batches: &[SessionEventBatch]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    batches
        .iter()
        .flat_map(|batch| &batch.events)
        .filter_map(|entry| match &entry.event {
            SessionEventCommand::DefineOutcome { outcome_id, .. }
                if seen.insert(outcome_id.clone()) =>
            {
                Some(outcome_id.clone())
            }
            _ => None,
        })
        .collect()
}
