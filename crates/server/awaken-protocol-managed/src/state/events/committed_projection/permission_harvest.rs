// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Reads the gate's recorded PermissionDecided audit rows so the Managed projection
// @formal reason: reports the recorded decision instead of inferring `allow` from a result; moved
// @formal reason: unchanged out of the candidate builder. It decides nothing and widens nothing: an
// @formal reason: unknown label is skipped. Covered by the recorded-permission projection tests.
//! Recorded permission decisions harvested from committed audit trails.

use super::*;

/// Collect the gate's recorded decision for every call named in these runs'
/// committed audit trails, with the policy and grounds recorded beside it.
/// A Runtime call id is unique only within its Thread, so each decision is
/// keyed by `(thread_id, call_id)`; the summary never depends on the order the
/// Thread snapshots are read in.
///
/// The audit kind is the one place a permission decision is written down, so
/// reading it here keeps the projection reporting a recorded answer rather than
/// inferring one. An unrecognized decision label is skipped: a value this
/// version does not understand must not be flattened into `allow`. The decider,
/// policy and reason code are additive fields: a row written before they
/// existed has none, and a policy label outside the closed vocabulary reads as
/// none, so the decision is still reported while the `evaluation` it cannot
/// name is absent. A row whose `decided_by` is not `policy` (a per-Run
/// narrowing, a plugin gate, or a label this version does not know) projects no
/// `evaluation`: the SDK object only has policy vocabulary.
/// The reason code is an open registry and is carried as recorded, including a
/// code this version has no name for.
pub(super) fn harvest_permission_decisions<'a>(
    snapshots: impl Iterator<
        Item = &'a awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    >,
) -> std::collections::HashMap<(String, String), RecordedPermission> {
    let mut decisions = std::collections::HashMap::new();
    for snapshot in snapshots {
        for event in &snapshot.events {
            if event.kind != awaken_agent_contract::audit::kind::Kind::PermissionDecided {
                continue;
            }
            let Some(call_id) = event.payload.get("call_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let decision = match event.payload.get("decision").and_then(|v| v.as_str()) {
                Some("allow") => EvaluatedPermission::Allow,
                Some("ask") => EvaluatedPermission::Ask,
                Some("deny") => EvaluatedPermission::Deny,
                _ => continue,
            };
            // Only a policy decider has a policy to project. A row written
            // before `decided_by` existed names none and keeps its policy; a
            // per-Run narrowing, a plugin gate or a decider label this version
            // does not know has no SDK `evaluation` to report.
            let policy_decided = match event.payload.get("decided_by") {
                None => true,
                Some(label) => matches!(
                    serde_json::from_value(label.clone()),
                    Ok(awaken_agent_contract::audit::permission::PermissionDecider::Policy)
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
            decisions.insert(
                (snapshot.thread_id.0.clone(), call_id.to_string()),
                RecordedPermission::from_audit(decision, policy, reason_code),
            );
        }
    }
    decisions
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;

    fn decided(thread: &str, call_id: &str, decision: &str) -> RunRecoverySnapshot {
        RunRecoverySnapshot {
            commit_timestamps: Default::default(),
            thread_id: ThreadId(thread.into()),
            claimed_run_id: RunId(format!("{thread}-run")),
            runs: Vec::new(),
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: vec![awaken_agent_contract::audit::record::Record {
                sequence: 1,
                run_id: RunId(format!("{thread}-run")),
                kind: awaken_agent_contract::audit::kind::Kind::PermissionDecided,
                payload: serde_json::json!({ "call_id": call_id, "decision": decision }),
            }],
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 0,
        }
    }

    #[test]
    fn permission_summary_is_keyed_by_thread_and_call_independent_of_read_order() {
        // Causes: C1 the root and a child Thread each record a gate decision
        // for the same Runtime call id (ids are only unique within a Thread);
        // C2 the child snapshots arrive in a HashMap, so their read order is
        // arbitrary. Effects: E1 each Thread's call reports its own recorded
        // decision; E2 the summary is the same for every read order.
        // Rules: R1 C1+C2 => E1+E2.
        let root = decided("session", "call-1", "allow");
        let child = decided("child", "call-1", "deny");
        let forward = harvest_permission_decisions([&root, &child].into_iter());
        let backward = harvest_permission_decisions([&child, &root].into_iter());
        assert_eq!(forward, backward, "R1/E2 read order changes nothing");
        let decision = |thread: &str| {
            forward
                .get(&(thread.to_string(), "call-1".to_string()))
                .map(|recorded| recorded.decision)
        };
        assert_eq!(
            decision("session"),
            Some(EvaluatedPermission::Allow),
            "R1/E1 root"
        );
        assert_eq!(
            decision("child"),
            Some(EvaluatedPermission::Deny),
            "R1/E1 child"
        );
    }
}
