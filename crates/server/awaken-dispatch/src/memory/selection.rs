// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Static owner: read-only broad candidate selection of the reference Dispatch store.
// @formal reason: Dynamic path: candidates observe the store clock, physical occupancy, exact-claim modes,
// @formal reason: thread peers, compatibility and persisted retry deadlines; the exact claim transaction
// @formal reason: remains mutation authority. Dispatch conformance and PostgreSQL discovery comparisons
// @formal reason: own the behavior evidence; no whole-selection or liveness proof is claimed.
//! The reference store's one broad claim selection policy.

use super::*;

/// Pick the next runnable run, oldest-first within each priority band: reclaim an
/// expired lease (recovery), then wake an awaiting run with pending input, then a
/// fresh pending run. This is the claim policy the SQL stores must match.
pub(super) fn select_where(
    state: &State,
    now_ms: u64,
    mut compatible: impl FnMut(&Row) -> bool,
) -> Option<RunId> {
    // An unconfirmed predecessor still needs recovery observation, but cannot
    // monopolize the recovery-priority band while unrelated work is runnable.
    // This changes selection only; exact claim, handoff and ACK remain fenced.
    select_in_physical_band(state, now_ms, &mut compatible, false)
        .or_else(|| select_in_physical_band(state, now_ms, &mut compatible, true))
}

fn select_in_physical_band(
    state: &State,
    now_ms: u64,
    compatible: &mut impl FnMut(&Row) -> bool,
    occupied: bool,
) -> Option<RunId> {
    let now_ms = crate::clock::normalize_millis(now_ms);
    // Session admission repair is a distinct claim mode: the resulting Worker
    // lease may call only the root activity authority until it is resolved.
    // Both reservation phases use the same deadline classifier as exact claims.
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.active_attempt.is_some() == occupied
            && classify_exact_claim_mode(row.state, row.exact_claim_deadline(), now_ms)
                == ExactClaimMode::ReservationRecovery
            && retry_deadline_is_due(row, now_ms)
            && !thread_has_claim_blocking_peer(state, run, false)
        {
            return Some(run.clone());
        }
    }

    // Cancellation is terminal control, not ordinary work. Once its owning lease
    // is free, claim it before wakes/fresh runs. Awaiting peers do not block this
    // branch, which lets a legacy multi-Awaiting Thread be cancelled sequentially.
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.active_attempt.is_some() == occupied
            && row.cancellation_requested
            && matches!(row.state, DispatchState::Pending | DispatchState::Awaiting)
            && retry_deadline_is_due(row, now_ms)
            && !thread_has_claim_blocking_peer(state, run, false)
            && compatible(row)
        {
            return Some(run.clone());
        }
    }

    // Recovery: re-own an expired-lease running row (first-match in enqueue order).
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.active_attempt.is_some() == occupied
            && row.state == DispatchState::Leased
            && crate::clock::deadline_has_passed(row.lease.as_ref().map(|l| l.expires_ms()), now_ms)
            && retry_deadline_is_due(row, now_ms)
            && compatible(row)
        {
            return Some(run.clone());
        }
    }
    // Wake: an awaiting run with due input whose Thread has no other open Run.
    // Excluding this exact row is what lets its reply wake it without opening a
    // fresh peer execution.
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.active_attempt.is_some() == occupied
            && row.state == DispatchState::Awaiting
            && retry_deadline_is_due(row, now_ms)
            && state
                .pending
                .iter()
                .any(|p| &p.input.run_id == run && is_due(&p.input, now_ms))
            && !thread_has_claim_blocking_peer(state, run, true)
            && compatible(row)
        {
            return Some(run.clone());
        }
    }
    // Fresh work is ordered by priority (highest first), then enqueue order, and
    // only for Threads that have neither a Running nor Awaiting peer.
    let mut best: Option<(&RunId, i64)> = None;
    for run in &state.order {
        if let Some(row) = state.rows.get(run)
            && row.active_attempt.is_some() == occupied
            && row.state == DispatchState::Pending
            && retry_deadline_is_due(row, now_ms)
            && !thread_has_claim_blocking_peer(state, run, true)
            && compatible(row)
            && best.is_none_or(|(_, p)| row.priority > p)
        {
            best = Some((run, row.priority));
        }
    }
    best.map(|(run, _)| run.clone())
}

/// Whether another open Run on the candidate's Thread blocks this claim.
///
/// The candidate is deliberately excluded: an Awaiting Run must be able to wake
/// itself from its exact reply or cancellation. Cancellation only waits for an
/// executing peer, so historical stores containing more than one Awaiting row can
/// drain those rows one at a time instead of deadlocking forever.
pub(super) fn thread_has_claim_blocking_peer(
    state: &State,
    candidate_run: &RunId,
    include_awaiting: bool,
) -> bool {
    let Some(candidate) = state.rows.get(candidate_run) else {
        return false;
    };
    state.rows.iter().any(|(run_id, peer)| {
        run_id != candidate_run
            && peer.request.thread_id() == candidate.request.thread_id()
            && (matches!(
                peer.state,
                DispatchState::ReservationLeased | DispatchState::Leased
            ) || (include_awaiting && peer.state == DispatchState::Awaiting))
    })
}

pub(super) fn retry_deadline_is_due(row: &Row, now_ms: u64) -> bool {
    awaken_dispatch_execution::pool::retry_deadline_is_due(row.retry_not_before_ms, now_ms)
}

/// Whether one exact row is runnable under the same policy as [`select_where`]. The
/// boolean says that the claim is crash recovery and must spend retry budget.
pub(super) fn runnable(state: &State, run_id: &RunId, now_ms: u64) -> Option<bool> {
    let now_ms = crate::clock::normalize_millis(now_ms);
    let row = state.rows.get(run_id)?;
    if !retry_deadline_is_due(row, now_ms) {
        return None;
    }
    if classify_exact_claim_mode(row.state, row.exact_claim_deadline(), now_ms)
        == ExactClaimMode::ReservationRecovery
        && !thread_has_claim_blocking_peer(state, run_id, false)
    {
        return Some(false);
    }
    if row.state == DispatchState::Leased
        && crate::clock::deadline_has_passed(
            row.lease.as_ref().map(|lease| lease.expires_ms()),
            now_ms,
        )
    {
        return Some(true);
    }
    let thread_busy = thread_has_claim_blocking_peer(state, run_id, !row.cancellation_requested);
    if thread_busy {
        return None;
    }
    if row.state == DispatchState::Awaiting
        && (row.cancellation_requested
            || state
                .pending
                .iter()
                .any(|pending| pending.input.run_id == *run_id && is_due(&pending.input, now_ms)))
    {
        return Some(false);
    }
    (row.state == DispatchState::Pending).then_some(false)
}
