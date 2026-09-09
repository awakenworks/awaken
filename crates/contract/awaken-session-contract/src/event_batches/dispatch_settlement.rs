//! Exact post-Dispatch settlement of retained User Event commands.

use super::{
    SessionEventBatch, SessionEventBatchError, SessionEventCommand, SessionEventProjectionAnchor,
    mark_processed_observation,
};
use awaken_agent_contract::agent::run::Id as RunId;

/// Durable evidence that the exact User Run's Dispatch CAS returned Applied.
/// The Worker/transport supplies this only after the Dispatch owner settles;
/// the Session root then commits this receipt with activity removal and the
/// Event processed marker in one replacement CAS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEventDispatchSettlementReceipt {
    pub session_activity_epoch: u64,
    /// Commit cursor of the terminal/Awaiting Run boundary observed before the
    /// Dispatch CAS. This is deliberately distinct from `projection_anchor` on
    /// the Event entry, which freezes the earlier input-publication position.
    pub commit_observation_anchor: SessionEventProjectionAnchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEventDispatchSettlementPlan {
    Apply,
    Replay,
    Reject,
}

/// The sole finite decision joining an applied Dispatch receipt to the exact
/// retained User Event and activity epoch. Collection lookup and the root CAS
/// remain outside this reducer; no caller may mark a User Run processed through
/// a second completion rule.
#[must_use]
pub const fn session_event_dispatch_settlement_plan(
    command_matches: bool,
    processed: bool,
    active_activity_epoch: bool,
    existing: Option<SessionEventDispatchSettlementReceipt>,
    incoming: SessionEventDispatchSettlementReceipt,
) -> SessionEventDispatchSettlementPlan {
    if !command_matches
        || incoming.session_activity_epoch == 0
        || incoming.commit_observation_anchor.source_commit_cursor == 0
    {
        return SessionEventDispatchSettlementPlan::Reject;
    }
    match existing {
        Some(recorded)
            if recorded.session_activity_epoch == incoming.session_activity_epoch
                && recorded.commit_observation_anchor.source_commit_cursor
                    == incoming.commit_observation_anchor.source_commit_cursor
                && processed
                && !active_activity_epoch =>
        {
            SessionEventDispatchSettlementPlan::Replay
        }
        Some(_) => SessionEventDispatchSettlementPlan::Reject,
        None if !processed && active_activity_epoch => SessionEventDispatchSettlementPlan::Apply,
        None => SessionEventDispatchSettlementPlan::Reject,
    }
}

/// Decide whether the applied aggregate mutation may close Running. This does
/// not model the database or queue: it joins the exact production plan result
/// with the same root's activity-removal and Event-completeness facts.
#[must_use]
pub(crate) const fn session_event_execution_may_close(
    dispatch: SessionEventDispatchSettlementPlan,
    exact_activity_removed: bool,
    last_active: bool,
    all_events_processed: bool,
) -> bool {
    matches!(dispatch, SessionEventDispatchSettlementPlan::Apply)
        && exact_activity_removed
        && last_active
        && all_events_processed
}

impl SessionEventBatch {
    /// Commit the post-Dispatch receipt and processed observation for the exact
    /// retained User Run. `active_activity_epoch` comes from the same mutable
    /// Session root that removes the epoch before its replacement CAS.
    pub fn record_user_run_dispatch_settlement(
        &mut self,
        operation_id: &str,
        run_id: &RunId,
        active_activity_epoch: bool,
        receipt: SessionEventDispatchSettlementReceipt,
        processed_at_unix_ms: u64,
    ) -> Result<SessionEventDispatchSettlementPlan, SessionEventBatchError> {
        let entry_index = self
            .events
            .iter()
            .position(|entry| entry.event.operation_id() == operation_id)
            .ok_or(SessionEventBatchError::ProgressMismatch)?;
        let entry = &mut self.events[entry_index];
        let command_matches = matches!(
            &entry.event,
            SessionEventCommand::UserMessage { run_id: retained, .. } if retained == run_id
        );
        let plan = session_event_dispatch_settlement_plan(
            command_matches,
            entry.processed,
            active_activity_epoch,
            entry.dispatch_settlement,
            receipt,
        );
        if plan == SessionEventDispatchSettlementPlan::Apply {
            entry.dispatch_settlement = Some(receipt);
            let public_anchor = entry
                .projection_anchor
                .or(Some(receipt.commit_observation_anchor));
            mark_processed_observation(
                &mut entry.processed,
                &mut entry.projection_anchor,
                &mut entry.processed_at_unix_ms,
                public_anchor,
                processed_at_unix_ms,
            )?;
            if let Some(system) = self.events.get_mut(entry_index + 1)
                && matches!(system.event, SessionEventCommand::SystemMessage { .. })
            {
                mark_processed_observation(
                    &mut system.processed,
                    &mut system.projection_anchor,
                    &mut system.processed_at_unix_ms,
                    Some(receipt.commit_observation_anchor),
                    processed_at_unix_ms,
                )?;
            }
        }
        if plan == SessionEventDispatchSettlementPlan::Reject {
            return Err(SessionEventBatchError::ProgressMismatch);
        }
        Ok(plan)
    }
}

#[cfg(kani)]
#[kani::proof]
fn user_event_processing_requires_exact_applied_dispatch_and_activity_settlement() {
    // Cause/effect table exhausts command identity, processed/activity state,
    // optional prior receipt, and arbitrary u64 epoch/anchor coordinates. R1:
    // matching pending command + active non-zero epoch + no receipt => Apply;
    // R2 exact receipt + processed + inactive => Replay; every incomplete,
    // conflicting, zero, or partially applied combination rejects. R3 Running
    // may close only after R1, exact activity removal, final activity and full
    // retained Event completion all hold.
    let incoming = SessionEventDispatchSettlementReceipt {
        session_activity_epoch: kani::any(),
        commit_observation_anchor: SessionEventProjectionAnchor {
            source_commit_cursor: kani::any(),
        },
    };
    let existing = kani::any::<bool>().then_some(SessionEventDispatchSettlementReceipt {
        session_activity_epoch: kani::any(),
        commit_observation_anchor: SessionEventProjectionAnchor {
            source_commit_cursor: kani::any(),
        },
    });
    let command_matches = kani::any();
    let processed = kani::any();
    let active = kani::any();
    let plan = session_event_dispatch_settlement_plan(
        command_matches,
        processed,
        active,
        existing,
        incoming,
    );
    let valid = command_matches
        && incoming.session_activity_epoch != 0
        && incoming.commit_observation_anchor.source_commit_cursor != 0;
    assert_eq!(
        plan == SessionEventDispatchSettlementPlan::Apply,
        valid && existing.is_none() && !processed && active
    );
    assert_eq!(
        plan == SessionEventDispatchSettlementPlan::Replay,
        valid && existing == Some(incoming) && processed && !active
    );
    assert_eq!(
        plan == SessionEventDispatchSettlementPlan::Reject,
        !(valid && existing.is_none() && !processed && active)
            && !(valid && existing == Some(incoming) && processed && !active)
    );
    let exact_activity_removed = kani::any();
    let last_active = kani::any();
    let all_events_processed = kani::any();
    assert_eq!(
        session_event_execution_may_close(
            plan,
            exact_activity_removed,
            last_active,
            all_events_processed,
        ),
        plan == SessionEventDispatchSettlementPlan::Apply
            && exact_activity_removed
            && last_active
            && all_events_processed
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEventInput;
    use awaken_agent_contract::agent::content::ContentBlock;

    #[test]
    fn user_dispatch_settlement_decision_table_is_closed() {
        // Causes: exact/foreign User command, zero/non-zero epoch, pending/
        // processed marker, active/settled epoch and absent/exact/conflicting
        // receipt. Effects: only exact pending+active applies; only exact
        // processed+inactive replays; every torn or conflicting row rejects.
        // Kani exhausts all scalar coordinates and Boolean combinations.
        let receipt = SessionEventDispatchSettlementReceipt {
            session_activity_epoch: 7,
            commit_observation_anchor: SessionEventProjectionAnchor {
                source_commit_cursor: 11,
            },
        };
        assert_eq!(
            session_event_dispatch_settlement_plan(true, false, true, None, receipt),
            SessionEventDispatchSettlementPlan::Apply
        );
        assert_eq!(
            session_event_dispatch_settlement_plan(true, true, false, Some(receipt), receipt,),
            SessionEventDispatchSettlementPlan::Replay
        );
        for rejected in [
            session_event_dispatch_settlement_plan(false, false, true, None, receipt),
            session_event_dispatch_settlement_plan(
                true,
                false,
                true,
                None,
                SessionEventDispatchSettlementReceipt {
                    session_activity_epoch: 0,
                    ..receipt
                },
            ),
            session_event_dispatch_settlement_plan(true, false, false, None, receipt),
            session_event_dispatch_settlement_plan(true, true, true, Some(receipt), receipt),
            session_event_dispatch_settlement_plan(
                true,
                true,
                false,
                Some(SessionEventDispatchSettlementReceipt {
                    session_activity_epoch: 8,
                    ..receipt
                }),
                receipt,
            ),
        ] {
            assert_eq!(rejected, SessionEventDispatchSettlementPlan::Reject);
        }
        assert!(session_event_execution_may_close(
            SessionEventDispatchSettlementPlan::Apply,
            true,
            true,
            true,
        ));
        for retained in [
            session_event_execution_may_close(
                SessionEventDispatchSettlementPlan::Replay,
                true,
                true,
                true,
            ),
            session_event_execution_may_close(
                SessionEventDispatchSettlementPlan::Apply,
                false,
                true,
                true,
            ),
            session_event_execution_may_close(
                SessionEventDispatchSettlementPlan::Apply,
                true,
                false,
                true,
            ),
            session_event_execution_may_close(
                SessionEventDispatchSettlementPlan::Apply,
                true,
                true,
                false,
            ),
        ] {
            assert!(!retained, "incomplete settlement must retain execution");
        }

        // An accompanying System command is part of the exact User Run input
        // reservation. Its committed observation therefore settles in the same
        // aggregate mutation; otherwise the last activity could disappear
        // while the initial batch still had retained work.
        let mut batch = SessionEventBatch::compile(
            "session",
            "batch",
            vec![
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("user")],
                },
                SessionEventInput::SystemMessage {
                    content: vec![ContentBlock::text("system")],
                },
            ],
        )
        .unwrap();
        let (operation_id, run_id) = match &batch.events[0].event {
            SessionEventCommand::UserMessage {
                operation_id,
                run_id,
                ..
            } => (operation_id.clone(), run_id.clone()),
            _ => unreachable!(),
        };
        assert_eq!(
            batch
                .record_user_run_dispatch_settlement(
                    &operation_id,
                    &run_id,
                    true,
                    receipt,
                    1_700_000_000_000,
                )
                .unwrap(),
            SessionEventDispatchSettlementPlan::Apply
        );
        assert!(
            batch.is_complete(),
            "one Run settles its paired System input"
        );
    }
}
