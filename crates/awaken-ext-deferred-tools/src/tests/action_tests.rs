use awaken_contract::model::ScheduledActionSpec;
use awaken_contract::state::StateCommand;
use awaken_runtime::state::StateKey;

use crate::config::{DeferredToolsConfig, ToolLoadMode};
use crate::plugin::hooks::{apply_deferral_actions, build_deferred_tool_list, collect_exclusions};
use crate::state::{
    DeferToolAction, DeferralState, DeferralStateAction, DeferralStateValue, PromoteToolAction,
};
use crate::tool_search::TOOL_SEARCH_ID;

// ---------------------------------------------------------------------------
// 1. Action handler lifecycle tests (state-level)
// ---------------------------------------------------------------------------

#[test]
fn defer_action_sets_tool_to_deferred() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("tool_a".into(), ToolLoadMode::Eager);

    DeferralState::apply(
        &mut state,
        DeferralStateAction::SetBatch(vec![("tool_a".into(), ToolLoadMode::Deferred)]),
    );

    assert_eq!(state.modes["tool_a"], ToolLoadMode::Deferred);
}

#[test]
fn promote_action_sets_tool_to_eager() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("tool_a".into(), ToolLoadMode::Deferred);

    DeferralState::apply(
        &mut state,
        DeferralStateAction::PromoteBatch(vec!["tool_a".into()]),
    );

    assert_eq!(state.modes["tool_a"], ToolLoadMode::Eager);
}

// ---------------------------------------------------------------------------
// 2. Conflict resolution tests
// ---------------------------------------------------------------------------

#[test]
fn promote_wins_over_defer_when_applied_in_order() {
    let mut state = DeferralStateValue::default();
    DeferralState::apply(&mut state, DeferralStateAction::Defer("tool_x".into()));
    DeferralState::apply(&mut state, DeferralStateAction::Promote("tool_x".into()));
    assert_eq!(state.modes["tool_x"], ToolLoadMode::Eager);
}

#[test]
fn defer_wins_over_promote_when_applied_last() {
    let mut state = DeferralStateValue::default();
    DeferralState::apply(&mut state, DeferralStateAction::Promote("tool_x".into()));
    DeferralState::apply(&mut state, DeferralStateAction::Defer("tool_x".into()));
    assert_eq!(state.modes["tool_x"], ToolLoadMode::Deferred);
}

// ---------------------------------------------------------------------------
// 3. apply_deferral_actions tests
// ---------------------------------------------------------------------------

#[test]
fn apply_deferral_actions_promote_wins_on_conflict() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("tool_a".into(), ToolLoadMode::Eager);

    let defer_payloads = vec![vec!["tool_a".into()]];
    let promote_payloads = vec![vec!["tool_a".into()]];
    apply_deferral_actions(&mut state, &defer_payloads, &promote_payloads);

    // Promote is applied after defer, so promote wins
    assert_eq!(state.modes["tool_a"], ToolLoadMode::Eager);
}

#[test]
fn apply_deferral_actions_multiple_batches() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("t1".into(), ToolLoadMode::Eager);
    state.modes.insert("t2".into(), ToolLoadMode::Eager);
    state.modes.insert("t3".into(), ToolLoadMode::Deferred);

    let defer_payloads = vec![vec!["t1".into()], vec!["t2".into()]];
    let promote_payloads = vec![vec!["t3".into()]];
    apply_deferral_actions(&mut state, &defer_payloads, &promote_payloads);

    assert_eq!(state.modes["t1"], ToolLoadMode::Deferred);
    assert_eq!(state.modes["t2"], ToolLoadMode::Deferred);
    assert_eq!(state.modes["t3"], ToolLoadMode::Eager);
}

// ---------------------------------------------------------------------------
// 4. Cross-module scheduling tests
// ---------------------------------------------------------------------------

#[test]
fn schedule_defer_action_encodes_correctly() {
    let mut cmd = StateCommand::new();
    cmd.schedule_action::<DeferToolAction>(vec!["tool_a".into(), "tool_b".into()])
        .unwrap();

    assert_eq!(cmd.scheduled_actions.len(), 1);
    assert_eq!(cmd.scheduled_actions[0].key, DeferToolAction::KEY);

    let payload: Vec<String> =
        serde_json::from_value(cmd.scheduled_actions[0].payload.clone()).unwrap();
    assert_eq!(payload, vec!["tool_a", "tool_b"]);
}

#[test]
fn schedule_promote_action_encodes_correctly() {
    let mut cmd = StateCommand::new();
    cmd.schedule_action::<PromoteToolAction>(vec!["tool_c".into()])
        .unwrap();

    assert_eq!(cmd.scheduled_actions.len(), 1);
    assert_eq!(cmd.scheduled_actions[0].key, PromoteToolAction::KEY);

    let payload: Vec<String> =
        serde_json::from_value(cmd.scheduled_actions[0].payload.clone()).unwrap();
    assert_eq!(payload, vec!["tool_c"]);
}

// ---------------------------------------------------------------------------
// 5. Configuration impact tests
// ---------------------------------------------------------------------------

#[test]
fn config_enabled_false_disables_deferral() {
    let config = DeferredToolsConfig {
        enabled: Some(false),
        ..Default::default()
    };
    assert!(!config.should_enable(10_000.0));
}

#[test]
fn config_enabled_true_forces_deferral() {
    let config = DeferredToolsConfig {
        enabled: Some(true),
        ..Default::default()
    };
    assert!(config.should_enable(0.0));
}

#[test]
fn config_enabled_none_uses_auto_threshold() {
    let config = DeferredToolsConfig::default(); // enabled: None
    assert!(!config.should_enable(0.0)); // Below threshold
    assert!(config.should_enable(2000.0)); // Above default beta_overhead (1136)
}

#[test]
fn config_enabled_none_boundary() {
    let config = DeferredToolsConfig::default();
    // Exactly at threshold — not above, so should be false
    assert!(!config.should_enable(config.beta_overhead));
    // Just above threshold
    assert!(config.should_enable(config.beta_overhead + 0.01));
}

// ---------------------------------------------------------------------------
// 6. Empty payload edge cases
// ---------------------------------------------------------------------------

#[test]
fn defer_empty_payload_is_noop() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("tool_a".into(), ToolLoadMode::Eager);
    DeferralState::apply(&mut state, DeferralStateAction::SetBatch(vec![]));
    assert_eq!(state.modes["tool_a"], ToolLoadMode::Eager);
    assert_eq!(state.modes.len(), 1);
}

#[test]
fn promote_empty_payload_is_noop() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("tool_a".into(), ToolLoadMode::Deferred);
    DeferralState::apply(&mut state, DeferralStateAction::PromoteBatch(vec![]));
    assert_eq!(state.modes["tool_a"], ToolLoadMode::Deferred);
    assert_eq!(state.modes.len(), 1);
}

#[test]
fn apply_deferral_actions_empty_payloads_is_noop() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("tool_a".into(), ToolLoadMode::Eager);
    apply_deferral_actions(&mut state, &[], &[]);
    assert_eq!(state.modes["tool_a"], ToolLoadMode::Eager);
}

// ---------------------------------------------------------------------------
// 7. Scope tests — collect_exclusions and build_deferred_tool_list
// ---------------------------------------------------------------------------

#[test]
fn collect_exclusions_skips_tool_search() {
    let mut state = DeferralStateValue::default();
    state
        .modes
        .insert("regular_tool".into(), ToolLoadMode::Deferred);
    state
        .modes
        .insert(TOOL_SEARCH_ID.into(), ToolLoadMode::Deferred);

    let exclusions = collect_exclusions(&state);
    assert!(exclusions.contains(&"regular_tool".to_string()));
    assert!(!exclusions.contains(&TOOL_SEARCH_ID.to_string()));
}

#[test]
fn build_deferred_tool_list_empty_for_all_eager() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("tool_a".into(), ToolLoadMode::Eager);
    assert!(build_deferred_tool_list(&state).is_empty());
}

#[test]
fn build_deferred_tool_list_empty_for_empty_state() {
    let state = DeferralStateValue::default();
    assert!(build_deferred_tool_list(&state).is_empty());
}

#[test]
fn build_deferred_tool_list_sorted_output() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("zebra".into(), ToolLoadMode::Deferred);
    state.modes.insert("alpha".into(), ToolLoadMode::Deferred);
    let list = build_deferred_tool_list(&state);
    assert!(list.contains("alpha, zebra"));
}

#[test]
fn collect_exclusions_empty_for_all_eager() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("tool_a".into(), ToolLoadMode::Eager);
    state.modes.insert("tool_b".into(), ToolLoadMode::Eager);
    assert!(collect_exclusions(&state).is_empty());
}
