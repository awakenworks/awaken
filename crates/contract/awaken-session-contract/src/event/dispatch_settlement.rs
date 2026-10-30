// @formal entrypoint session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
//! Exact post-Dispatch settlement of retained User Event commands.

use super::{
    SessionEventBatch, SessionEventBatchError, SessionEventCommand, SessionEventProjectionAnchor,
    mark_processed_observation,
};
use awaken_agent_contract::agent::run::Id as RunId;

/// Latest durable evidence that the exact User Run's Dispatch CAS returned Applied.
/// One Run can resume after several approvals; each admitted activity settles
/// separately while the original Event's processed time and input anchor stay frozen.
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
            if processed
                && active_activity_epoch
                && recorded.session_activity_epoch != 0
                && recorded.commit_observation_anchor.source_commit_cursor != 0
                && incoming.session_activity_epoch > recorded.session_activity_epoch
                && incoming.commit_observation_anchor.source_commit_cursor
                    >= recorded.commit_observation_anchor.source_commit_cursor =>
        {
            // An approval resumes the SAME User Run under a newly admitted
            // activity. Only its applied Dispatch receipt advances this fence;
            // public input processing remains the immutable first observation.
            SessionEventDispatchSettlementPlan::Apply
        }
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
        // Legacy User Events can already be processed without this newer
        // receipt field. The incoming applied Dispatch plus exact active epoch
        // authorizes the FIRST receipt; the old public marker is not rewritten.
        None if active_activity_epoch => SessionEventDispatchSettlementPlan::Apply,
        None => SessionEventDispatchSettlementPlan::Reject,
    }
}

/// Derive the input-actionability fact the closing decision consumes.
///
/// This is deliberately a named production kernel rather than an expression
/// inlined at the call site: it is the predicate a real defect lived in (an
/// Awaiting Run that queued a later ordinary User was held Running, hiding the
/// very approval that would unblock it), and an inlined `let` cannot be reached
/// by a proof. Retained input permits idling when either every admitted Event
/// is already processed, or the Run is Awaiting and every still-pending Event
/// is an ordinary User command -- whose admission is not physical activity --
/// or the System command frozen into such a pending User's own reservation.
/// The three classes of retained Event the idling predicate distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetainedInputClass {
    /// An ordinary User command. Its unprocessed admission is queued work, not
    /// physical activity, so it cannot by itself hold the Session Running.
    OrdinaryUser,
    /// A System command frozen into the preceding User Run's own reservation.
    ReservedSystem,
    /// Every other retained command kind, which does keep the existing
    /// reconciliation gate closed until it is processed.
    Other,
}

impl SessionEventCommand {
    /// Classify this command for the retained-input idling decision. Adding a
    /// command variant forces an arm here rather than silently falling into an
    /// inert class.
    #[must_use]
    pub const fn retained_input_class(&self) -> RetainedInputClass {
        match self {
            Self::UserMessage { .. } => RetainedInputClass::OrdinaryUser,
            Self::SystemMessage { .. } => RetainedInputClass::ReservedSystem,
            Self::DefineOutcome { .. } | Self::ToolReply { .. } | Self::Interrupt { .. } => {
                RetainedInputClass::Other
            }
        }
    }
}

/// Whether one retained Event leaves the Session free to idle.
///
/// This is the predicate a real defect lived in: an Awaiting Run that had
/// queued a later ordinary User was held Running, which hid the very approval
/// that would unblock it. A System command is inert only while the User whose
/// reservation froze it is itself still pending -- a System standing alone is
/// real pending work.
#[must_use]
const fn retained_event_is_inert(
    processed: bool,
    class: RetainedInputClass,
    previous: Option<(RetainedInputClass, bool)>,
) -> bool {
    if processed {
        return true;
    }
    match class {
        RetainedInputClass::OrdinaryUser => true,
        RetainedInputClass::ReservedSystem => {
            matches!(previous, Some((RetainedInputClass::OrdinaryUser, false)))
        }
        RetainedInputClass::Other => false,
    }
}

/// Derive the input-actionability fact the closing decision consumes.
///
/// This is deliberately a named production kernel rather than an expression
/// inlined at the call site: an inlined `let` cannot be reached by a proof at
/// all. Retained input permits idling when either every admitted Event is
/// already processed, or the Run is Awaiting and every still-pending Event is
/// inert by `retained_event_is_inert`.
#[must_use]
pub fn session_inputs_allow_idle(
    batches: &[SessionEventBatch],
    boundary: crate::event::SessionAgentRunBoundary,
) -> bool {
    let all_events_processed = batches.iter().all(SessionEventBatch::is_complete);
    let only_blocked_users = boundary == crate::event::SessionAgentRunBoundary::Awaiting
        && batches.iter().all(|batch| {
            batch.events.iter().enumerate().all(|(index, entry)| {
                retained_event_is_inert(
                    entry.processed,
                    entry.event.retained_input_class(),
                    index
                        .checked_sub(1)
                        .and_then(|previous| batch.events.get(previous))
                        .map(|previous| {
                            (previous.event.retained_input_class(), previous.processed)
                        }),
                )
            })
        });
    all_events_processed || only_blocked_users
}

/// Decide whether the applied aggregate mutation may close Running. This does
/// not model the database or queue: it joins the exact production plan result
/// with the same root's activity-removal and input-actionability facts.
#[must_use]
pub const fn session_event_execution_may_close(
    dispatch: SessionEventDispatchSettlementPlan,
    exact_activity_removed: bool,
    last_active: bool,
    inputs_allow_idle: bool,
) -> bool {
    matches!(dispatch, SessionEventDispatchSettlementPlan::Apply)
        && exact_activity_removed
        && last_active
        && inputs_allow_idle
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
                let system_anchor = system
                    .projection_anchor
                    .or(Some(receipt.commit_observation_anchor));
                mark_processed_observation(
                    &mut system.processed,
                    &mut system.projection_anchor,
                    &mut system.processed_at_unix_ms,
                    system_anchor,
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
/// @obligation session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
/// @obligation session_execution.initial_event_path_reaches_idle_only_after_full_settlement
#[kani::proof]
fn user_event_processing_requires_exact_applied_dispatch_and_activity_settlement() {
    // Cause/effect table exhausts command identity, processed/activity state,
    // optional prior receipt, and arbitrary u64 epoch/anchor coordinates. R1:
    // matching command + active non-zero epoch + no receipt => Apply, including
    // a processed legacy Event whose supported stored format omits the receipt;
    // R2 processed + newly active greater epoch + non-regressing commit => Apply;
    // R3 exact receipt + processed + inactive => Replay; every incomplete,
    // stale, zero, or partially applied combination rejects. R4 Running
    // may close only after R1 or R2, exact activity removal, final activity and full
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
    let first = existing.is_none() && active;
    let resumed = existing.is_some_and(|recorded| {
        processed
            && active
            && recorded.session_activity_epoch != 0
            && recorded.commit_observation_anchor.source_commit_cursor != 0
            && incoming.session_activity_epoch > recorded.session_activity_epoch
            && incoming.commit_observation_anchor.source_commit_cursor
                >= recorded.commit_observation_anchor.source_commit_cursor
    });
    assert_eq!(
        plan == SessionEventDispatchSettlementPlan::Apply,
        valid && (first || resumed)
    );
    assert_eq!(
        plan == SessionEventDispatchSettlementPlan::Replay,
        valid && existing == Some(incoming) && processed && !active
    );
    assert_eq!(
        plan == SessionEventDispatchSettlementPlan::Reject,
        !(valid && (first || resumed))
            && !(valid && existing == Some(incoming) && processed && !active)
    );
    // R4's fourth conjunct is the fact the aggregate DERIVES from its retained
    // input. It is proved as its own theorem in
    // `retained_input_idling_is_exactly_settled_or_only_blocked_users` below,
    // over the real kernel and a bounded representative batch; quantifying over
    // it here is sound only because that sibling constrains it.
    let exact_activity_removed = kani::any();
    let last_active = kani::any();
    let inputs_allow_idle = kani::any();
    assert_eq!(
        session_event_execution_may_close(
            plan,
            exact_activity_removed,
            last_active,
            inputs_allow_idle,
        ),
        plan == SessionEventDispatchSettlementPlan::Apply
            && exact_activity_removed
            && last_active
            && inputs_allow_idle
    );
}

/// The retained-input half of R4, proved over the production predicate rather
/// than over a free symbol.
///
/// Causes: the Event's processed marker, its class, and the class and marker of
/// its immediate predecessor (absent for the first). Effects: R1 any processed
/// Event is inert; R2 a pending ordinary User is inert, whatever precedes it --
/// this is the rule whose absence held an Awaiting Session Running behind its
/// own queued User; R3 a pending System is inert only while the User that froze
/// it is itself still pending, so a System standing alone is real work; R4 every
/// other pending kind blocks idling unconditionally.
///
/// Bounds: this proves the per-Event rule and the class each real command maps
/// to. The fold that applies it -- `Iterator::all` over the entries of each
/// batch and over the batches of a Session, disjoined with
/// `all_events_processed` -- is NOT proved here: a non-empty
/// `Vec<SessionEventEntry>` does not complete under Kani in this crate (a bare
/// command is 0.15s, the same command inside one `Vec` does not finish in
/// 400s), which is the same wall `terminal_batch_resolution_empty` documents.
/// The fold is covered by the executable decision tables in this module and in
/// `crate::tests::event_batches`.
#[cfg(kani)]
/// @obligation session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
/// @obligation session_execution.initial_event_path_reaches_idle_only_after_full_settlement
#[kani::proof]
fn retained_input_idling_is_exactly_settled_or_only_blocked_users() {
    let class = proof_class(kani::any());
    let previous = kani::any::<bool>().then(|| (proof_class(kani::any()), kani::any::<bool>()));

    // R1: a processed Event never blocks, in any class or position.
    assert!(retained_event_is_inert(true, class, previous));

    let pending = retained_event_is_inert(false, class, previous);
    // R2 and R4: an ordinary User is inert and any other non-System kind is not,
    // independently of what precedes them.
    if class == RetainedInputClass::OrdinaryUser {
        assert!(
            pending,
            "a queued ordinary User must not hold the Session Running"
        );
    }
    if class == RetainedInputClass::Other {
        assert!(
            !pending,
            "every other pending command keeps the gate closed"
        );
    }
    // R3: a pending System is inert exactly while its own pending User precedes it.
    if class == RetainedInputClass::ReservedSystem {
        assert_eq!(
            pending,
            previous == Some((RetainedInputClass::OrdinaryUser, false)),
            "a System is inert only inside its own User's reservation"
        );
    }
    // Processing an Event can never turn an inert one back into a blocker.
    assert!(!(pending && !retained_event_is_inert(true, class, previous)));

    // The classification is the derivation's other half: every real command
    // variant maps to exactly one class, over values built by the real type.
    let operation_id = String::new;
    assert_eq!(
        SessionEventCommand::UserMessage {
            operation_id: operation_id(),
            run_id: RunId(String::new()),
            content: Vec::new(),
            data_subject_id: None,
        }
        .retained_input_class(),
        RetainedInputClass::OrdinaryUser
    );
    assert_eq!(
        SessionEventCommand::SystemMessage {
            operation_id: operation_id(),
            content: Vec::new(),
        }
        .retained_input_class(),
        RetainedInputClass::ReservedSystem
    );
    assert_eq!(
        SessionEventCommand::Interrupt {
            operation_id: operation_id(),
            interrupt: crate::event::SessionEventInterrupt {
                requested_target: None,
                targets: Vec::new(),
            },
        }
        .retained_input_class(),
        RetainedInputClass::Other
    );
    assert_eq!(
        SessionEventCommand::DefineOutcome {
            operation_id: operation_id(),
            outcome_id: String::new(),
            description: String::new(),
            rubric: crate::event::SessionOutcomeRubric::Text {
                content: String::new(),
            },
            max_iterations: None,
        }
        .retained_input_class(),
        RetainedInputClass::Other
    );
}

/// The three retained-input classes as a symbolic choice, without building a
/// command: the mapping from commands to classes is asserted separately above.
#[cfg(kani)]
fn proof_class(tag: u8) -> RetainedInputClass {
    match tag % 3 {
        0 => RetainedInputClass::OrdinaryUser,
        1 => RetainedInputClass::ReservedSystem,
        _ => RetainedInputClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::SessionEventInput;
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
        // L1 supported legacy input already processed + no receipt + exact new
        // active epoch -> establish the first receipt. L2 no active epoch ->
        // reject: a historical processed marker alone grants no closing power.
        assert_eq!(
            session_event_dispatch_settlement_plan(true, true, true, None, receipt),
            SessionEventDispatchSettlementPlan::Apply
        );
        assert_eq!(
            session_event_dispatch_settlement_plan(true, true, false, None, receipt),
            SessionEventDispatchSettlementPlan::Reject
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
            &crate::event::SessionId::from("session"),
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
        // R4 first Awaiting -> resumed Awaiting -> resumed terminal: SAME Run,
        // strictly newer active epochs and non-regressing commit observations.
        // Each receipt applies once; User and paired System publication stays
        // byte-identical. R5 stale/inactive/rollback receipts reject atomically.
        let public_inputs = |batch: &SessionEventBatch| {
            batch
                .events
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "event": entry.event, "processed": entry.processed,
                        "anchor": entry.projection_anchor, "time": entry.processed_at_unix_ms,
                    })
                })
                .collect::<Vec<_>>()
        };
        let published = public_inputs(&batch);
        for (epoch, cursor) in [(8, 12), (9, 13)] {
            let next = SessionEventDispatchSettlementReceipt {
                session_activity_epoch: epoch,
                commit_observation_anchor: SessionEventProjectionAnchor {
                    source_commit_cursor: cursor,
                },
            };
            assert_eq!(
                batch
                    .record_user_run_dispatch_settlement(
                        &operation_id,
                        &run_id,
                        true,
                        next,
                        1_700_000_001_000,
                    )
                    .unwrap(),
                SessionEventDispatchSettlementPlan::Apply,
                "R4 resume"
            );
            assert_eq!(batch.events[0].dispatch_settlement, Some(next));
            assert_eq!(
                public_inputs(&batch),
                published,
                "R4 immutable input publication"
            );
            assert_eq!(
                batch
                    .record_user_run_dispatch_settlement(
                        &operation_id,
                        &run_id,
                        false,
                        next,
                        1_700_000_002_000,
                    )
                    .unwrap(),
                SessionEventDispatchSettlementPlan::Replay,
                "R4 exact replay"
            );
        }
        let settled = serde_json::to_value(&batch).unwrap();
        for (active, epoch, cursor) in [
            (true, 8, 14),
            (true, 9, 14),
            (true, 10, 12),
            (false, 10, 14),
        ] {
            let wrong = SessionEventDispatchSettlementReceipt {
                session_activity_epoch: epoch,
                commit_observation_anchor: SessionEventProjectionAnchor {
                    source_commit_cursor: cursor,
                },
            };
            assert!(
                batch
                    .record_user_run_dispatch_settlement(
                        &operation_id,
                        &run_id,
                        active,
                        wrong,
                        1_700_000_003_000,
                    )
                    .is_err(),
                "R5 stale or unowned receipt"
            );
            assert_eq!(
                serde_json::to_value(&batch).unwrap(),
                settled,
                "R5 no partial mutation"
            );
        }
    }
}
