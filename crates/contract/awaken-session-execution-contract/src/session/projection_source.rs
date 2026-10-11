//! The read-only committed-truth port behind protocol projections.

use super::*;

/// Committed Session/Thread truth a protocol projection (Managed Events and
/// Session GET) is built from, and nothing else.
///
/// Consumer: `SessionApplication`'s projection-evidence reads, which the
/// Managed adapter calls instead of reaching into the Runtime itself. The
/// port deliberately exposes no Run, realization, Environment, or execution
/// context operation: a projection read that could build an execution context
/// is how a GET once installed physical Environment state and reported a
/// placeholder Agent. Implementations answer from committed commit-partition
/// reads only (the Runtime Host's `commit_for_read`).
#[async_trait]
pub trait SessionProjectionSource: Send + Sync {
    /// One internally consistent logical Thread prefix through the parent
    /// Session partition; `None` when the Thread has no committed Run.
    async fn projection_thread_snapshot(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
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

/// Every runtime owning committed Thread reads and coordination queries serves
/// the projection port through exactly those read methods, so no runtime can
/// offer a second, divergent projection read path.
#[async_trait]
impl<T> SessionProjectionSource for T
where
    T: ThreadReadRuntime + SessionCoordinationReads + ?Sized,
{
    async fn projection_thread_snapshot(
        &self,
        session_id: &crate::SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        ThreadReadRuntime::session_thread_recovery_snapshot(self, session_id, thread_id).await
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
        ThreadReadRuntime::committed_outcome_projection(self, thread, outcome_id).await
    }
}
