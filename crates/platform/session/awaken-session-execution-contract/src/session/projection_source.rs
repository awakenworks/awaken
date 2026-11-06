//! The read-only committed-truth port behind Session projections.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// Project the original exact-Run fallback from an already-read prefix.
/// This performs no query and never changes committed facts, cursors or
/// ordinals; only the caller's claimed-Run read coordinate is selected.
/// Native exact stores still own the physical partition and ordinal query.
pub fn select_recovery_snapshot_run(
    snapshot: Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    run_id: &RunId,
) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError> {
    let Some(mut snapshot) = snapshot else {
        return Ok(None);
    };
    if snapshot
        .run_record(run_id)
        .map_err(|error| RunError::internal(error.to_string()))?
        .is_none()
    {
        return Ok(None);
    }
    snapshot.claimed_run_id = run_id.clone();
    Ok(Some(snapshot))
}

/// Committed Session/Thread truth for protocol projections and exact-Run
/// observations. Selecting a Run grants no execution or cleanup authority.
///
/// Consumer: `SessionApplication`'s committed projection reads, which the
/// Managed adapter calls instead of reaching into the Runtime itself. The
/// port deliberately exposes no Run execution, realization, Environment, or
/// context operation: a projection read that could build an execution context
/// is how a GET once installed physical Environment state and reported a
/// placeholder Agent. Implementations answer from committed commit-partition
/// reads only (the Runtime Host's `commit_for_read`).
///
/// ```compile_fail,E0599
/// use awaken_session_execution_contract::SessionProjectionSource;
/// async fn cannot_quiesce(source: &dyn SessionProjectionSource) {
///     // A selected committed Run is data; the pure role has no stop command.
///     source.quiesce_terminal_delegations("session").await;
/// }
/// ```
#[async_trait]
pub trait SessionProjectionSource: Send + Sync {
    /// One internally consistent logical Thread prefix through the explicit
    /// parent Session partition. `None` selects the latest committed Run;
    /// `Some` selects the supplied original Run while retaining every Run and
    /// state command in the prefix. An absent selected Run returns no snapshot;
    /// neither absence nor a selected snapshot proves physical quiescence.
    async fn projection_thread_snapshot(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
        selected_run: Option<&RunId>,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>;

    /// Coordinated Thread links derived from committed facts at or before the
    /// borrowed root prefix (or the current prefix when `None`).
    async fn projection_coordinated_threads(
        &self,
        session_id: &crate::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<crate::CoordinatedThreadLink>, RunError>;

    /// One exact terminal Outcome projection; `None` while absent or active.
    async fn projection_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<CommittedOutcomeProjection>, RunError>;
}

/// The projection port derived from the two read roles it is defined by.
///
/// A composition that selects `ThreadReads` and
/// `SessionCoordinationReads` from separate executor components derives its
/// projection here, so the projection can never read through a third owner:
/// it is exactly those two roles and nothing else.
#[derive(Clone)]
pub struct CommittedSessionProjection {
    thread_reads: Arc<dyn ThreadReads>,
    coordination_reads: Arc<dyn SessionCoordinationReads>,
}

impl CommittedSessionProjection {
    #[must_use]
    pub fn new(
        thread_reads: Arc<dyn ThreadReads>,
        coordination_reads: Arc<dyn SessionCoordinationReads>,
    ) -> Self {
        Self {
            thread_reads,
            coordination_reads,
        }
    }
}

#[async_trait]
impl SessionProjectionSource for CommittedSessionProjection {
    async fn projection_thread_snapshot(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
        selected_run: Option<&RunId>,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        projected_thread_snapshot(
            self.thread_reads.as_ref(),
            session_id,
            thread_id,
            selected_run,
        )
        .await
    }

    async fn projection_coordinated_threads(
        &self,
        session_id: &crate::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<crate::CoordinatedThreadLink>, RunError> {
        self.coordination_reads
            .coordinated_threads(session_id, observed_root)
            .await
    }

    async fn projection_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<CommittedOutcomeProjection>, RunError> {
        self.thread_reads
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

/// The one validated Thread prefix read every projection shares: an exact
/// selected Run, or the latest one, from the Thread read role. A snapshot of
/// another Thread or another selected Run is refused, never returned.
async fn projected_thread_snapshot<R: ThreadReads + ?Sized>(
    reads: &R,
    session_id: &crate::SessionId,
    thread_id: &str,
    selected_run: Option<&RunId>,
) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError> {
    let snapshot = match selected_run {
        Some(run_id) => {
            reads
                .session_thread_run_recovery_snapshot(session_id, thread_id, run_id)
                .await
        }
        None => {
            reads
                .session_thread_recovery_snapshot(session_id, thread_id)
                .await
        }
    }?;
    if snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.thread_id.0 != thread_id)
    {
        return Err(RunError::internal(
            "Runtime returned a recovery snapshot for a different Session Thread",
        ));
    }
    if let Some((snapshot, run)) = snapshot.as_ref().zip(selected_run)
        && (snapshot.claimed_run_id != *run
            || snapshot
                .run_record(run)
                .map_err(|error| RunError::internal(error.to_string()))?
                .is_none())
    {
        return Err(RunError::internal(
            "Runtime returned a recovery snapshot for a different selected Run",
        ));
    }
    Ok(snapshot)
}

/// Every runtime owning committed Thread reads and coordination queries serves
/// the projection port through exactly those read methods, so no runtime can
/// offer a second, divergent projection read path.
#[async_trait]
impl<T> SessionProjectionSource for T
where
    T: ThreadReads + SessionCoordinationReads + ?Sized,
{
    async fn projection_thread_snapshot(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
        selected_run: Option<&RunId>,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        projected_thread_snapshot(self, session_id, thread_id, selected_run).await
    }

    async fn projection_coordinated_threads(
        &self,
        session_id: &crate::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<crate::CoordinatedThreadLink>, RunError> {
        SessionCoordinationReads::coordinated_threads(self, session_id, observed_root).await
    }

    async fn projection_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<CommittedOutcomeProjection>, RunError> {
        ThreadReads::committed_outcome_projection(self, thread, outcome_id).await
    }
}
