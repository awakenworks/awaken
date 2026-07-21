//! ADR-0057 phase 0 (`pin-invariants`): characterization tests that FREEZE the
//! "byte-identical / behavior-identical" promises before any unified-config
//! refactor touches these paths. A later phase that adds `kind`, renames a field,
//! or reorders serialization must keep every assertion here green — if it cannot,
//! the change is a wire/fingerprint break and must be a deliberate, versioned one.
//!
//! What is pinned:
//!  1. The publication fingerprint of representative configs (golden SHA-256).
//!  2. The wire shape: `Pinned` serializes as the flat model triple, `Auto` as
//!     `{"mode":"auto"}`; empty optional axes are omitted (skip_serializing_if).
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

/// A representative config authored the way today's plane authors one. `backend`
/// selects the kind through the historic `backend_ref` string (`"genai"` native,
/// `"acp:<cli>"`, `"a2a:<endpoint>"`) — the exact surface the refactor must keep
/// byte-stable.
fn config(id: &str, backend: &str) -> AgentConfig {
    AgentConfig {
        id: id.to_string(),
        instructions: "be helpful".to_string(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("p", "claude-opus-4-8", backend),
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
        "73a169c220424fbe417b5a615272e1af89c61ac4a8b173444fb509f467643785",
        "native config fingerprint drifted"
    );
    assert_eq!(
        fingerprint(&config("agent-acp", "acp:claude")),
        "0decc48fdedb4b30f4ad34d37a289053b52c16094473cca8438be45d851fb777",
        "acp config fingerprint drifted"
    );
    assert_eq!(
        fingerprint(&config("agent-a2a", "a2a:https://remote.example/agent")),
        "ad7c4b7a66c307cb95cd7bd42de6846eaad34bcf8f9ca9aa321f7f49c9456372",
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

/// A `Pinned` model selection serializes as the bare flat triple (the historic
/// shape), NOT a tagged variant — every pre-existing config depends on this.
#[test]
fn pinned_model_serializes_as_the_flat_triple() {
    let v: Value = serde_json::to_value(config("agent-1", "acp:claude")).unwrap();
    let mb = &v["model_binding"];
    assert_eq!(mb["provider_identity_ref"], "p");
    assert_eq!(mb["model_ref"], "claude-opus-4-8");
    assert_eq!(mb["backend_ref"], "acp:claude");
    assert!(
        mb.get("mode").is_none(),
        "a Pinned binding must not carry a mode tag"
    );
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
    full.skills = vec![json!({"id": "review"})];
    full.mcp_servers = vec![json!({"name": "gh", "url": "https://mcp.example"})];

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

/// The `plugin_config["acp"]` section round-trips and reads back by the exact
/// path today's executor uses (`compact_window`, `mcp_servers`). The future
/// `AcpSpec::{from,into}_plugin_config` codec must preserve this byte-for-byte.
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
    let acp = round
        .plugin_config
        .get("acp")
        .expect("acp section preserved");
    assert_eq!(acp["compact_window"], 120_000);
    assert_eq!(acp["mcp_servers"][0]["name"], "gh");
    assert_eq!(cfg, round);
}
