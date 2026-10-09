// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Pure attribution of recorded gate decisions to existing committed ToolUse occurrences.
// @formal reason: This reads one recovery prefix, grants no authority and introduces no audit or store.
//! Recorded gate decisions keyed by their Thread, committed source and call.

use crate::{OrderIndex, ProjectionError};
use awaken_agent_contract::audit::{
    kind::Kind,
    permission::{PermissionDecider, PermissionPolicy, PermissionReasonCode},
};
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, decode_run_lifecycle_cursor,
};
use awaken_runtime_contract::permission::GateAuditDecision;
use std::collections::HashMap;

/// One recorded decision; the protocol renders this existing SDK vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedToolPermission {
    pub decision: GateAuditDecision,
    pub policy: Option<PermissionPolicy>,
    pub reason_code: Option<PermissionReasonCode>,
}

/// Call ids can repeat between Threads, Runs and steps. The existing public
/// tool identity uses the owning Message id (or the exact pending Run id), so
/// this index uses that same source rather than a second occurrence counter.
pub type RecordedToolPermissions = HashMap<(String, String, String), RecordedToolPermission>;

pub fn recorded_tool_permissions(
    index: &OrderIndex<'_>,
) -> Result<RecordedToolPermissions, ProjectionError> {
    let mut decisions = HashMap::new();
    for snapshot in index.permission_snapshots() {
        for event in &snapshot.events {
            if event.kind != Kind::PermissionDecided {
                continue;
            }
            let Some(call_id) = event.payload.get("call_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let decision = match event.payload.get("decision").and_then(|v| v.as_str()) {
                Some("allow") => GateAuditDecision::Allow,
                Some("ask") => GateAuditDecision::Ask,
                Some("deny") => GateAuditDecision::Deny,
                _ => continue,
            };
            let source_commit = decode_run_lifecycle_cursor(RunLifecycleCursor(event.sequence)).0;
            if source_commit == 0 || source_commit > snapshot.store_cursor {
                return Err(ProjectionError::corrupt(
                    "permission audit has no exact commit in its recovery prefix",
                ));
            }
            let mut source = None;
            let mut latest = None;
            let mut unqualified_legacy = None;
            for &message_index in index.permission_call_sources(&snapshot.thread_id.0, call_id) {
                let message = &snapshot.messages[message_index];
                if message
                    .id
                    .assistant_coordinates()
                    .is_some_and(|(run, _, _)| run != event.run_id.0)
                {
                    continue;
                }
                let cursor = snapshot
                    .message_commit_cursors
                    .get(message_index)
                    .copied()
                    .filter(|cursor| *cursor > 0)
                    .ok_or_else(|| {
                        ProjectionError::corrupt(
                            "permission ToolUse occurrence has no committed Message coordinate",
                        )
                    })?;
                if cursor > source_commit {
                    continue;
                }
                if cursor < source_commit && message.id.assistant_coordinates().is_none() {
                    unqualified_legacy =
                        Some(unqualified_legacy.map_or(cursor, |current: u64| current.max(cursor)));
                    continue;
                }
                match latest {
                    Some(current) if cursor < current => {}
                    Some(current) if cursor == current => {
                        return Err(ProjectionError::corrupt(
                            "permission audit has ambiguous ToolUse occurrences at one commit",
                        ));
                    }
                    _ => {
                        latest = Some(cursor);
                        source = Some(message.id.0.clone());
                    }
                }
            }
            if unqualified_legacy
                .is_some_and(|cursor| latest.is_none_or(|selected| cursor >= selected))
            {
                return Err(ProjectionError::corrupt(
                    "historical permission Message has no exact Run coordinate",
                ));
            }
            let source = source.unwrap_or_else(|| event.run_id.0.clone());
            let policy_decided = match event.payload.get("decided_by") {
                None => true,
                Some(label) => matches!(
                    serde_json::from_value(label.clone()),
                    Ok(PermissionDecider::Policy)
                ),
            };
            let policy = event
                .payload
                .get("policy")
                .filter(|_| policy_decided)
                .and_then(|value| serde_json::from_value(value.clone()).ok());
            let reason_code = event
                .payload
                .get("reason_code")
                .and_then(|value| serde_json::from_value(value.clone()).ok());
            let key = (snapshot.thread_id.0.clone(), source, call_id.to_string());
            let recorded = RecordedToolPermission {
                decision,
                policy,
                reason_code,
            };
            match decisions.entry(key) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert((event, recorded));
                }
                std::collections::hash_map::Entry::Occupied(entry) if entry.get().0 == event => {}
                // The gate re-audits an occurrence it still holds: a batch
                // resumed past one confirmed call re-audits each sibling that
                // is still pending. The occurrence keeps the decision it was
                // first published with, so a re-audit cannot rewrite an issued
                // event and warm and cold readers agree. Two different
                // decisions at one audit position cannot be ordered.
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let first = entry.get().0;
                    if first.sequence == event.sequence && first != event {
                        return Err(ProjectionError::corrupt(
                            "multiple permission decisions claim one ToolUse occurrence",
                        ));
                    }
                    if event.sequence < first.sequence {
                        entry.insert((event, recorded));
                    }
                }
            }
        }
    }
    Ok(decisions
        .into_iter()
        .map(|(key, (_, value))| (key, value))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::{
        agent::{
            content::ContentBlock,
            message::{Id as MessageId, Message, Role},
            run::Id as RunId,
            thread::Id as ThreadId,
        },
        audit::record::Record,
        thread::read::{lifecycle::encode_run_lifecycle_cursor, recovery::RunRecoverySnapshot},
    };

    fn permissions<'a>(
        snapshots: impl Iterator<Item = &'a RunRecoverySnapshot>,
    ) -> Result<RecordedToolPermissions, ProjectionError> {
        let children = snapshots
            .map(|snapshot| (snapshot.thread_id.0.clone(), snapshot.clone()))
            .collect();
        let historical_pending = HashMap::new();
        recorded_tool_permissions(&OrderIndex::new(crate::OrderEvidence {
            session_id: "root",
            batches: &[],
            closed_intervals: &[],
            budget_reach: &[],
            parent_terminal: crate::ParentTerminalEvidence::default(),
            lifecycle: &[],
            root: None,
            children: &children,
            links: &[],
            outcomes: &[],
            historical_pending: &historical_pending,
        }))
    }

    fn snapshot(thread: &str) -> RunRecoverySnapshot {
        RunRecoverySnapshot {
            thread_id: ThreadId(thread.into()),
            claimed_run_id: RunId("run".into()),
            runs: vec![],
            latest_run_id: None,
            messages: vec![],
            message_commit_cursors: vec![],
            commit_timestamps: Default::default(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 0,
            store_cursor: 0,
            next_commit_ordinal: 0,
        }
    }
    fn append(snapshot: &mut RunRecoverySnapshot, run: &str, cursor: u64, decision: &str) {
        snapshot.messages.push(Message {
            id: MessageId::assistant(&RunId(run.into()), cursor as usize),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call".into(),
                name: "echo".into(),
                input: serde_json::json!({}),
            }],
        });
        snapshot.message_commit_cursors.push(cursor);
        snapshot.store_cursor = snapshot.store_cursor.max(cursor);
        snapshot.events.push(Record {
            sequence: encode_run_lifecycle_cursor(cursor, 0).unwrap().0,
            run_id: RunId(run.into()),
            kind: Kind::PermissionDecided,
            payload: serde_json::json!({"call_id":"call", "decision":decision}),
        });
    }
    #[test]
    fn permission_summary_is_keyed_by_thread_and_call_independent_of_read_order() {
        // C1 same call id in two Threads; C2 snapshot enumeration reversed.
        // R1 C1+C2 => each occurrence keeps its own decision and index equality.
        let mut root = snapshot("root");
        let mut child = snapshot("child");
        append(&mut root, "run", 1, "allow");
        append(&mut child, "run", 1, "deny");
        let forward = permissions([&root, &child].into_iter()).unwrap();
        let backward = permissions([&child, &root].into_iter()).unwrap();
        assert_eq!(forward, backward);
        assert_eq!(
            forward[&(
                "root".into(),
                MessageId::assistant(&RunId("run".into()), 1).0,
                "call".into()
            )]
                .decision,
            GateAuditDecision::Allow
        );
        assert_eq!(
            forward[&(
                "child".into(),
                MessageId::assistant(&RunId("run".into()), 1).0,
                "call".into()
            )]
                .decision,
            GateAuditDecision::Deny
        );
    }
    #[test]
    fn recorded_permission_occurrences_survive_run_and_step_reuse() {
        // C1 call id reused across Runs; C2 reused across steps of one Run;
        // C3 a future Message is already in the same read prefix. R1 C1/C2/C3
        // => each earlier audit binds its own latest preceding Message only.
        let mut evidence = snapshot("thread");
        append(&mut evidence, "run-a", 1, "allow");
        append(&mut evidence, "run-b", 2, "deny");
        append(&mut evidence, "run-b", 3, "ask");
        let decisions = permissions([&evidence].into_iter()).unwrap();
        for (run, step, decision) in [
            ("run-a", 1, GateAuditDecision::Allow),
            ("run-b", 2, GateAuditDecision::Deny),
            ("run-b", 3, GateAuditDecision::Ask),
        ] {
            let source = MessageId::assistant(&RunId(run.into()), step).0;
            assert_eq!(
                decisions[&("thread".into(), source, "call".into())].decision,
                decision
            );
        }
    }
    #[test]
    fn pending_run_cannot_inherit_a_previous_runs_permission() {
        // C1 one committed ToolUse belongs to run-a; C2 run-b records the same
        // raw call id without a Message. R1 C1+C2 => separate Message/Run keys;
        // the later pending audit can never overwrite the older decision.
        let mut evidence = snapshot("thread");
        append(&mut evidence, "run-a", 1, "deny");
        let mut pending = evidence.events[0].clone();
        pending.sequence = encode_run_lifecycle_cursor(2, 0).unwrap().0;
        pending.run_id = RunId("run-b".into());
        pending.payload["decision"] = serde_json::json!("ask");
        evidence.events.push(pending);
        evidence.store_cursor = 2;
        let decisions = permissions([&evidence].into_iter()).unwrap();
        assert_eq!(
            decisions[&(
                "thread".into(),
                MessageId::assistant(&RunId("run-a".into()), 1).0,
                "call".into()
            )]
                .decision,
            GateAuditDecision::Deny
        );
        assert_eq!(
            decisions[&("thread".into(), "run-b".into(), "call".into())].decision,
            GateAuditDecision::Ask
        );
    }

    #[test]
    fn permission_occurrence_requires_exact_coordinates_and_preserves_unknowns() {
        // R1 missing coordinate => refusal; R2 two matching Messages at one
        // commit => refusal; R3 unknown decision => absent, never allow; R4 no
        // ToolUse Message => exact synthetic Run identity, no invented Message.
        let mut evidence = snapshot("thread");
        append(&mut evidence, "run", 1, "deny");
        let mut missing = evidence.clone();
        missing.message_commit_cursors.clear();
        assert!(permissions([&missing].into_iter()).is_err());
        let mut ambiguous = evidence.clone();
        let mut second = evidence.messages[0].clone();
        second.id = MessageId::assistant(&RunId("run".into()), 2);
        ambiguous.messages.push(second);
        ambiguous.message_commit_cursors.push(1);
        assert!(permissions([&ambiguous].into_iter()).is_err());
        let mut unknown = missing;
        unknown.events[0].payload["decision"] = serde_json::json!("future");
        assert!(permissions([&unknown].into_iter()).unwrap().is_empty());
        let mut pending = evidence;
        pending.messages.clear();
        pending.message_commit_cursors.clear();
        let decisions = permissions([&pending].into_iter()).unwrap();
        assert_eq!(
            decisions[&("thread".into(), "run".into(), "call".into())].decision,
            GateAuditDecision::Deny
        );
    }

    #[test]
    fn permission_replay_is_idempotent_but_conflicting_audits_are_rejected() {
        // C1 byte-equal Message/audit replay; C2 distinct audits claim one
        // occurrence, at a later audit position (the gate re-audits a sibling
        // still pending after a resumed batch) or at the same one; C3 Message
        // role is User/System; C4 audit has no source commit or exceeds its
        // prefix. R1 C1 => one identical index entry; R2 C2 => the first
        // decision stands, while two at one position are refused; R3 C3 =>
        // no Message attribution; R4 C4 => refusal, never a synthetic
        // permission grant.
        let mut evidence = snapshot("thread");
        append(&mut evidence, "run", 1, "deny");
        let expected = permissions([&evidence].into_iter()).unwrap();
        let mut replay = evidence.clone();
        replay.messages.push(evidence.messages[0].clone());
        replay.message_commit_cursors.push(2);
        replay.events.push(evidence.events[0].clone());
        replay.store_cursor = 2;
        assert_eq!(permissions([&replay].into_iter()).unwrap(), expected, "R1");
        for decision in ["deny", "allow"] {
            let mut redecided = evidence.clone();
            let mut audit = redecided.events[0].clone();
            audit.sequence = encode_run_lifecycle_cursor(1, 1).unwrap().0;
            audit.payload["decision"] = serde_json::json!(decision);
            redecided.events.push(audit.clone());
            assert_eq!(
                permissions([&redecided].into_iter()).unwrap(),
                expected,
                "R2 a later audit of one occurrence keeps its first decision"
            );
            redecided.events.reverse();
            assert_eq!(
                permissions([&redecided].into_iter()).unwrap(),
                expected,
                "R2 independent of audit read order"
            );
            let mut same_position = evidence.clone();
            audit.sequence = same_position.events[0].sequence;
            same_position.events.push(audit);
            // An equal row at the same position is the R1 replay.
            assert_eq!(
                permissions([&same_position].into_iter()).is_err(),
                decision != "deny",
                "R2 two decisions at one audit position"
            );
        }
        for role in [Role::User, Role::System] {
            let mut spoofed = evidence.clone();
            spoofed.messages[0].role = role;
            let index = permissions([&spoofed].into_iter()).unwrap();
            assert!(
                index.contains_key(&("thread".into(), "run".into(), "call".into())),
                "R3"
            );
        }
        for cursor in [0, 2] {
            let mut invalid = evidence.clone();
            invalid.events[0].sequence = encode_run_lifecycle_cursor(cursor, 0).unwrap().0;
            assert!(permissions([&invalid].into_iter()).is_err(), "R4");
        }
    }

    #[test]
    fn later_batch_audit_uses_exact_run_and_legacy_cannot_shadow_a_new_occurrence() {
        // C1 native sequential gates can commit after their batch Message;
        // C2 an older legacy Message reuses the call id but has no Run proof;
        // C3 a later exact occurrence exists. R1 C1 with canonical Run proof
        // => the preceding batch source; R2 C2 alone => refusal; R3 C2+C3 =>
        // the newer exact source, with no permission transferred to legacy.
        let mut evidence = snapshot("thread");
        append(&mut evidence, "run", 1, "deny");
        evidence.events[0].sequence = encode_run_lifecycle_cursor(2, 0).unwrap().0;
        evidence.store_cursor = 2;
        assert_eq!(permissions([&evidence].into_iter()).unwrap().len(), 1, "R1");
        evidence.messages[0].id = MessageId("legacy".into());
        assert!(permissions([&evidence].into_iter()).is_err(), "R2");
        evidence.events.clear();
        append(&mut evidence, "run", 3, "ask");
        let index = permissions([&evidence].into_iter()).unwrap();
        assert_eq!(index.len(), 1, "R3");
        assert_eq!(
            index[&(
                "thread".into(),
                MessageId::assistant(&RunId("run".into()), 3).0,
                "call".into()
            )]
                .decision,
            GateAuditDecision::Ask,
            "R3"
        );
    }
}
