use awaken_contract::registry_spec::{AgentSpec, McpServerSpec, ModelBindingSpec, ProviderSpec};
use awaken_contract::{BuiltinSeedSet, BuiltinSpec, SkillSpec};

// ── helpers ──────────────────────────────────────────────────────────────────

fn agent_spec() -> AgentSpec {
    // `Default::default()` already applies the legacy "allow all" shim
    // (`allowed_tool_patterns = ["*"]`), so this matches what the JSON
    // deserialize round-trip produces without an explicit opt-in.
    AgentSpec {
        id: "test-agent".into(),
        model_id: "m".into(),
        system_prompt: "p".into(),
        ..Default::default()
    }
}

fn provider_spec() -> ProviderSpec {
    ProviderSpec {
        id: "openai".into(),
        adapter: "openai".into(),
        ..Default::default()
    }
}

fn model_binding_spec() -> ModelBindingSpec {
    ModelBindingSpec {
        id: "gpt-4o".into(),
        provider_id: "openai".into(),
        upstream_model: "gpt-4o-mini".into(),
    }
}

fn mcp_server_spec() -> McpServerSpec {
    McpServerSpec {
        id: "test-mcp".into(),
        ..Default::default()
    }
}

fn skill_spec() -> SkillSpec {
    SkillSpec {
        id: "db-management".into(),
        name: "Database Management".into(),
        description: "Helps manage database work".into(),
        instructions_md: "Inspect schema before running SQL.".into(),
        ..Default::default()
    }
}

// ── namespace() ──────────────────────────────────────────────────────────────

#[test]
fn namespace_returns_expected_string_for_each_variant() {
    assert_eq!(
        BuiltinSpec::Agent(Box::new(agent_spec())).namespace(),
        "agents"
    );
    assert_eq!(
        BuiltinSpec::Provider(provider_spec()).namespace(),
        "providers"
    );
    assert_eq!(
        BuiltinSpec::Model(model_binding_spec()).namespace(),
        "models"
    );
    assert_eq!(
        BuiltinSpec::McpServer(mcp_server_spec()).namespace(),
        "mcp-servers"
    );
    assert_eq!(BuiltinSpec::Skill(skill_spec()).namespace(), "skills");
}

// ── id() ─────────────────────────────────────────────────────────────────────

#[test]
fn id_delegates_to_inner_spec_for_each_variant() {
    assert_eq!(
        BuiltinSpec::Agent(Box::new(agent_spec())).id(),
        "test-agent"
    );
    assert_eq!(BuiltinSpec::Provider(provider_spec()).id(), "openai");
    assert_eq!(BuiltinSpec::Model(model_binding_spec()).id(), "gpt-4o");
    assert_eq!(BuiltinSpec::McpServer(mcp_server_spec()).id(), "test-mcp");
    assert_eq!(BuiltinSpec::Skill(skill_spec()).id(), "db-management");
}

// ── serde round-trip ─────────────────────────────────────────────────────────

#[test]
fn serde_roundtrip_agent() {
    let original = BuiltinSpec::Agent(Box::new(agent_spec()));
    let value = serde_json::to_value(&original).unwrap();
    let decoded: BuiltinSpec = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        value,
        serde_json::to_value(&decoded).unwrap(),
        "Agent round-trip mismatch"
    );
}

#[test]
fn serde_roundtrip_provider() {
    let original = BuiltinSpec::Provider(provider_spec());
    let value = serde_json::to_value(&original).unwrap();
    let decoded: BuiltinSpec = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        value,
        serde_json::to_value(&decoded).unwrap(),
        "Provider round-trip mismatch"
    );
}

#[test]
fn serde_roundtrip_model() {
    let original = BuiltinSpec::Model(model_binding_spec());
    let value = serde_json::to_value(&original).unwrap();
    let decoded: BuiltinSpec = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        value,
        serde_json::to_value(&decoded).unwrap(),
        "Model round-trip mismatch"
    );
}

#[test]
fn serde_roundtrip_mcp_server() {
    let original = BuiltinSpec::McpServer(mcp_server_spec());
    let value = serde_json::to_value(&original).unwrap();
    let decoded: BuiltinSpec = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        value,
        serde_json::to_value(&decoded).unwrap(),
        "McpServer round-trip mismatch"
    );
}

#[test]
fn serde_roundtrip_skill() {
    let original = BuiltinSpec::Skill(skill_spec());
    let value = serde_json::to_value(&original).unwrap();
    let decoded: BuiltinSpec = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        value,
        serde_json::to_value(&decoded).unwrap(),
        "Skill round-trip mismatch"
    );
}

// ── tag discriminator ────────────────────────────────────────────────────────

#[test]
fn tag_discriminator_is_kind_with_snake_case_names() {
    let agent_value = serde_json::to_value(BuiltinSpec::Agent(Box::new(agent_spec()))).unwrap();
    assert_eq!(
        agent_value["kind"].as_str(),
        Some("agent"),
        "Agent kind tag mismatch"
    );

    let mcp_value = serde_json::to_value(BuiltinSpec::McpServer(mcp_server_spec())).unwrap();
    assert_eq!(
        mcp_value["kind"].as_str(),
        Some("mcp_server"),
        "McpServer kind tag mismatch"
    );

    let provider_value = serde_json::to_value(BuiltinSpec::Provider(provider_spec())).unwrap();
    assert_eq!(
        provider_value["kind"].as_str(),
        Some("provider"),
        "Provider kind tag mismatch"
    );

    let model_value = serde_json::to_value(BuiltinSpec::Model(model_binding_spec())).unwrap();
    assert_eq!(
        model_value["kind"].as_str(),
        Some("model"),
        "Model kind tag mismatch"
    );

    let skill_value = serde_json::to_value(BuiltinSpec::Skill(skill_spec())).unwrap();
    assert_eq!(
        skill_value["kind"].as_str(),
        Some("skill"),
        "Skill kind tag mismatch"
    );
}

// ── mixed-variant Vec ────────────────────────────────────────────────────────

#[test]
fn mixed_variant_vec_round_trips() {
    let specs = vec![
        BuiltinSpec::Agent(Box::new(agent_spec())),
        BuiltinSpec::Provider(provider_spec()),
        BuiltinSpec::Model(model_binding_spec()),
        BuiltinSpec::McpServer(mcp_server_spec()),
        BuiltinSpec::Skill(skill_spec()),
    ];
    let original_value = serde_json::to_value(&specs).unwrap();
    let decoded: Vec<BuiltinSpec> = serde_json::from_value(original_value.clone()).unwrap();
    assert_eq!(
        original_value,
        serde_json::to_value(&decoded).unwrap(),
        "mixed-variant Vec round-trip mismatch"
    );
}

// ── BuiltinSeedSet::empty ────────────────────────────────────────────────────

#[test]
fn builtin_seed_set_empty_constructs_valid_empty_seed() {
    let seed = BuiltinSeedSet::empty("1.2.3");
    assert_eq!(seed.binary_version, "1.2.3");
    assert!(seed.specs.is_empty());
}
