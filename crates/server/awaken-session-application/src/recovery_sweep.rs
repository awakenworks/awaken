//! Bounded iteration for independent roots within one Session recovery phase.

use std::future::Future;

use futures_util::{StreamExt, stream};

use super::{SessionRecoveryCandidate, SessionRecoveryCandidates};

/// Reuse the existing Event-phase concurrency budget for independent root I/O.
/// This is not Worker capacity or a second queue. Each caller still reloads its
/// root and owns all domain decisions and effects. A phase joins every result
/// before the next phase starts; dropping it drops every in-flight future.
pub(super) async fn map_candidates<T, F, Fut>(
    candidates: &SessionRecoveryCandidates,
    map: F,
) -> Vec<T>
where
    F: Fn(SessionRecoveryCandidate) -> Fut,
    Fut: Future<Output = T>,
{
    let mut pending = stream::iter(candidates.sessions.iter().cloned().enumerate())
        .map(|(index, candidate)| {
            let future = map(candidate);
            async move { (index, future.await) }
        })
        .buffer_unordered(4);
    let mut completed = Vec::with_capacity(candidates.sessions.len());
    while let Some(result) = pending.next().await {
        completed.push(result);
    }
    // A failed sibling cannot erase successes. Stable candidate order keeps the
    // existing reports independent of I/O completion order.
    completed.sort_unstable_by_key(|(index, _)| *index);
    completed.into_iter().map(|(_, result)| result).collect()
}
