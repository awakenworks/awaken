//! Equality of two committed Thread prefixes for projection fencing.

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;

/// Compare one Thread's committed recovery prefix while ignoring the
/// backend-wide diagnostic high-water, which unrelated Threads may advance.
///
/// Consumer: the protocol projector's source fence, which re-reads the prefixes
/// after building a candidate and discards the candidate if any moved.
#[must_use]
pub fn same_thread_projection_prefix(
    left: Option<&RunRecoverySnapshot>,
    right: Option<&RunRecoverySnapshot>,
) -> bool {
    let (left, right) = match (left, right) {
        (None, None) => return true,
        (Some(left), Some(right)) => (left, right),
        _ => return false,
    };
    // Exhaustive destructuring makes a new recovery field require an explicit
    // comparison decision. Only the backend-wide diagnostic cursor is ignored;
    // equal source versions alone never establish equal retained facts.
    let RunRecoverySnapshot {
        thread_id,
        claimed_run_id,
        runs,
        latest_run_id,
        messages,
        message_commit_cursors,
        commit_timestamps,
        state,
        state_commit_cursors,
        events,
        resume_tickets,
        thread_version,
        store_cursor: _,
        next_commit_ordinal,
    } = left;
    thread_id == &right.thread_id
        && claimed_run_id == &right.claimed_run_id
        && runs == &right.runs
        && latest_run_id == &right.latest_run_id
        && messages == &right.messages
        && message_commit_cursors == &right.message_commit_cursors
        && commit_timestamps == &right.commit_timestamps
        && state == &right.state
        && state_commit_cursors == &right.state_commit_cursors
        && events == &right.events
        && resume_tickets == &right.resume_tickets
        && thread_version == &right.thread_version
        && next_commit_ordinal == &right.next_commit_ordinal
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::{
        awaiting::{AwaitTarget, PauseReason, ResumeTicket},
        message::{Id as MessageId, Message, Role},
        run::{Id as RunId, Record, RunState},
        state::{Command, MergePolicy, Scope},
        thread::Id as ThreadId,
    };
    use awaken_agent_contract::thread::read::recovery::RunResumeTicket;

    #[test]
    fn borrowed_prefix_comparison_covers_every_serialized_recovery_field() {
        // C1 either prefix absent; C2 any retained field differs; C3 only the
        // backend-wide store cursor advances. R1 both absent => same; R2 one
        // absent => changed; R3 each C2 dimension => changed, even at equal
        // versions; R4 C3 => same. Check the field inventory against Serialize
        // so adding a field cannot silently escape this decision table.
        let thread = ThreadId("thread".into());
        let run = RunId("run".into());
        let original = RunRecoverySnapshot {
            thread_id: thread.clone(),
            claimed_run_id: run.clone(),
            runs: vec![],
            latest_run_id: None,
            messages: vec![],
            message_commit_cursors: vec![],
            commit_timestamps: [(1, 1)].into(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 0,
        };
        let fields = serde_json::json!({
            "thread_id":"other", "claimed_run_id":"other",
            "runs":[Record { id: run.clone(), thread_id: thread.clone(), state: RunState::Running }],
            "latest_run_id":"run",
            "messages":[Message::text(MessageId("message".into()), Role::User, "value")],
            "message_commit_cursors":[1], "commit_timestamps":{"1":2},
            "state":[Command::set(Scope::Run, MergePolicy::Disjoint, "key", serde_json::json!(1))],
            "state_commit_cursors":[1],
            "events":[awaken_agent_contract::audit::record::Record {
                sequence: 1, run_id: run.clone(),
                kind: awaken_agent_contract::audit::kind::Kind::PermissionDecided,
                payload: serde_json::json!({}),
            }],
            "resume_tickets":[RunResumeTicket { run_id: run.clone(), ticket: ResumeTicket::new(
                "correlation", run.clone(), thread.clone(), "snapshot", "catalog",
                AwaitTarget::Pause(PauseReason::Manual),
            )}],
            "thread_version":2, "store_cursor":2, "next_commit_ordinal":1,
        });
        let baseline = serde_json::to_value(&original).unwrap();
        assert_eq!(
            baseline
                .as_object()
                .unwrap()
                .keys()
                .collect::<std::collections::BTreeSet<_>>(),
            fields
                .as_object()
                .unwrap()
                .keys()
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert!(same_thread_projection_prefix(None, None));
        assert!(!same_thread_projection_prefix(Some(&original), None));
        assert!(!same_thread_projection_prefix(None, Some(&original)));
        assert!(same_thread_projection_prefix(
            Some(&original),
            Some(&original)
        ));
        for (field, value) in fields.as_object().unwrap() {
            let mut changed = baseline.clone();
            changed[field] = value.clone();
            let changed: RunRecoverySnapshot = serde_json::from_value(changed).unwrap();
            let expected = field == "store_cursor";
            assert_eq!(
                same_thread_projection_prefix(Some(&original), Some(&changed)),
                expected,
                "{field}"
            );
            assert_eq!(
                same_thread_projection_prefix(Some(&changed), Some(&original)),
                expected,
                "{field} reverse"
            );
        }
    }
}
