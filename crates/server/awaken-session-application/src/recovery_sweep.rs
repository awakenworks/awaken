//! Bounded iteration for independent roots within one Session recovery phase.

use std::future::Future;

use futures_util::{StreamExt, stream};

use super::{SessionRecoveryCandidate, SessionRecoveryCandidates};

/// Event batches contain local durable commands whose per-Session ordering and
/// effect fencing remain inside the canonical driver. A wider bound prevents a
/// fleet-sized burst from being serialized into artificial waves here.
pub(super) const EVENT_RECOVERY_CONCURRENCY: usize = 16;

/// Resource reconciliation may cross external effect boundaries, so it retains
/// the earlier conservative fan-out independently of Event throughput.
pub(super) const RESOURCE_RECOVERY_CONCURRENCY: usize = 4;

/// New nonterminal Resource generations are on the Session start critical path.
/// When they exist, admit every one before a single background audit/terminal
/// cleanup candidate. The one background share preserves deterministic cleanup
/// progress without letting permanently slow old roots occupy the whole sweep.
pub(super) fn select_resource_candidates(
    candidates: &SessionRecoveryCandidates,
) -> Vec<SessionRecoveryCandidate> {
    let (mut activations, background): (Vec<_>, Vec<_>) = candidates
        .sessions
        .iter()
        .filter(|candidate| candidate.interests.resources)
        .cloned()
        .partition(|candidate| candidate.interests.resource_activation);
    if activations.is_empty() {
        return background;
    }
    activations.extend(background.into_iter().take(1));
    activations
}

/// One bounded scheduler for independent roots in every Session recovery phase.
/// This is not Worker capacity or a second queue. Each caller still reloads its
/// root and owns all domain decisions and effects. A phase joins every result
/// before the next phase starts; dropping it drops every in-flight future.
pub(super) async fn map_candidates<T, F, Fut>(
    candidates: &SessionRecoveryCandidates,
    max_in_flight: usize,
    map: F,
) -> Vec<T>
where
    F: Fn(SessionRecoveryCandidate) -> Fut,
    Fut: Future<Output = T>,
{
    assert!(
        max_in_flight > 0,
        "Session recovery concurrency must be positive"
    );
    let mut pending = stream::iter(candidates.sessions.iter().cloned().enumerate())
        .map(|(index, candidate)| {
            let future = map(candidate);
            async move { (index, future.await) }
        })
        .buffer_unordered(max_in_flight);
    let mut completed = Vec::with_capacity(candidates.sessions.len());
    while let Some(result) = pending.next().await {
        completed.push(result);
    }
    // A failed sibling cannot erase successes. Stable candidate order keeps the
    // existing reports independent of I/O completion order.
    completed.sort_unstable_by_key(|(index, _)| *index);
    completed.into_iter().map(|(_, result)| result).collect()
}
