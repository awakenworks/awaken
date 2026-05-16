use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;
use crate::registry::resolve::pipeline::catalog::{
    CATALOG_WILDCARD_AUDIT_WARN_CACHE_LIMIT, LEGACY_ALLOW_ALL_WARN_CACHE_LIMIT,
    catalog_pattern_matches, has_unescaped_catalog_wildcard, is_argument_level_catalog_pattern,
    is_argument_syntax_for_registered_tool, permission_rules_without_catalog_match,
    should_warn_catalog_wildcard_entry, should_warn_legacy_allow_all, unmatched_catalog_patterns,
};

#[test]
fn resolve_allow_list_supports_glob_patterns() {
    let spec = AgentSpec {
        allowed_tools: Some(vec!["read_*".into(), "mcp__github__*".into()]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![
            (
                "read_file",
                Arc::new(MockTool {
                    id: "read_file".into(),
                }),
            ),
            (
                "read_url",
                Arc::new(MockTool {
                    id: "read_url".into(),
                }),
            ),
            (
                "write_file",
                Arc::new(MockTool {
                    id: "write_file".into(),
                }),
            ),
            (
                "mcp__github__pr",
                Arc::new(MockTool {
                    id: "mcp__github__pr".into(),
                }),
            ),
            (
                "mcp__gitlab__pr",
                Arc::new(MockTool {
                    id: "mcp__gitlab__pr".into(),
                }),
            ),
        ],
        "test-model",
        ModelBinding {
            provider_id: "p".into(),
            upstream_model: "n".into(),
        },
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(
        run.tools.len(),
        3,
        "tools kept: {:?}",
        run.tools.keys().collect::<Vec<_>>()
    );
    assert!(run.tools.contains_key("read_file"));
    assert!(run.tools.contains_key("read_url"));
    assert!(run.tools.contains_key("mcp__github__pr"));
    assert!(!run.tools.contains_key("write_file"));
    assert!(!run.tools.contains_key("mcp__gitlab__pr"));
}

#[test]
fn resolve_allow_list_wildcard_keeps_all() {
    let spec = AgentSpec {
        allowed_tools: Some(vec!["*".into()]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![
            ("read", Arc::new(MockTool { id: "read".into() })),
            ("write", Arc::new(MockTool { id: "write".into() })),
            (
                "delete",
                Arc::new(MockTool {
                    id: "delete".into(),
                }),
            ),
        ],
        "test-model",
        ModelBinding {
            provider_id: "p".into(),
            upstream_model: "n".into(),
        },
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.tools.len(), 3, "[\"*\"] should keep every tool");
}

#[test]
fn legacy_allow_all_warning_is_once_per_agent() {
    let warned_agents = Mutex::new(VecDeque::new());

    assert!(should_warn_legacy_allow_all("agent-a", &warned_agents));
    assert!(!should_warn_legacy_allow_all("agent-a", &warned_agents));
    assert!(should_warn_legacy_allow_all("agent-b", &warned_agents));
}

#[test]
fn legacy_allow_all_warning_cache_is_bounded() {
    let warned_agents = Mutex::new(VecDeque::new());

    for i in 0..(LEGACY_ALLOW_ALL_WARN_CACHE_LIMIT + 1) {
        assert!(should_warn_legacy_allow_all(
            &format!("agent-{i}"),
            &warned_agents
        ));
    }

    let warned_agents = warned_agents.lock().unwrap();
    assert_eq!(warned_agents.len(), LEGACY_ALLOW_ALL_WARN_CACHE_LIMIT);
    assert!(!warned_agents.iter().any(|agent_id| agent_id == "agent-0"));
}

#[test]
fn catalog_tool_id_match_handles_basic_wildcards() {
    let cases = [
        ("Bash", "Bash", true),
        ("Bash", "Read", false),
        ("Bash", "BashExtra", false),
        ("*", "Bash", true),
        ("*", "mcp:weather/forecast", true),
        ("mcp:*", "mcp:weather/forecast", true),
        ("mcp:*", "plugin:reminder/add", false),
        ("mcp:weather/*", "mcp:weather/forecast", true),
        ("mcp:weather/*", "mcp:weather/foo/bar", true),
        ("mcp__github__*", "mcp__github__read_issue", true),
        ("*issue", "mcp__github__read_issue", true),
        ("a*b", "a/b", true),
        ("a*/b", "a/x/b", true),
        ("\\*literal", "*literal", true),
        ("\\*literal", "Xliteral", false),
        ("mcp__github__read?", "mcp__github__read1", false),
        ("mcp__github__read?", "mcp__github__read?", true),
        ("mcp__[ab]*", "mcp__a_tool", false),
        ("{Bash}", "Bash", false),
        ("{Bash}", "{Bash}", true),
        ("!Bash", "!Bash", true),
        ("!Bash", "Bash", false),
        ("/B.*/", "Bash", false),
        ("Bash(npm *)", "Bash", false),
    ];

    for (pattern, value, expected) in cases {
        assert_eq!(
            catalog_pattern_matches(&[pattern.to_string()], value),
            expected,
            "pattern={pattern:?} value={value:?}"
        );
    }
}

#[test]
fn catalog_argument_pattern_detection_flags_permission_style_args() {
    assert!(!is_argument_level_catalog_pattern("Bash"));
    assert!(!is_argument_level_catalog_pattern("mcp__github__*"));
    assert!(!is_argument_level_catalog_pattern("literal(paren)"));
    assert!(!is_argument_level_catalog_pattern("Bash(npm *"));
    assert!(is_argument_level_catalog_pattern("Bash(npm *)"));
    assert!(is_argument_level_catalog_pattern(
        r#"Edit(file_path ~ "src/**/*.rs")"#
    ));
}

#[test]
fn registered_tool_argument_syntax_detection_flags_simple_args() {
    let tool_ids = vec!["Bash".to_string(), "literal(paren)".to_string()];

    assert!(is_argument_syntax_for_registered_tool(
        r#"Bash(= "ls")"#,
        &tool_ids
    ));
    assert!(is_argument_syntax_for_registered_tool(
        "Bash(npm)",
        &tool_ids
    ));
    assert!(is_argument_syntax_for_registered_tool(
        r#"Bash(command = "ls")"#,
        &tool_ids
    ));
    assert!(!is_argument_syntax_for_registered_tool(
        "Bash(*)", &tool_ids
    ));
    assert!(!is_argument_syntax_for_registered_tool(
        "literal(paren)",
        &tool_ids
    ));
    assert!(!is_argument_syntax_for_registered_tool(
        "Other(npm)",
        &tool_ids
    ));
}

#[test]
fn unmatched_catalog_patterns_returns_typos() {
    let patterns = vec![
        "read_*".to_string(),
        "mcp_github_*".to_string(),
        "Bash(npm *)".to_string(),
    ];
    let tool_ids = vec!["read_file".to_string(), "mcp__github__issue".to_string()];

    assert_eq!(
        unmatched_catalog_patterns(&patterns, &tool_ids),
        vec!["mcp_github_*".to_string()]
    );
}

#[test]
fn unmatched_catalog_patterns_skips_registered_tool_argument_syntax() {
    let patterns = vec![
        r#"Bash(= "ls")"#.to_string(),
        "Bash(npm)".to_string(),
        r#"Bash(command = "ls")"#.to_string(),
        "literal(paren)".to_string(),
    ];
    let tool_ids = vec!["Bash".to_string(), "literal(paren)".to_string()];

    assert!(unmatched_catalog_patterns(&patterns, &tool_ids).is_empty());
}

#[test]
fn catalog_wildcard_audit_detects_unescaped_stars() {
    assert!(has_unescaped_catalog_wildcard("mcp:*"));
    assert!(has_unescaped_catalog_wildcard("tool*id"));
    assert!(!has_unescaped_catalog_wildcard(r"tool\*id"));
    assert!(!has_unescaped_catalog_wildcard("Bash"));
}

#[test]
fn catalog_wildcard_audit_warning_cache_is_bounded() {
    let warned = Mutex::new(VecDeque::new());
    assert!(should_warn_catalog_wildcard_entry(
        "agent\0allowed_tools\0mcp:*",
        &warned
    ));
    assert!(!should_warn_catalog_wildcard_entry(
        "agent\0allowed_tools\0mcp:*",
        &warned
    ));

    for n in 0..(CATALOG_WILDCARD_AUDIT_WARN_CACHE_LIMIT + 1) {
        assert!(should_warn_catalog_wildcard_entry(
            &format!("agent\0allowed_tools\0pattern-{n}*"),
            &warned,
        ));
    }
    assert_eq!(
        warned
            .lock()
            .expect("test catalog wildcard warning cache poisoned")
            .len(),
        CATALOG_WILDCARD_AUDIT_WARN_CACHE_LIMIT
    );
}

#[test]
fn unmatched_catalog_patterns_skips_empty_catalogs() {
    let patterns = vec!["read_*".to_string()];
    let tool_ids = Vec::new();

    assert!(unmatched_catalog_patterns(&patterns, &tool_ids).is_empty());
}

#[test]
fn permission_rule_orphan_detected_when_tool_not_in_catalog() {
    let mut sections = HashMap::new();
    sections.insert(
        "permission".into(),
        json!({
            "rules": [
                { "tool": "Bash(npm *)", "behavior": "ask" },
                { "tool": "read_file", "behavior": "allow" },
            ]
        }),
    );

    let retained = vec!["read_file".to_string(), "Edit".to_string()];
    let orphans = permission_rules_without_catalog_match(&sections, &retained);
    assert_eq!(orphans, vec!["Bash(npm *)".to_string()]);
}

#[test]
fn permission_rule_orphan_glob_pattern_matches_catalog() {
    let mut sections = HashMap::new();
    sections.insert(
        "permission".into(),
        json!({
            "rules": [
                { "tool": "mcp__github__*", "behavior": "ask" },
            ]
        }),
    );

    let retained = vec!["mcp__github__pr".to_string()];
    let orphans = permission_rules_without_catalog_match(&sections, &retained);
    assert!(orphans.is_empty(), "glob pattern should match catalog tool");
}

#[test]
fn permission_rule_orphan_no_permission_section_is_noop() {
    let sections = HashMap::new();
    let retained = vec!["Bash".to_string()];
    assert!(permission_rules_without_catalog_match(&sections, &retained).is_empty());
}

#[test]
fn permission_rule_orphan_unparseable_pattern_skipped() {
    let mut sections = HashMap::new();
    sections.insert(
        "permission".into(),
        json!({
            "rules": [
                { "tool": "((((invalid", "behavior": "ask" },
            ]
        }),
    );

    let retained = vec!["Bash".to_string()];
    let orphans = permission_rules_without_catalog_match(&sections, &retained);
    assert!(orphans.is_empty());
}

#[test]
fn resolve_exclude_list_supports_glob_patterns() {
    let spec = AgentSpec {
        excluded_tools: Some(vec!["mcp__gitlab__*".into()]),
        ..make_spec("a")
    };

    let regs = build_registries(
        vec![
            ("read", Arc::new(MockTool { id: "read".into() })),
            (
                "mcp__github__pr",
                Arc::new(MockTool {
                    id: "mcp__github__pr".into(),
                }),
            ),
            (
                "mcp__gitlab__pr",
                Arc::new(MockTool {
                    id: "mcp__gitlab__pr".into(),
                }),
            ),
            (
                "mcp__gitlab__merge",
                Arc::new(MockTool {
                    id: "mcp__gitlab__merge".into(),
                }),
            ),
        ],
        "test-model",
        ModelBinding {
            provider_id: "p".into(),
            upstream_model: "n".into(),
        },
        "p",
        Arc::new(MockExecutor),
        vec![],
        spec,
    );

    let run = resolve(&regs, "a").unwrap();
    assert_eq!(run.tools.len(), 2);
    assert!(run.tools.contains_key("read"));
    assert!(run.tools.contains_key("mcp__github__pr"));
    assert!(!run.tools.contains_key("mcp__gitlab__pr"));
    assert!(!run.tools.contains_key("mcp__gitlab__merge"));
}
