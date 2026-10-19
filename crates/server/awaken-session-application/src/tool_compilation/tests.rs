//! Exhaustive boundary table for the sole Profile-over-capability fold.
use super::inherit_profile_tools;
use awaken_agent_contract::{
    ClientToolDescriptor, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
};
use awaken_executable_agent_contract::ExecutableAgentSessionProfile;
use awaken_session_configuration_contract::SessionToolConfiguration;

#[test]
fn profile_tool_inheritance_is_exact_and_empty_clients_never_fall_back() {
    // Causes: C1 Profile absent/present; C2 Profile toolsets empty/nonempty;
    // C3 Profile clients empty/nonempty; C4 capability defaults empty/nonempty.
    // H1 !C1 => both defaults unchanged (C2/C3 not applicable), at both C4 edges.
    // H2 C1+C2(empty)+C3(nonempty) => default sets/exact Profile clients.
    // H3 C1+C2(nonempty)+C3(nonempty) => exact Profile sets and clients.
    // H4 C1+C3(empty), at BOTH C2 edges => clients empty, never host fallback.
    // These six feasible boundary rows exhaust the branch conditions; values
    // have different policies, opaque settings, names, descriptions and schemas
    // so a merge, default-on-empty or policy widening is observable. This
    // branch table makes no additional claim about multi-item list ordering.
    let cap_policy = ToolsetPolicy {
        source: ToolsetSource::Mcp {
            server_name: "capability-server".into(),
        },
        overrides: vec![ToolPolicyOverride::with_optional_configuration(
            "opaque",
            Default::default(),
            Some(serde_json::json!({"owner":"capability"})),
        )],
        default: Default::default(),
    };
    let mut profile_policy = cap_policy.clone();
    profile_policy.source = ToolsetSource::Mcp {
        server_name: "profile-server".into(),
    };
    profile_policy.default.enabled = false;
    profile_policy.overrides[0].configuration = Some(serde_json::json!({"owner":"profile"}));
    let cap_client = ClientToolDescriptor {
        name: "capability-client".into(),
        description: "capability description".into(),
        input_schema: serde_json::json!({"type":"object","properties":{"capability":{"type":"string"}}}),
    };
    let profile_client = ClientToolDescriptor {
        name: "profile-client".into(),
        description: "profile description".into(),
        input_schema: serde_json::json!({"type":"object","properties":{"profile":{"type":"number"}}}),
    };
    let caps = SessionToolConfiguration {
        toolsets: vec![cap_policy.clone()],
        client_tools: vec![cap_client],
    };
    let profile_fallback = ExecutableAgentSessionProfile {
        client_tools: vec![profile_client.clone()],
        ..Default::default()
    };
    let profile_full = ExecutableAgentSessionProfile {
        toolsets: vec![profile_policy.clone()],
        ..profile_fallback.clone()
    };
    let profile_empty = ExecutableAgentSessionProfile::default();
    let profile_sets_only = ExecutableAgentSessionProfile {
        toolsets: vec![profile_policy.clone()],
        ..Default::default()
    };
    for (rule, defaults, profile, expected) in [
        (
            "H1-empty",
            SessionToolConfiguration::default(),
            None,
            SessionToolConfiguration::default(),
        ),
        ("H1-rich", caps.clone(), None, caps.clone()),
        (
            "H2",
            caps.clone(),
            Some(&profile_fallback),
            SessionToolConfiguration {
                toolsets: vec![cap_policy.clone()],
                client_tools: vec![profile_client.clone()],
            },
        ),
        (
            "H3",
            caps.clone(),
            Some(&profile_full),
            SessionToolConfiguration {
                toolsets: vec![profile_policy.clone()],
                client_tools: vec![profile_client],
            },
        ),
        (
            "H4-default-sets",
            caps.clone(),
            Some(&profile_empty),
            SessionToolConfiguration {
                toolsets: vec![cap_policy],
                client_tools: Vec::new(),
            },
        ),
        (
            "H4-profile-sets",
            caps,
            Some(&profile_sets_only),
            SessionToolConfiguration {
                toolsets: vec![profile_policy],
                client_tools: Vec::new(),
            },
        ),
    ] {
        assert_eq!(inherit_profile_tools(defaults, profile), expected, "{rule}");
    }
}
