use std::collections::HashMap;

use awaken_contract::PluginConfigKey;

use crate::config::*;

#[test]
fn default_config_has_auto_enable() {
    let config = DeferredToolsConfig::default();
    assert!(config.enabled.is_none());
    assert!(config.rules.is_empty());
    assert!((config.beta_overhead - 1136.0).abs() < f64::EPSILON);
}

#[test]
fn parse_json_config() {
    let json = r#"{
        "enabled": true,
        "rules": [
            {"tool": "Bash", "mode": "eager"},
            {"tool": "Read", "mode": "eager"},
            {"tool": "Edit", "mode": "eager"},
            {"tool": "Grep", "mode": "eager"},
            {"tool": "mcp__*", "mode": "deferred"},
            {"tool": "Web*", "mode": "deferred"},
            {"tool": "Task*", "mode": "deferred"}
        ]
    }"#;
    let config: DeferredToolsConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.enabled, Some(true));
    assert_eq!(config.rules.len(), 7);
    assert_eq!(
        config
            .rules
            .iter()
            .filter(|r| r.mode == ToolLoadMode::Eager)
            .count(),
        4
    );
    assert_eq!(
        config
            .rules
            .iter()
            .filter(|r| r.mode == ToolLoadMode::Deferred)
            .count(),
        3
    );
}

#[test]
fn parse_json_config_without_enabled() {
    let json = r#"{ "rules": [{"tool": "Bash", "mode": "eager"}] }"#;
    let config: DeferredToolsConfig = serde_json::from_str(json).unwrap();
    assert!(config.enabled.is_none());
}

#[test]
fn serde_roundtrip() {
    let config = DeferredToolsConfig {
        enabled: Some(true),
        rules: vec![
            DeferralRule {
                tool: "Read".into(),
                mode: ToolLoadMode::Eager,
            },
            DeferralRule {
                tool: "Bash".into(),
                mode: ToolLoadMode::Eager,
            },
            DeferralRule {
                tool: "mcp__*".into(),
                mode: ToolLoadMode::Deferred,
            },
        ],
        ..Default::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    let decoded: DeferredToolsConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.enabled, Some(true));
    assert_eq!(
        decoded
            .rules
            .iter()
            .filter(|r| r.mode == ToolLoadMode::Eager)
            .count(),
        2
    );
}

#[test]
fn config_key_binding() {
    assert_eq!(DeferredToolsConfigKey::KEY, "deferred_tools");
}

#[test]
fn resolve_mode_eager_rules() {
    let config = DeferredToolsConfig {
        rules: vec![
            DeferralRule {
                tool: "Bash".into(),
                mode: ToolLoadMode::Eager,
            },
            DeferralRule {
                tool: "Read".into(),
                mode: ToolLoadMode::Eager,
            },
        ],
        ..Default::default()
    };
    assert_eq!(config.resolve_mode("Bash"), ToolLoadMode::Eager);
    assert_eq!(config.resolve_mode("Read"), ToolLoadMode::Eager);
    // Unspecified tools are deferred by default
    assert_eq!(config.resolve_mode("mcp__query"), ToolLoadMode::Deferred);
    assert_eq!(config.resolve_mode("UnknownTool"), ToolLoadMode::Deferred);
}

#[test]
fn resolve_mode_exact_match() {
    let config = DeferredToolsConfig {
        rules: vec![
            DeferralRule {
                tool: "Bash".into(),
                mode: ToolLoadMode::Eager,
            },
            DeferralRule {
                tool: "mcp__reverts__query".into(),
                mode: ToolLoadMode::Eager,
            },
        ],
        ..Default::default()
    };
    // Exact match
    assert_eq!(
        config.resolve_mode("mcp__reverts__query"),
        ToolLoadMode::Eager
    );
    // Other MCP tools are deferred
    assert_eq!(
        config.resolve_mode("mcp__reverts__get_source"),
        ToolLoadMode::Deferred
    );
}

#[test]
fn resolve_mode_empty_rules_defers_all() {
    let config = DeferredToolsConfig::default();
    assert_eq!(config.resolve_mode("Bash"), ToolLoadMode::Deferred);
    assert_eq!(config.resolve_mode("anything"), ToolLoadMode::Deferred);
}

#[test]
fn tool_load_mode_default_is_deferred() {
    assert_eq!(ToolLoadMode::default(), ToolLoadMode::Deferred);
}

// --- should_enable tests ---

#[test]
fn should_enable_forced_true() {
    let config = DeferredToolsConfig {
        enabled: Some(true),
        ..Default::default()
    };
    assert!(config.should_enable(0.0));
}

#[test]
fn should_enable_forced_false() {
    let config = DeferredToolsConfig {
        enabled: Some(false),
        ..Default::default()
    };
    assert!(!config.should_enable(99999.0));
}

#[test]
fn should_enable_auto_above_threshold() {
    let config = DeferredToolsConfig {
        enabled: None,
        beta_overhead: 1000.0,
        ..Default::default()
    };
    assert!(config.should_enable(1500.0));
}

#[test]
fn should_enable_auto_below_threshold() {
    let config = DeferredToolsConfig {
        enabled: None,
        beta_overhead: 1000.0,
        ..Default::default()
    };
    assert!(!config.should_enable(500.0));
}

// --- prior_p tests ---

#[test]
fn prior_p_from_agent_priors() {
    let mut priors = HashMap::new();
    priors.insert("tool_a".into(), 0.42);
    let config = DeferredToolsConfig {
        agent_priors: priors,
        ..Default::default()
    };
    assert!((config.prior_p("tool_a") - 0.42).abs() < f64::EPSILON);
}

#[test]
fn prior_p_default_for_unknown() {
    let config = DeferredToolsConfig::default();
    assert!((config.prior_p("unknown") - 0.01).abs() < f64::EPSILON);
}

// --- DiscBetaParams tests ---

#[test]
fn disc_beta_params_defaults() {
    let params = DiscBetaParams::default();
    assert!((params.omega - 0.95).abs() < f64::EPSILON);
    assert!((params.n0 - 5.0).abs() < f64::EPSILON);
    assert_eq!(params.defer_after, 5);
    assert!((params.thresh_mult - 0.5).abs() < f64::EPSILON);
    assert!((params.gamma - 2000.0).abs() < f64::EPSILON);
}

#[test]
fn disc_beta_params_serde_roundtrip() {
    let params = DiscBetaParams {
        omega: 0.9,
        n0: 3.0,
        defer_after: 10,
        thresh_mult: 0.8,
        gamma: 1500.0,
    };
    let json = serde_json::to_string(&params).unwrap();
    let decoded: DiscBetaParams = serde_json::from_str(&json).unwrap();
    assert!((decoded.omega - 0.9).abs() < f64::EPSILON);
    assert_eq!(decoded.defer_after, 10);
}
