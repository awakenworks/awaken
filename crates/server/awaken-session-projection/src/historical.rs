// @formal entrypoint session.runtime_projection_historical_pending_precedes_terminal_projection
//! Historical Awaiting targets recovered from the Thread audit prefix.
//!
//! A pause whose ticket was answered before any reader observed it leaves no
//! live ticket behind; its committed `RunStateChanged` audit row still names
//! the await target. Consumers: the timeline's pause and stop-reason facts and
//! the canonical order of the pause's answerable event.

use std::collections::HashMap;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent, RunLifecycleEventKind};
use awaken_session_application_contract::Pending;

use crate::ProjectionError;

/// The answerable target of each Awaiting lifecycle commit whose Thread audit
/// prefix retained it, keyed by the exact audit sequence that produced that
/// lifecycle cursor. An absent earlier audit cannot borrow a later target.
pub fn historical_pending_by_lifecycle<'a>(
    lifecycle_events: &[RunLifecycleEvent],
    snapshots: impl Fn(&str) -> Option<&'a RunRecoverySnapshot>,
) -> Result<HashMap<RunLifecycleCursor, Pending>, ProjectionError> {
    Ok(audited_await_targets(lifecycle_events, snapshots)?
        .into_iter()
        .filter_map(|(cursor, target)| Some((cursor, Pending::from_await_target(&target)?)))
        .collect())
}

/// The await target every Awaiting lifecycle commit's own audit recorded,
/// answerable by a client or not (a scheduled action, a delegation, a pause),
/// keyed by that lifecycle cursor. A commit absent here left no target: its
/// pause cannot be classified until a later commit supersedes it.
pub fn audited_await_targets<'a>(
    lifecycle_events: &[RunLifecycleEvent],
    snapshots: impl Fn(&str) -> Option<&'a RunRecoverySnapshot>,
) -> Result<
    HashMap<RunLifecycleCursor, awaken_agent_contract::agent::awaiting::AwaitTarget>,
    ProjectionError,
> {
    let mut targets = HashMap::new();
    for lifecycle in lifecycle_events
        .iter()
        .filter(|event| event.kind == RunLifecycleEventKind::Awaiting)
    {
        let Some(snapshot) = snapshots(&lifecycle.thread_id.0) else {
            continue;
        };
        if snapshot.thread_id != lifecycle.thread_id {
            return Err(ProjectionError::corrupt(
                "Awaiting audit snapshot belongs to another Thread",
            ));
        }
        let mut matching = snapshot.events.iter().filter(|audit| {
            audit.sequence == lifecycle.cursor.0 && audit.run_id == lifecycle.run_id
        });
        let Some(audit) = matching.next() else {
            continue;
        };
        if matching.any(|later| later.kind != audit.kind || later.payload != audit.payload) {
            return Err(ProjectionError::corrupt(
                "conflicting committed Awaiting audits at one Thread, Run and sequence",
            ));
        }
        let state = serde_json::from_value::<awaken_agent_contract::agent::run::RunState>(
            audit.payload.get("state").cloned().unwrap_or_default(),
        )
        .map_err(|_| ProjectionError::corrupt("committed Awaiting state is malformed"))?;
        if audit.kind != awaken_agent_contract::audit::kind::Kind::RunStateChanged
            || state != awaken_agent_contract::agent::run::RunState::Awaiting
        {
            return Err(ProjectionError::corrupt(
                "Awaiting lifecycle has no matching committed state audit",
            ));
        }
        if let Some(value) = audit
            .payload
            .get("await_target")
            .filter(|value| !value.is_null())
        {
            let target = serde_json::from_value::<
                awaken_agent_contract::agent::awaiting::AwaitTarget,
            >(value.clone())
            .map_err(|_| ProjectionError::corrupt("committed Awaiting target is malformed"))?;
            targets.insert(lifecycle.cursor, target);
        }
    }
    Ok(targets)
}

/// Root and child use the same ordered, source-scoped history selection.
/// A provider-local call id alone cannot transfer an earlier permission to
/// another Thread, Run or input. With committed Run coordinates the call id
/// binds the internal ticket name to the model-facing alias; legacy messages
/// without those coordinates still require an exact tool name. Keep every
/// matching boundary: one assistant response can contain several separately
/// approved calls.
#[must_use]
pub fn historical_message_boundaries<'a>(
    message: &Message,
    history: &'a HashMap<RunLifecycleCursor, Pending>,
    lifecycle: &'a [RunLifecycleEvent],
    thread_id: &str,
    source_run_id: Option<&str>,
) -> Vec<(Pending, &'a awaken_agent_contract::agent::run::Id)> {
    lifecycle
        .iter()
        .filter(|event| {
            event.thread_id.0 == thread_id && source_run_id.is_none_or(|run| event.run_id.0 == run)
        })
        .filter_map(|event| {
            let pending = history.get(&event.cursor)?;
            let public_name =
                historical_message_await_name(message, pending, &event.run_id, source_run_id)?;
            let mut public_pending = pending.clone();
            public_pending.name = public_name.to_string();
            Some((public_pending, &event.run_id))
        })
        .collect()
}

/// The one source-scoped occurrence rule for a committed ToolUse Message and
/// its Awaiting audit. A legacy Message with no proven Run coordinate needs an
/// exact name as well as call id and input; a signed Run coordinate can retain
/// the Message's public alias. User/System content never grants a tool call.
/// Consumer: historical transcript selection and the Managed adapter's
/// as-of-Awaiting synthetic identity decision.
#[must_use]
pub fn historical_message_await_name<'a>(
    message: &'a Message,
    pending: &Pending,
    awaiting_run: &awaken_agent_contract::RunId,
    source_run_id: Option<&str>,
) -> Option<&'a str> {
    if message.role != Role::Assistant
        || source_run_id.is_some_and(|run| run != awaiting_run.0.as_str())
    {
        return None;
    }
    message.content.iter().find_map(|block| {
        let ContentBlock::ToolUse { id, name, input } = block else {
            return None;
        };
        (id == &pending.tool_use_id
            && input == &pending.input
            && (name == &pending.name || source_run_id == Some(awaiting_run.0.as_str())))
        .then_some(name.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn awaiting(source: u64) -> RunLifecycleEvent {
        RunLifecycleEvent {
            cursor: awaken_agent_contract::encode_run_lifecycle_cursor(source, 0).unwrap(),
            source_commit_cursor: source,
            thread_id: awaken_agent_contract::ThreadId("thread".into()),
            run_id: awaken_agent_contract::RunId("run".into()),
            kind: RunLifecycleEventKind::Awaiting,
            state: awaken_agent_contract::agent::run::RunState::Awaiting,
            await_reason: None,
        }
    }

    fn audit(source: u64, call: Option<&str>) -> awaken_agent_contract::audit::record::Record {
        let target =
            call.map(
                |call_id| awaken_agent_contract::agent::awaiting::AwaitTarget::RemoteInput {
                    reason: awaken_agent_contract::agent::awaiting::RemoteInputReason::UserInput,
                    call_id: call_id.into(),
                },
            );
        let mut payload =
            serde_json::json!({"state": awaken_agent_contract::agent::run::RunState::Awaiting});
        if let Some(target) = target {
            payload["await_target"] = serde_json::to_value(target).unwrap();
        }
        awaken_agent_contract::audit::record::Record {
            sequence: awaiting(source).cursor.0,
            run_id: awaken_agent_contract::RunId("run".into()),
            kind: awaken_agent_contract::audit::kind::Kind::RunStateChanged,
            payload,
        }
    }

    fn audit_prefix(
        events: Vec<awaken_agent_contract::audit::record::Record>,
    ) -> RunRecoverySnapshot {
        let run = awaken_agent_contract::RunId("run".into());
        let thread = awaken_agent_contract::ThreadId("thread".into());
        RunRecoverySnapshot {
            thread_id: thread.clone(),
            claimed_run_id: run.clone(),
            runs: vec![awaken_agent_contract::agent::run::Record {
                id: run.clone(),
                thread_id: thread,
                state: awaken_agent_contract::agent::run::RunState::Awaiting,
            }],
            latest_run_id: Some(run),
            messages: vec![],
            message_commit_cursors: vec![],
            commit_timestamps: Default::default(),
            state: vec![],
            state_commit_cursors: vec![],
            events,
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 4,
            next_commit_ordinal: 0,
        }
    }

    #[test]
    fn awaiting_audit_join_is_exact_and_never_fills_a_historical_hole() {
        // Cause/effect graph: C1 Thread+Run+canonical audit sequence agree;
        // C2 a historical target/audit is missing; C3 later target exists;
        // C4 duplicate identity is equal/conflicting; C5 source scope, audit
        // kind/state, or target codec is damaged. E1 only the exact commit owns
        // its target; E2 a hole stays
        // unknown; E3 equal duplicates are idempotent; E4 conflicts reject the
        // entire pure projection without exposing its payload (the publication
        // stutter table owns effects).
        // Rules J1=C1=>E1; J2=C2+C3=>E2+later E1, never ordinal substitution;
        // J3=C4(equal)=>E3; J4=C4(conflict)|C5=>typed Corrupt; J5 source row
        // order does not change J1/J2. All rules exercise the production join.
        let lifecycle = [awaiting(2), awaiting(4)];
        for (rule, events, expected) in [
            (
                "J1/J5",
                vec![audit(4, Some("later")), audit(2, Some("first"))],
                vec![(2, "first"), (4, "later")],
            ),
            (
                "J2 absent audit",
                vec![audit(4, Some("later"))],
                vec![(4, "later")],
            ),
            (
                "J2 absent target",
                vec![audit(2, None), audit(4, Some("later"))],
                vec![(4, "later")],
            ),
            (
                "J2 null target",
                vec![
                    {
                        let mut row = audit(2, None);
                        row.payload["await_target"] = serde_json::Value::Null;
                        row
                    },
                    audit(4, Some("later")),
                ],
                vec![(4, "later")],
            ),
            (
                "J3",
                vec![
                    audit(2, Some("first")),
                    audit(2, Some("first")),
                    audit(4, Some("later")),
                ],
                vec![(2, "first"), (4, "later")],
            ),
        ] {
            let snapshot = audit_prefix(events);
            let actual = audited_await_targets(&lifecycle, |_| Some(&snapshot)).unwrap();
            assert_eq!(actual.len(), expected.len(), "{rule}");
            for (source, call_id) in expected {
                assert_eq!(
                    actual[&awaiting(source).cursor],
                    awaken_agent_contract::agent::awaiting::AwaitTarget::RemoteInput {
                        reason:
                            awaken_agent_contract::agent::awaiting::RemoteInputReason::UserInput,
                        call_id: call_id.into(),
                    },
                    "{rule}"
                );
            }
        }
        let mut foreign_run = audit(2, Some("foreign"));
        foreign_run.run_id = awaken_agent_contract::RunId("other".into());
        let snapshot = audit_prefix(vec![foreign_run]);
        assert!(
            audited_await_targets(&lifecycle, |_| Some(&snapshot))
                .unwrap()
                .is_empty(),
            "J1 foreign Run cannot grant a target"
        );
        let mut foreign_thread = audit_prefix(vec![audit(2, Some("foreign"))]);
        foreign_thread.thread_id = awaken_agent_contract::ThreadId("other".into());
        let conflict = audit_prefix(vec![audit(2, Some("first")), audit(2, Some("changed"))]);
        let mut invalid = audit_prefix(vec![audit(2, Some("first"))]);
        invalid.events[0].payload["await_target"] = serde_json::json!("private-input-marker");
        let mut wrong_kind = audit_prefix(vec![audit(2, Some("first"))]);
        wrong_kind.events[0].kind = awaken_agent_contract::audit::kind::Kind::ModelRequestCompleted;
        let mut wrong_state = audit_prefix(vec![audit(2, Some("first"))]);
        wrong_state.events[0].payload["state"] =
            serde_json::to_value(awaken_agent_contract::agent::run::RunState::Running).unwrap();
        for snapshot in [foreign_thread, conflict, invalid, wrong_kind, wrong_state] {
            let error = audited_await_targets(&lifecycle, |_| Some(&snapshot)).unwrap_err();
            assert!(
                matches!(error, ProjectionError::Corrupt(_)),
                "J4 typed corruption"
            );
            assert!(
                !error.to_string().contains("private-input-marker"),
                "J4 private source value stays private"
            );
        }
    }

    #[test]
    fn permission_history_does_not_cross_message_ownership() {
        // Causes: Thread, source Run, raw call id, public alias and input can each
        // differ. Effects: H1 all equal selects the committed boundary; H2 any
        // ownership/payload mismatch selects none. Proven same-Run aliases
        // retain the public name (H3); absent coordinates require name equality
        // (H4), including a negative legacy-alias row. A User ToolUse block
        // cannot grant an answerable Agent call (H5).
        // The table is shared for root/child consumers; a same-name call alone
        // is never permission evidence. No ordering comes from HashMap keys.
        let pending = Pending {
            tool_use_id: "call".into(),
            name: "write".into(),
            input: serde_json::json!({"path":"one"}),
            client_executed: false,
        };
        let history = HashMap::from([(RunLifecycleCursor(1), pending.clone())]);
        let lifecycle = vec![RunLifecycleEvent {
            cursor: RunLifecycleCursor(1),
            source_commit_cursor: 1,
            thread_id: awaken_agent_contract::ThreadId("thread".into()),
            run_id: awaken_agent_contract::agent::run::Id("run".into()),
            kind: RunLifecycleEventKind::Awaiting,
            state: awaken_agent_contract::agent::run::RunState::Awaiting,
            await_reason: None,
        }];
        for changed in [
            None,
            Some("thread"),
            Some("run"),
            Some("id"),
            Some("name"),
            Some("input"),
            Some("no-coordinate"),
            Some("legacy-alias"),
            Some("user-role"),
        ] {
            let mut call = pending.clone();
            if changed == Some("id") {
                call.tool_use_id = "other".into();
            }
            if matches!(changed, Some("name" | "legacy-alias")) {
                call.name = "read".into();
            }
            if changed == Some("input") {
                call.input = serde_json::json!({"path":"other"});
            }
            let message = Message::new(
                awaken_agent_contract::agent::message::Id("message".into()),
                if changed == Some("user-role") {
                    awaken_agent_contract::agent::message::Role::User
                } else {
                    awaken_agent_contract::agent::message::Role::Assistant
                },
                vec![ContentBlock::tool_use(
                    call.tool_use_id,
                    call.name,
                    call.input,
                )],
            );
            let thread = if changed == Some("thread") {
                "other"
            } else {
                "thread"
            };
            let run = match changed {
                Some("run") => Some("other"),
                Some("no-coordinate" | "legacy-alias") => None,
                _ => Some("run"),
            };
            let selected =
                historical_message_boundaries(&message, &history, &lifecycle, thread, run);
            assert_eq!(
                selected.len(),
                usize::from(changed.is_none() || matches!(changed, Some("no-coordinate" | "name"))),
                "H1-H3 {changed:?}"
            );
        }
    }
}
