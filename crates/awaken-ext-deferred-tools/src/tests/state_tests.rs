use awaken_runtime::state::StateKey;
use serde_json::json;

use crate::state::*;

#[test]
fn registry_register_single() {
    let mut val = DeferralRegistryValue::default();
    DeferralRegistry::apply(
        &mut val,
        DeferralRegistryAction::Register(StoredToolDescriptor {
            id: "tool_a".into(),
            name: "ToolA".into(),
            description: "A tool".into(),
            parameters: json!({"type": "object"}),
            category: None,
        }),
    );
    assert_eq!(val.tools.len(), 1);
    assert!(val.tools.contains_key("tool_a"));
}

#[test]
fn registry_register_batch() {
    let mut val = DeferralRegistryValue::default();
    DeferralRegistry::apply(
        &mut val,
        DeferralRegistryAction::RegisterBatch(vec![
            StoredToolDescriptor {
                id: "t1".into(),
                name: "T1".into(),
                description: "first".into(),
                parameters: json!({}),
                category: None,
            },
            StoredToolDescriptor {
                id: "t2".into(),
                name: "T2".into(),
                description: "second".into(),
                parameters: json!({}),
                category: Some("mcp".into()),
            },
        ]),
    );
    assert_eq!(val.tools.len(), 2);
    assert_eq!(val.tools["t2"].category.as_deref(), Some("mcp"));
}

#[test]
fn registry_register_overwrites() {
    let mut val = DeferralRegistryValue::default();
    DeferralRegistry::apply(
        &mut val,
        DeferralRegistryAction::Register(StoredToolDescriptor {
            id: "t1".into(),
            name: "T1".into(),
            description: "old".into(),
            parameters: json!({}),
            category: None,
        }),
    );
    DeferralRegistry::apply(
        &mut val,
        DeferralRegistryAction::Register(StoredToolDescriptor {
            id: "t1".into(),
            name: "T1".into(),
            description: "new".into(),
            parameters: json!({}),
            category: None,
        }),
    );
    assert_eq!(val.tools.len(), 1);
    assert_eq!(val.tools["t1"].description, "new");
}

#[test]
fn registry_remove() {
    let mut val = DeferralRegistryValue::default();
    DeferralRegistry::apply(
        &mut val,
        DeferralRegistryAction::RegisterBatch(vec![
            StoredToolDescriptor {
                id: "t1".into(),
                name: "T1".into(),
                description: "".into(),
                parameters: json!({}),
                category: None,
            },
            StoredToolDescriptor {
                id: "t2".into(),
                name: "T2".into(),
                description: "".into(),
                parameters: json!({}),
                category: None,
            },
        ]),
    );
    DeferralRegistry::apply(&mut val, DeferralRegistryAction::Remove("t1".into()));
    assert_eq!(val.tools.len(), 1);
    assert!(!val.tools.contains_key("t1"));
    assert!(val.tools.contains_key("t2"));
}

#[test]
fn registry_remove_nonexistent_is_noop() {
    let mut val = DeferralRegistryValue::default();
    DeferralRegistry::apply(&mut val, DeferralRegistryAction::Remove("ghost".into()));
    assert!(val.tools.is_empty());
}

use crate::config::ToolLoadMode;

#[test]
fn deferral_state_set_batch() {
    let mut val = DeferralStateValue::default();
    DeferralState::apply(
        &mut val,
        DeferralStateAction::SetBatch(vec![
            ("Bash".into(), ToolLoadMode::Eager),
            ("mcp__query".into(), ToolLoadMode::Deferred),
        ]),
    );
    assert_eq!(val.modes["Bash"], ToolLoadMode::Eager);
    assert_eq!(val.modes["mcp__query"], ToolLoadMode::Deferred);
}

#[test]
fn deferral_state_defer_and_promote() {
    let mut val = DeferralStateValue::default();
    DeferralState::apply(
        &mut val,
        DeferralStateAction::SetBatch(vec![("tool_a".into(), ToolLoadMode::Eager)]),
    );
    DeferralState::apply(&mut val, DeferralStateAction::Defer("tool_a".into()));
    assert_eq!(val.modes["tool_a"], ToolLoadMode::Deferred);
    DeferralState::apply(&mut val, DeferralStateAction::Promote("tool_a".into()));
    assert_eq!(val.modes["tool_a"], ToolLoadMode::Eager);
}

#[test]
fn deferral_state_promote_batch() {
    let mut val = DeferralStateValue::default();
    DeferralState::apply(
        &mut val,
        DeferralStateAction::SetBatch(vec![
            ("t1".into(), ToolLoadMode::Deferred),
            ("t2".into(), ToolLoadMode::Deferred),
            ("t3".into(), ToolLoadMode::Eager),
        ]),
    );
    DeferralState::apply(
        &mut val,
        DeferralStateAction::PromoteBatch(vec!["t1".into(), "t2".into()]),
    );
    assert_eq!(val.modes["t1"], ToolLoadMode::Eager);
    assert_eq!(val.modes["t2"], ToolLoadMode::Eager);
    assert_eq!(val.modes["t3"], ToolLoadMode::Eager);
}

#[test]
fn deferral_state_promote_unknown_inserts_eager() {
    let mut val = DeferralStateValue::default();
    DeferralState::apply(&mut val, DeferralStateAction::Promote("new_tool".into()));
    assert_eq!(val.modes["new_tool"], ToolLoadMode::Eager);
}

#[test]
fn deferral_state_deferred_tool_ids() {
    let mut val = DeferralStateValue::default();
    DeferralState::apply(
        &mut val,
        DeferralStateAction::SetBatch(vec![
            ("eager1".into(), ToolLoadMode::Eager),
            ("defer1".into(), ToolLoadMode::Deferred),
            ("defer2".into(), ToolLoadMode::Deferred),
            ("eager2".into(), ToolLoadMode::Eager),
        ]),
    );
    let mut deferred = val.deferred_tool_ids();
    deferred.sort();
    assert_eq!(deferred, vec!["defer1", "defer2"]);
}

#[test]
fn usage_stats_increment_turn() {
    let mut val = ToolUsageStatsValue::default();
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    assert_eq!(val.total_turns, 2);
}

#[test]
fn usage_stats_record_call() {
    let mut val = ToolUsageStatsValue::default();
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    ToolUsageStats::apply(
        &mut val,
        ToolUsageStatsAction::RecordCall {
            tool_id: "Bash".into(),
        },
    );
    ToolUsageStats::apply(
        &mut val,
        ToolUsageStatsAction::RecordCall {
            tool_id: "Bash".into(),
        },
    );
    let entry = &val.tools["Bash"];
    assert_eq!(entry.total_call_count, 2);
    assert_eq!(entry.turn_presence_count, 1);
    assert_eq!(entry.first_use_turn, Some(1));
    assert_eq!(entry.last_use_turn, Some(1));
}

#[test]
fn usage_stats_record_call_across_turns() {
    let mut val = ToolUsageStatsValue::default();
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    ToolUsageStats::apply(
        &mut val,
        ToolUsageStatsAction::RecordCall {
            tool_id: "Bash".into(),
        },
    );
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    ToolUsageStats::apply(
        &mut val,
        ToolUsageStatsAction::RecordCall {
            tool_id: "Bash".into(),
        },
    );
    let entry = &val.tools["Bash"];
    assert_eq!(entry.total_call_count, 2);
    assert_eq!(entry.turn_presence_count, 2);
    assert_eq!(entry.first_use_turn, Some(1));
    assert_eq!(entry.last_use_turn, Some(3));
}

#[test]
fn usage_stats_record_turn_calls_batch() {
    let mut val = ToolUsageStatsValue::default();
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    ToolUsageStats::apply(
        &mut val,
        ToolUsageStatsAction::RecordTurnCalls {
            calls: vec![("Bash".into(), 3), ("Read".into(), 1)],
        },
    );
    assert_eq!(val.tools["Bash"].total_call_count, 3);
    assert_eq!(val.tools["Bash"].turn_presence_count, 1);
    assert_eq!(val.tools["Read"].total_call_count, 1);
}

#[test]
fn usage_stats_presence_freq() {
    let mut val = ToolUsageStatsValue::default();
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    ToolUsageStats::apply(
        &mut val,
        ToolUsageStatsAction::RecordCall {
            tool_id: "Bash".into(),
        },
    );
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    ToolUsageStats::apply(&mut val, ToolUsageStatsAction::IncrementTurn);
    ToolUsageStats::apply(
        &mut val,
        ToolUsageStatsAction::RecordCall {
            tool_id: "Bash".into(),
        },
    );
    let freq = val.tools["Bash"].presence_freq(val.total_turns);
    assert!((freq - 2.0 / 3.0).abs() < 1e-9);
}

#[test]
fn usage_stats_empty_presence_freq_is_zero() {
    let entry = ToolUsageEntry::default();
    assert_eq!(entry.presence_freq(0), 0.0);
    assert_eq!(entry.presence_freq(10), 0.0);
}

use awaken_contract::contract::profile_store::ProfileKey;

use crate::state::{AgentToolPriors, AgentToolPriorsKey, DeferToolAction, PromoteToolAction};
use awaken_contract::model::ScheduledActionSpec;

#[test]
fn defer_tool_action_spec() {
    assert_eq!(DeferToolAction::KEY, "deferred_tools.defer");
    assert_eq!(
        DeferToolAction::PHASE,
        awaken_contract::model::Phase::BeforeInference
    );
}

#[test]
fn promote_tool_action_spec() {
    assert_eq!(PromoteToolAction::KEY, "deferred_tools.promote");
    assert_eq!(
        PromoteToolAction::PHASE,
        awaken_contract::model::Phase::BeforeInference
    );
}

#[test]
fn defer_tool_action_roundtrip() {
    let payload = vec!["tool_a".to_string(), "tool_b".to_string()];
    let encoded = DeferToolAction::encode_payload(&payload).unwrap();
    let decoded = DeferToolAction::decode_payload(encoded).unwrap();
    assert_eq!(decoded, payload);
}

#[test]
fn promote_tool_action_roundtrip() {
    let payload = vec!["tool_x".to_string()];
    let encoded = PromoteToolAction::encode_payload(&payload).unwrap();
    let decoded = PromoteToolAction::decode_payload(encoded).unwrap();
    assert_eq!(decoded, payload);
}

// ---------------------------------------------------------------------------
// DiscBetaState tests
// ---------------------------------------------------------------------------

#[test]
fn disc_beta_entry_new_from_prior() {
    let entry = DiscBetaEntry::new(0.1, 10.0, 500.0, 5.0);
    assert!((entry.alpha - 1.0).abs() < f64::EPSILON);
    assert!((entry.beta_param - 9.0).abs() < f64::EPSILON);
    assert_eq!(entry.last_used_turn, None);
    assert!((entry.c - 500.0).abs() < f64::EPSILON);
    assert!((entry.c_bar - 5.0).abs() < f64::EPSILON);
}

#[test]
fn disc_beta_entry_new_clamps_minimum() {
    let entry = DiscBetaEntry::new(0.0, 0.0, 10.0, 1.0);
    assert!((entry.alpha - 0.01).abs() < f64::EPSILON);
    assert!((entry.beta_param - 0.01).abs() < f64::EPSILON);
}

#[test]
fn disc_beta_mean() {
    let entry = DiscBetaEntry {
        alpha: 3.0,
        beta_param: 7.0,
        last_used_turn: None,
        c: 100.0,
        c_bar: 10.0,
    };
    assert!((entry.mean() - 0.3).abs() < 1e-10);
}

#[test]
fn disc_beta_mean_zero_total() {
    let entry = DiscBetaEntry {
        alpha: 0.0,
        beta_param: 0.0,
        last_used_turn: None,
        c: 0.0,
        c_bar: 0.0,
    };
    assert_eq!(entry.mean(), 0.0);
}

#[test]
fn disc_beta_upper_ci_above_mean() {
    let entry = DiscBetaEntry {
        alpha: 3.0,
        beta_param: 7.0,
        last_used_turn: None,
        c: 100.0,
        c_bar: 10.0,
    };
    let ci = entry.upper_ci(0.90);
    assert!(ci > entry.mean());
    assert!(ci <= 1.0);
}

#[test]
fn disc_beta_upper_ci_clamped_at_one() {
    // With tiny sample, upper CI should not exceed 1.0
    let entry = DiscBetaEntry {
        alpha: 0.01,
        beta_param: 0.01,
        last_used_turn: None,
        c: 100.0,
        c_bar: 10.0,
    };
    assert!(entry.upper_ci(0.95) <= 1.0);
}

#[test]
fn disc_beta_effective_n() {
    let entry = DiscBetaEntry {
        alpha: 3.0,
        beta_param: 7.0,
        last_used_turn: None,
        c: 100.0,
        c_bar: 10.0,
    };
    assert!((entry.effective_n() - 10.0).abs() < f64::EPSILON);
}

#[test]
fn disc_beta_breakeven_p() {
    let entry = DiscBetaEntry {
        alpha: 1.0,
        beta_param: 1.0,
        last_used_turn: None,
        c: 1000.0,
        c_bar: 100.0,
    };
    // (1000 - 100) / 2000 = 0.45
    assert!((entry.breakeven_p(2000.0) - 0.45).abs() < 1e-10);
}

#[test]
fn disc_beta_breakeven_p_zero_gamma() {
    let entry = DiscBetaEntry::new(0.1, 5.0, 1000.0, 100.0);
    assert!(entry.breakeven_p(0.0).is_infinite());
}

#[test]
fn disc_beta_breakeven_p_no_savings() {
    let entry = DiscBetaEntry::new(0.1, 5.0, 10.0, 100.0);
    assert!(entry.breakeven_p(2000.0).is_infinite());
}

#[test]
fn disc_beta_init_batch() {
    let mut val = DiscBetaStateValue::default();
    DiscBetaState::apply(
        &mut val,
        DiscBetaAction::InitBatch(vec![
            ("t1".into(), DiscBetaEntry::new(0.1, 5.0, 500.0, 10.0)),
            ("t2".into(), DiscBetaEntry::new(0.5, 10.0, 200.0, 5.0)),
        ]),
    );
    assert_eq!(val.tools.len(), 2);
    assert!(val.tools.contains_key("t1"));
    assert!(val.tools.contains_key("t2"));
}

#[test]
fn disc_beta_init_batch_no_overwrite() {
    let mut val = DiscBetaStateValue::default();
    DiscBetaState::apply(
        &mut val,
        DiscBetaAction::InitBatch(vec![(
            "t1".into(),
            DiscBetaEntry::new(0.1, 5.0, 500.0, 10.0),
        )]),
    );
    let original_alpha = val.tools["t1"].alpha;
    // Second init should not overwrite
    DiscBetaState::apply(
        &mut val,
        DiscBetaAction::InitBatch(vec![(
            "t1".into(),
            DiscBetaEntry::new(0.9, 100.0, 500.0, 10.0),
        )]),
    );
    assert!((val.tools["t1"].alpha - original_alpha).abs() < f64::EPSILON);
}

#[test]
fn disc_beta_observe_turn_discounts_and_updates() {
    let mut val = DiscBetaStateValue::default();
    DiscBetaState::apply(
        &mut val,
        DiscBetaAction::InitBatch(vec![
            ("t1".into(), DiscBetaEntry::new(0.5, 10.0, 500.0, 10.0)),
            ("t2".into(), DiscBetaEntry::new(0.5, 10.0, 200.0, 5.0)),
        ]),
    );

    let alpha_t1_before = val.tools["t1"].alpha;
    let beta_t1_before = val.tools["t1"].beta_param;
    let alpha_t2_before = val.tools["t2"].alpha;
    let beta_t2_before = val.tools["t2"].beta_param;

    DiscBetaState::apply(
        &mut val,
        DiscBetaAction::ObserveTurn {
            omega: 0.95,
            current_turn: 1,
            tools_called: vec!["t1".into()],
        },
    );

    // t1 was called: alpha = old*0.95 + 1, beta = old*0.95
    let expected_alpha_t1 = alpha_t1_before * 0.95 + 1.0;
    let expected_beta_t1 = beta_t1_before * 0.95;
    assert!((val.tools["t1"].alpha - expected_alpha_t1).abs() < 1e-10);
    assert!((val.tools["t1"].beta_param - expected_beta_t1).abs() < 1e-10);
    assert_eq!(val.tools["t1"].last_used_turn, Some(1));

    // t2 was not called: alpha = old*0.95, beta = old*0.95 + 1
    let expected_alpha_t2 = alpha_t2_before * 0.95;
    let expected_beta_t2 = beta_t2_before * 0.95 + 1.0;
    assert!((val.tools["t2"].alpha - expected_alpha_t2).abs() < 1e-10);
    assert!((val.tools["t2"].beta_param - expected_beta_t2).abs() < 1e-10);
    assert_eq!(val.tools["t2"].last_used_turn, None);
}

#[test]
fn disc_beta_observe_turn_no_calls() {
    let mut val = DiscBetaStateValue::default();
    DiscBetaState::apply(
        &mut val,
        DiscBetaAction::InitBatch(vec![(
            "t1".into(),
            DiscBetaEntry::new(0.5, 10.0, 500.0, 10.0),
        )]),
    );

    DiscBetaState::apply(
        &mut val,
        DiscBetaAction::ObserveTurn {
            omega: 0.95,
            current_turn: 1,
            tools_called: vec![],
        },
    );

    // All tools get beta incremented
    assert!(val.tools["t1"].beta_param > val.tools["t1"].alpha);
    assert_eq!(val.tools["t1"].last_used_turn, None);
}

// ---------------------------------------------------------------------------
// AgentToolPriors tests
// ---------------------------------------------------------------------------

#[test]
fn agent_tool_priors_key_constant() {
    assert_eq!(AgentToolPriorsKey::KEY, "deferred_tools.agent_priors");
}

#[test]
fn agent_tool_priors_default() {
    let priors = AgentToolPriors::default();
    assert!(priors.tools.is_empty());
    assert_eq!(priors.session_count, 0);
}

#[test]
fn agent_tool_priors_serde_roundtrip() {
    let mut priors = AgentToolPriors::default();
    priors.tools.insert("Bash".into(), 0.32);
    priors.tools.insert("mcp__query".into(), 0.017);
    priors.session_count = 42;

    let json = serde_json::to_string(&priors).unwrap();
    let decoded: AgentToolPriors = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.session_count, 42);
    assert!((decoded.tools["Bash"] - 0.32).abs() < 1e-10);
    assert!((decoded.tools["mcp__query"] - 0.017).abs() < 1e-10);
}

#[test]
fn agent_tool_priors_profile_key_encode_decode() {
    let mut priors = AgentToolPriors::default();
    priors.tools.insert("Read".into(), 0.5);
    priors.session_count = 3;

    let encoded = AgentToolPriorsKey::encode(&priors).unwrap();
    let decoded = AgentToolPriorsKey::decode(encoded).unwrap();
    assert_eq!(decoded, priors);
}

#[test]
fn agent_tool_priors_ewma_update() {
    // Simulate one EWMA update iteration
    let mut priors = AgentToolPriors {
        tools: [("Bash".into(), 0.30)].into_iter().collect(),
        session_count: 10,
    };
    let session_p = 0.50_f64;
    let lambda = 0.1_f64.max(1.0 / (priors.session_count as f64 + 1.0));
    let prior_p = priors.tools["Bash"];
    let updated = (1.0 - lambda) * prior_p + lambda * session_p;
    priors.tools.insert("Bash".into(), updated);
    priors.session_count += 1;

    // With λ=0.1: updated = 0.9*0.30 + 0.1*0.50 = 0.27 + 0.05 = 0.32
    assert!((priors.tools["Bash"] - 0.32).abs() < 1e-10);
    assert_eq!(priors.session_count, 11);
}

#[test]
fn agent_tool_priors_ewma_lambda_stabilises() {
    // For session_count >= 9, lambda should stay at 0.1
    let session_count_high = 100_u64;
    let lambda = 0.1_f64.max(1.0 / (session_count_high as f64 + 1.0));
    assert!((lambda - 0.1).abs() < 1e-10);

    // For session_count = 0 (first session), lambda = 1.0
    let lambda_first = 0.1_f64.max(1.0 / (0_f64 + 1.0));
    assert!((lambda_first - 1.0).abs() < 1e-10);
}
