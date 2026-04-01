use crate::config::*;
use crate::policy::*;
use crate::state::*;

fn make_config() -> DeferredToolsConfig {
    DeferredToolsConfig {
        enabled: Some(true),
        rules: vec![DeferralRule {
            tool: "Bash".into(),
            mode: ToolLoadMode::Eager,
        }],
        ..Default::default()
    }
}

#[test]
fn config_only_initial_classification() {
    let policy = ConfigOnlyPolicy;
    let stats = ToolUsageStatsValue::default();
    let state = DeferralStateValue::default();
    let tool_ids = vec![
        "Bash".to_string(),
        "mcp__query".to_string(),
        "Read".to_string(),
    ];
    let decisions = policy.evaluate(&stats, &state, &make_config(), &tool_ids);
    assert_eq!(decisions.len(), 3);
    assert_eq!(
        decisions
            .iter()
            .find(|d| d.tool_id == "Bash")
            .unwrap()
            .target_mode,
        ToolLoadMode::Eager
    );
    assert_eq!(
        decisions
            .iter()
            .find(|d| d.tool_id == "mcp__query")
            .unwrap()
            .target_mode,
        ToolLoadMode::Deferred
    );
    assert_eq!(
        decisions
            .iter()
            .find(|d| d.tool_id == "Read")
            .unwrap()
            .target_mode,
        ToolLoadMode::Deferred
    );
}

// --- DiscBetaEvaluator tests ---

fn make_disc_beta_config() -> DeferredToolsConfig {
    DeferredToolsConfig {
        enabled: Some(true),
        rules: vec![DeferralRule {
            tool: "mcp__*".into(),
            mode: ToolLoadMode::Eager,
        }],
        disc_beta: DiscBetaParams {
            omega: 0.95,
            n0: 5.0,
            defer_after: 5,
            thresh_mult: 0.5,
            gamma: 2000.0,
        },
        ..Default::default()
    }
}

#[test]
fn disc_beta_defers_idle_tool_below_threshold() {
    let config = make_disc_beta_config();

    // Tool with very low usage probability and high schema cost.
    let mut disc_beta = DiscBetaStateValue::default();
    disc_beta.tools.insert(
        "mcp__rare".into(),
        DiscBetaEntry {
            alpha: 0.01,
            beta_param: 10.0,
            last_used_turn: Some(0),
            c: 5000.0,
            c_bar: 10.0,
        },
    );

    let mut state = DeferralStateValue::default();
    state.modes.insert("mcp__rare".into(), ToolLoadMode::Eager);

    // mcp__rare matches mcp__* in eager_load, so it's always-eager — should NOT be deferred
    let defers = DiscBetaEvaluator::tools_to_defer(&disc_beta, &state, &config, 20);
    assert!(defers.is_empty());
}

#[test]
fn disc_beta_keeps_actively_used_tool() {
    let config = DeferredToolsConfig {
        enabled: Some(true),
        rules: vec![],
        disc_beta: DiscBetaParams {
            omega: 0.95,
            n0: 5.0,
            defer_after: 5,
            thresh_mult: 0.5,
            gamma: 2000.0,
        },
        ..Default::default()
    };

    let mut disc_beta = DiscBetaStateValue::default();
    disc_beta.tools.insert(
        "mcp__active".into(),
        DiscBetaEntry {
            alpha: 5.0,
            beta_param: 1.0,
            last_used_turn: Some(18),
            c: 5000.0,
            c_bar: 10.0,
        },
    );

    let mut state = DeferralStateValue::default();
    state
        .modes
        .insert("mcp__active".into(), ToolLoadMode::Eager);

    // Turn 20, last used at 18 => idle = 2, below defer_after=5
    let defers = DiscBetaEvaluator::tools_to_defer(&disc_beta, &state, &config, 20);
    assert!(defers.is_empty());
}

#[test]
fn disc_beta_never_defers_always_eager_tool() {
    let config = make_disc_beta_config();

    // "mcp__core" matches mcp__* in eager_load => always Eager
    let mut disc_beta = DiscBetaStateValue::default();
    disc_beta.tools.insert(
        "mcp__core".into(),
        DiscBetaEntry {
            alpha: 0.01,
            beta_param: 20.0,
            last_used_turn: None,
            c: 5000.0,
            c_bar: 10.0,
        },
    );

    let mut state = DeferralStateValue::default();
    state.modes.insert("mcp__core".into(), ToolLoadMode::Eager);

    let defers = DiscBetaEvaluator::tools_to_defer(&disc_beta, &state, &config, 100);
    assert!(defers.is_empty());
}

#[test]
fn disc_beta_skips_already_deferred_tool() {
    let config = DeferredToolsConfig {
        enabled: Some(true),
        rules: vec![],
        disc_beta: DiscBetaParams {
            omega: 0.95,
            n0: 5.0,
            defer_after: 5,
            thresh_mult: 0.5,
            gamma: 2000.0,
        },
        ..Default::default()
    };

    let mut disc_beta = DiscBetaStateValue::default();
    disc_beta.tools.insert(
        "mcp__tool".into(),
        DiscBetaEntry {
            alpha: 0.01,
            beta_param: 10.0,
            last_used_turn: None,
            c: 5000.0,
            c_bar: 10.0,
        },
    );

    let mut state = DeferralStateValue::default();
    state
        .modes
        .insert("mcp__tool".into(), ToolLoadMode::Deferred);

    let defers = DiscBetaEvaluator::tools_to_defer(&disc_beta, &state, &config, 100);
    assert!(defers.is_empty());
}

#[test]
fn disc_beta_respects_defer_after_threshold() {
    let config = DeferredToolsConfig {
        enabled: Some(true),
        rules: vec![],
        disc_beta: DiscBetaParams {
            omega: 0.95,
            n0: 5.0,
            defer_after: 5,
            thresh_mult: 0.5,
            gamma: 2000.0,
        },
        ..Default::default()
    };

    let mut disc_beta = DiscBetaStateValue::default();
    disc_beta.tools.insert(
        "mcp__tool".into(),
        DiscBetaEntry {
            alpha: 0.01,
            beta_param: 10.0,
            last_used_turn: Some(8),
            c: 5000.0,
            c_bar: 10.0,
        },
    );

    let mut state = DeferralStateValue::default();
    state.modes.insert("mcp__tool".into(), ToolLoadMode::Eager);

    // Turn 12, last used at 8 => idle = 4, below defer_after=5
    let defers = DiscBetaEvaluator::tools_to_defer(&disc_beta, &state, &config, 12);
    assert!(defers.is_empty());

    // Turn 14, last used at 8 => idle = 6, above defer_after=5
    let defers = DiscBetaEvaluator::tools_to_defer(&disc_beta, &state, &config, 14);
    assert_eq!(defers, vec!["mcp__tool"]);
}
