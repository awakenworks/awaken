use std::sync::Arc;

use awaken_contract::StateMap;
use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
use awaken_contract::contract::tool_intercept::ToolInterceptPayload;
use awaken_contract::model::Phase;
use awaken_contract::state::Snapshot;
use awaken_runtime::{PhaseContext, ToolGateHook};
use serde_json::json;

use crate::rules::{PermissionRule, ToolPermissionBehavior};
use crate::state::{PermissionPolicy, PermissionPolicyKey};

use super::checker::PermissionToolGateHook;

fn snapshot_with_policy(policy: PermissionPolicy) -> Snapshot {
    let mut state_map = StateMap::default();
    state_map.insert::<PermissionPolicyKey>(policy);
    Snapshot::new(0, Arc::new(state_map))
}

fn make_ctx(snapshot: Snapshot, tool_name: &str, tool_args: serde_json::Value) -> PhaseContext {
    PhaseContext::new(Phase::ToolGate, snapshot).with_tool_info(
        tool_name,
        "call_1",
        Some(tool_args),
    )
}

fn resume_input() -> ToolCallResume {
    ToolCallResume {
        decision_id: "d1".into(),
        action: ResumeDecisionAction::Resume,
        result: json!({"approved": true}),
        reason: Some("user approved".into()),
        updated_at: 0,
    }
}

// -----------------------------------------------------------------------
// Vulnerability test: resumed denied tool must still be blocked
// -----------------------------------------------------------------------

#[tokio::test]
async fn resumed_denied_tool_is_blocked() {
    let mut policy = PermissionPolicy::default();
    let rule = PermissionRule::new_tool("dangerous_tool", ToolPermissionBehavior::Deny);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy), "dangerous_tool", json!({}))
        .with_resume_input(resume_input());

    let intercept = PermissionToolGateHook.run(&ctx).await.unwrap();

    // A denied tool MUST be blocked even when resumed
    assert!(
        matches!(intercept, Some(ToolInterceptPayload::Block { .. })),
        "expected Block intercept for denied tool on resume, got {intercept:?}"
    );
}

// -----------------------------------------------------------------------
// Allowed tool on resume should proceed (no intercept)
// -----------------------------------------------------------------------

#[tokio::test]
async fn resumed_allowed_tool_proceeds() {
    let mut policy = PermissionPolicy::default();
    let rule = PermissionRule::new_tool("safe_tool", ToolPermissionBehavior::Allow);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy), "safe_tool", json!({}))
        .with_resume_input(resume_input());

    assert!(
        PermissionToolGateHook.run(&ctx).await.unwrap().is_none(),
        "allowed tool should proceed on resume"
    );
}

// -----------------------------------------------------------------------
// Ask tool on resume should proceed (user already approved)
// -----------------------------------------------------------------------

#[tokio::test]
async fn resumed_ask_tool_proceeds() {
    let mut policy = PermissionPolicy::default();
    let rule = PermissionRule::new_tool("ask_tool", ToolPermissionBehavior::Ask);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy), "ask_tool", json!({}))
        .with_resume_input(resume_input());

    // Ask was already approved by user via resume, should not re-suspend
    assert!(
        PermissionToolGateHook.run(&ctx).await.unwrap().is_none(),
        "ask tool should proceed on resume (user already approved)"
    );
}

// -----------------------------------------------------------------------
// Non-resumed denied tool is still blocked (regression guard)
// -----------------------------------------------------------------------

#[tokio::test]
async fn non_resumed_denied_tool_is_blocked() {
    let mut policy = PermissionPolicy::default();
    let rule = PermissionRule::new_tool("dangerous_tool", ToolPermissionBehavior::Deny);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy), "dangerous_tool", json!({}));

    assert!(matches!(
        PermissionToolGateHook.run(&ctx).await.unwrap(),
        Some(ToolInterceptPayload::Block { .. })
    ));
}

// -----------------------------------------------------------------------
// Non-resumed ask tool gets suspended
// -----------------------------------------------------------------------

#[tokio::test]
async fn non_resumed_ask_tool_is_suspended() {
    let mut policy = PermissionPolicy::default();
    let rule = PermissionRule::new_tool("ask_tool", ToolPermissionBehavior::Ask);
    policy.rules.insert(rule.subject.key(), rule);

    let ctx = make_ctx(snapshot_with_policy(policy), "ask_tool", json!({}));

    assert!(matches!(
        PermissionToolGateHook.run(&ctx).await.unwrap(),
        Some(ToolInterceptPayload::Suspend(_))
    ));
}
