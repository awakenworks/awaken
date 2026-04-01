use crate::config::ToolLoadMode;
use crate::plugin::hooks::*;
use crate::state::*;
use crate::tool_search::TOOL_SEARCH_ID;
use serde_json::json;

#[test]
fn build_deferred_list_from_state() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("Bash".into(), ToolLoadMode::Eager);
    state
        .modes
        .insert("mcp__query".into(), ToolLoadMode::Deferred);
    state
        .modes
        .insert("WebSearch".into(), ToolLoadMode::Deferred);
    state.modes.insert("Read".into(), ToolLoadMode::Eager);
    let list = build_deferred_tool_list(&state);
    assert!(list.contains("mcp__query"));
    assert!(list.contains("WebSearch"));
    assert!(!list.contains("Bash"));
}

#[test]
fn build_deferred_list_empty_when_all_eager() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("Bash".into(), ToolLoadMode::Eager);
    let list = build_deferred_tool_list(&state);
    assert!(list.is_empty());
}

#[test]
fn collect_exclusions_returns_deferred_ids() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("Bash".into(), ToolLoadMode::Eager);
    state.modes.insert("mcp__a".into(), ToolLoadMode::Deferred);
    state.modes.insert("mcp__b".into(), ToolLoadMode::Deferred);
    let mut exclusions = collect_exclusions(&state);
    exclusions.sort();
    assert_eq!(exclusions, vec!["mcp__a", "mcp__b"]);
}

#[test]
fn collect_exclusions_never_excludes_tool_search() {
    let mut state = DeferralStateValue::default();
    state
        .modes
        .insert(TOOL_SEARCH_ID.into(), ToolLoadMode::Deferred);
    let exclusions = collect_exclusions(&state);
    assert!(!exclusions.contains(&TOOL_SEARCH_ID.to_string()));
}

#[test]
fn extract_promote_ids_from_tool_search_result() {
    let data = json!({"tools": "<functions>...</functions>", "__promote": ["tool_a", "tool_b"]});
    let ids = extract_promote_ids_from_tool_result(&data);
    assert_eq!(ids, vec!["tool_a".to_string(), "tool_b".to_string()]);
}

#[test]
fn extract_promote_ids_missing_field() {
    let data = json!({"tools": "..."});
    assert!(extract_promote_ids_from_tool_result(&data).is_empty());
}

#[test]
fn extract_promote_ids_empty_array() {
    let data = json!({"__promote": []});
    assert!(extract_promote_ids_from_tool_result(&data).is_empty());
}

#[test]
fn apply_defer_and_promote_actions() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("t1".into(), ToolLoadMode::Eager);
    state.modes.insert("t2".into(), ToolLoadMode::Deferred);
    apply_deferral_actions(
        &mut state,
        &[vec!["t1".to_string()]],
        &[vec!["t2".to_string()]],
    );
    assert_eq!(state.modes["t1"], ToolLoadMode::Deferred);
    assert_eq!(state.modes["t2"], ToolLoadMode::Eager);
}

#[test]
fn apply_promote_wins_over_defer_for_same_tool() {
    let mut state = DeferralStateValue::default();
    state.modes.insert("t1".into(), ToolLoadMode::Eager);
    apply_deferral_actions(
        &mut state,
        &[vec!["t1".to_string()]],
        &[vec!["t1".to_string()]],
    );
    assert_eq!(state.modes["t1"], ToolLoadMode::Eager);
}
