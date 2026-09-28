// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Reads the gate's recorded PermissionDecided audit rows so the Managed projection
// @formal reason: reports the recorded decision instead of inferring `allow` from a result; moved
// @formal reason: unchanged out of the candidate builder. It decides nothing and widens nothing: an
// @formal reason: unknown label is skipped. Covered by the recorded-permission projection tests.
//! Recorded permission decisions harvested from committed audit trails.

use super::*;

/// Collect the gate's recorded decision for every call named in these runs'
/// committed audit trails, with the policy and grounds recorded beside it.
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
) -> std::collections::HashMap<String, RecordedPermission> {
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
                call_id.to_string(),
                RecordedPermission::from_audit(decision, policy, reason_code),
            );
        }
    }
    decisions
}
