//! Model-selection and publication content-address invariants.
//!
//! What is pinned:
//!  1. The publication fingerprint of representative configs (golden SHA-256).
//!  2. Every selection serializes with one explicit `mode`; empty optional axes
//!     are omitted (skip_serializing_if).
//!  3. Lossless JSON round-trip of every kind expressed via `backend_ref`.
//!  4. The `plugin_config["acp"]` section round-trips and reads back by path
//!     (the data `AcpSpec::{from,into}_plugin_config` must preserve).

use std::collections::BTreeMap;

use awaken_config_store::{AgentConfig, ModelSelection, compile_resolved};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::snapshot::AgentSnapshotMetadata;
use serde_json::{Value, json};

fn compile(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
) -> Result<awaken_config_store::ExecutableAgentSnapshot, awaken_config_store::CompileError> {
    compile_resolved(config, tools, AgentSnapshotMetadata::default())
}

/// A representative config authored through the canonical selection contract.
fn config(id: &str, backend: &str) -> AgentConfig {
    AgentConfig {
        id: id.to_string(),
        instructions: "be helpful".to_string(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("p", "claude-opus-4-8", backend),
        inference: Default::default(),
        tool_ids: vec!["echo".to_string()],
        ..Default::default()
    }
}

fn echo_tool() -> ToolDescriptor {
    ToolDescriptor::pinned("test", "echo", "a tool", json!({"type": "object"}))
}

fn fingerprint(cfg: &AgentConfig) -> String {
    compile(cfg, &[echo_tool()]).unwrap().fingerprint.0.clone()
}

// ─── 1. Golden fingerprints ──────────────────────────────────────────────────

/// The content address of each representative config is FROZEN. A refactor that
/// changes any config's serialized bytes changes its hash and breaks this — the
/// tripwire for "pre-existing configs keep their publication identity".
#[test]
fn golden_publication_fingerprints_are_frozen() {
    // Native (genai), ACP (acp:claude), A2A (a2a:endpoint) — all via backend_ref.
    assert_eq!(
        fingerprint(&config("agent-native", "genai")),
        "150f6344b5b5055fc1ac417b38a66a0585e845af603bfbc8da423c608a086ea7",
        "native config fingerprint drifted"
    );
    assert_eq!(
        fingerprint(&config("agent-acp", "acp:claude")),
        "0bfc760559cf5cc90ea9f8ccf7651ae9d3e53ba777a6e686456e144b4dc905f3",
        "acp config fingerprint drifted"
    );
    assert_eq!(
        fingerprint(&config("agent-a2a", "a2a:https://remote.example/agent")),
        "c8b3b88e453888c2765ba933966c73b7ac483dbe303f49b0d97635998933ee36",
        "a2a config fingerprint drifted"
    );
}

/// Identity fields (name/description/metadata) are excluded from the fingerprint:
/// setting them must NOT change the content address (they are authoring metadata,
/// not behavior).
#[test]
fn identity_fields_do_not_enter_the_fingerprint() {
    let bare = config("agent-1", "genai");
    let mut adorned = bare.clone();
    adorned.name = Some("Helper".into());
    adorned.description = Some("a helpful agent".into());
    adorned.metadata = BTreeMap::from([("team".into(), "core".into())]);
    assert_eq!(
        fingerprint(&bare),
        fingerprint(&adorned),
        "identity metadata must not change the publication identity"
    );
}

/// The fingerprint is deterministic across compiles of an equal config.
#[test]
fn fingerprint_is_deterministic() {
    assert_eq!(
        fingerprint(&config("agent-1", "genai")),
        fingerprint(&config("agent-1", "genai")),
    );
}

// ─── 2. Wire shape ───────────────────────────────────────────────────────────

/// A `Pinned` model selection uses the same explicit discriminator as every
/// policy variant.
#[test]
fn pinned_model_serializes_as_mode_pinned() {
    let v: Value = serde_json::to_value(config("agent-1", "acp:claude")).unwrap();
    let mb = &v["model_binding"];
    assert_eq!(mb["provider_identity_ref"], "p");
    assert_eq!(mb["model_ref"], "claude-opus-4-8");
    assert_eq!(mb["backend_ref"], "acp:claude");
    assert_eq!(mb["mode"], "pinned");
}

/// An `Auto` model selection serializes as `{"mode":"auto"}`.
#[test]
fn auto_model_serializes_as_mode_auto() {
    let mut cfg = config("agent-1", "genai");
    cfg.model_binding = ModelSelection::Auto;
    let v: Value = serde_json::to_value(&cfg).unwrap();
    assert_eq!(v["model_binding"], json!({"mode": "auto"}));
}

/// Empty optional axes are omitted from the wire (skip_serializing_if), so a
/// minimal config's bytes — and thus fingerprint — stay identical as new optional
/// axes are appended.
#[test]
fn empty_optional_axes_are_absent_from_the_wire() {
    let v: Value = serde_json::to_value(config("agent-1", "genai")).unwrap();
    let obj = v.as_object().unwrap();
    for absent in [
        "tool_patterns",
        "model_candidates",
        "tool_overrides",
        "mcp_servers",
        "skills",
        "multiagent",
        "name",
        "description",
        "metadata",
    ] {
        assert!(
            !obj.contains_key(absent),
            "empty `{absent}` must be omitted from the wire"
        );
    }
}

// ─── 3. Lossless round-trip ──────────────────────────────────────────────────

/// Every kind (expressed via backend_ref), including one carrying skills / MCP /
/// plugin_config, round-trips through JSON with no loss — the property a field
/// addition must not break.
#[test]
fn configs_round_trip_through_json_losslessly() {
    let mut full = config("agent-full", "acp:codex");
    full.plugin_ids = vec!["mcp".into()];
    full.plugin_config = BTreeMap::from([(
        "acp".into(),
        json!({ "compact_window": 120_000, "mcp_servers": [{"name": "gh"}] }),
    )]);
    full.skills = vec![awaken_agent_contract::AgentSkillBinding::custom("review")];
    full.mcp_servers = vec![
        awaken_runtime_contract::agent_bindings::AgentMcpServerBinding {
            name: "gh".into(),
            transport: awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::http(
                "https://mcp.example",
            ),
            credential: None,
            prompts_as_skills: false,
        },
    ];

    for cfg in [
        config("agent-native", "genai"),
        config("agent-acp", "acp:claude"),
        config("agent-a2a", "a2a:https://remote.example/agent"),
        full,
    ] {
        let round: AgentConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            cfg, round,
            "config `{}` did not round-trip losslessly",
            cfg.id
        );
    }
}

// ─── 4. ACP plugin_config section (what AcpSpec codec must preserve) ──────────

/// The `plugin_config["acp"]` section round-trips through the sole typed codec.
/// Historical route shapes that this version cannot interpret remain byte-stable.
#[test]
fn acp_plugin_config_section_round_trips_and_reads_by_path() {
    let mut cfg = config("agent-acp", "acp:claude");
    cfg.plugin_config = BTreeMap::from([(
        "acp".into(),
        json!({
            "compact_window": 120_000,
            "mcp_servers": [{"name": "gh", "url": "https://mcp.example"}],
        }),
    )]);

    let round: AgentConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
    let spec = awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(&round.plugin_config);
    assert_eq!(spec.compact_window, Some(120_000));
    assert_eq!(
        spec.into_plugin_config(round.plugin_config.clone()),
        round.plugin_config
    );
    assert_eq!(cfg, round);
}
