use awaken_runtime::state::StateKey;

use crate::config::*;
use crate::policy::*;
use crate::state::*;

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

// ---------------------------------------------------------------------------
// Runtime promote survival
// ---------------------------------------------------------------------------
//
// When ToolSearch (or skill activation) promotes a deferred tool at runtime,
// BeforeInference must NOT re-classify tools from static config (which would
// immediately reverse the promote). Only DiscBeta may re-defer, and only for
// genuinely idle tools.

/// Simulates the BeforeInference hook: no config re-classification, only
/// DiscBeta re-defer for idle tools.
fn simulate_before_inference(
    state: &DeferralStateValue,
    config: &DeferredToolsConfig,
    disc_beta: &DiscBetaStateValue,
    usage_stats: &ToolUsageStatsValue,
) -> DeferralStateValue {
    let mut effective = state.clone();

    let re_defer_ids =
        DiscBetaEvaluator::tools_to_defer(disc_beta, &effective, config, usage_stats.total_turns);
    for id in re_defer_ids {
        effective.modes.insert(id, ToolLoadMode::Deferred);
    }

    effective
}

/// A runtime-promoted tool survives BeforeInference (not re-deferred from config).
#[test]
fn promoted_tool_survives_before_inference() {
    let config = DeferredToolsConfig {
        enabled: Some(true),
        rules: vec![],
        ..Default::default()
    };
    let disc_beta = DiscBetaStateValue::default();
    let stats = ToolUsageStatsValue::default();

    let mut state = DeferralStateValue::default();
    state
        .modes
        .insert("mcp__query".into(), ToolLoadMode::Deferred);

    // ToolSearch promotes the tool.
    DeferralState::apply(
        &mut state,
        DeferralStateAction::Promote("mcp__query".into()),
    );

    let effective = simulate_before_inference(&state, &config, &disc_beta, &stats);
    assert_eq!(
        effective.modes["mcp__query"],
        ToolLoadMode::Eager,
        "promoted tool should survive BeforeInference"
    );
}

/// DiscBeta can still re-defer an idle promoted tool.
#[test]
fn disc_beta_still_re_defers_idle_promoted_tool() {
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
        "mcp__query".into(),
        DiscBetaEntry {
            alpha: 0.01,
            beta_param: 10.0,
            last_used_turn: Some(0),
            c: 5000.0,
            c_bar: 10.0,
        },
    );

    let mut state = DeferralStateValue::default();
    state.modes.insert("mcp__query".into(), ToolLoadMode::Eager);

    let stats = ToolUsageStatsValue {
        total_turns: 20, // idle for 20 turns >> defer_after=5
        ..Default::default()
    };

    let effective = simulate_before_inference(&state, &config, &disc_beta, &stats);
    assert_eq!(
        effective.modes["mcp__query"],
        ToolLoadMode::Deferred,
        "DiscBeta should re-defer idle promoted tool"
    );
}
