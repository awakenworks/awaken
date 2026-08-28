//! Wire types for the `agents` resource (`beta.agents.*`): `BetaManagedAgentsAgent`
//! — a reusable, versioned agent configuration (model + system + tools +
//! mcp_servers + skills + multiagent topology) a session instantiates by id.
//!
//! Pure serde shapes. SDK unions are decoded here into tagged Rust enums before
//! the registry normalizes them into its persisted JSON projection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use awaken_session_contract::AgentTool;

use crate::types::{ModelConfig, ModelConfigParams};

/// The one Managed MCP wire shape shared by Agent and Session requests:
/// `{name, type:"url", url}`. Internal sandbox-stdio bindings never become a
/// second public transport variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpServer {
    pub name: String,
    pub url: String,
}

impl AgentMcpServer {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Serialize for AgentMcpServer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("AgentMcpServer", 3)?;
        value.serialize_field("name", &self.name)?;
        value.serialize_field("type", "url")?;
        value.serialize_field("url", &self.url)?;
        value.end()
    }
}

impl<'de> Deserialize<'de> for AgentMcpServer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            name: String,
            url: String,
            #[serde(rename = "type")]
            kind: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.kind != "url" {
            return Err(serde::de::Error::custom("MCP server type must be `url`"));
        }
        Ok(Self {
            name: wire.name,
            url: wire.url,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentSkill {
    Anthropic {
        skill_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    Custom {
        skill_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

impl AgentSkill {
    #[must_use]
    pub fn into_binding(self) -> awaken_agent_contract::AgentSkillBinding {
        let (kind, skill_id, version) = match self {
            Self::Anthropic { skill_id, version } => (
                awaken_agent_contract::AgentSkillKind::Anthropic,
                skill_id,
                version,
            ),
            Self::Custom { skill_id, version } => (
                awaken_agent_contract::AgentSkillKind::Custom,
                skill_id,
                version,
            ),
        };
        awaken_agent_contract::AgentSkillBinding {
            kind,
            skill_id,
            version: version.unwrap_or_else(|| "latest".into()),
        }
    }

    #[must_use]
    pub fn from_binding(binding: awaken_agent_contract::AgentSkillBinding) -> Self {
        match binding.kind {
            awaken_agent_contract::AgentSkillKind::Anthropic => Self::Anthropic {
                skill_id: binding.skill_id,
                version: Some(binding.version),
            },
            awaken_agent_contract::AgentSkillKind::Custom => Self::Custom {
                skill_id: binding.skill_id,
                version: Some(binding.version),
            },
        }
    }

    /// Project the immutable version selected by Session creation. Cold reads
    /// consume this exact pin instead of reopening Runtime capabilities or the
    /// mutable Skill catalog.
    #[must_use]
    pub(crate) fn from_resolved_binding(
        binding: &awaken_session_contract::ResolvedSkillBinding,
    ) -> Self {
        Self::from_binding(awaken_agent_contract::AgentSkillBinding {
            kind: binding.kind,
            skill_id: binding.skill_id.clone(),
            version: binding.version.to_string(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MultiagentRosterEntry {
    Id(String),
    Reference(AgentRosterReference),
    SelfReference(SelfRosterReference),
    Advisor(AdvisorRosterEntry),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvisorRosterEntry {
    pub model: String,
    #[serde(rename = "type")]
    pub kind: AdvisorRosterEntryKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AdvisorRosterEntryKind {
    #[serde(rename = "advisor")]
    Advisor,
}

/// The Claude model families named by the Managed Advisor compatibility table.
///
/// This private vocabulary is the one owner for both save-time executor/advisor
/// admission and client-side Advisor result visibility. Keeping those decisions
/// together prevents a newly admitted redacted model from accidentally using a
/// stale plaintext projection rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedAdvisorModelFamily {
    Haiku45,
    Sonnet46,
    Sonnet5,
    Opus46,
    Opus47,
    Opus48,
    Opus5,
    Fable5,
    Mythos5,
}

fn managed_model_family(model: &str) -> Option<ManagedAdvisorModelFamily> {
    use ManagedAdvisorModelFamily as Family;

    // Awaken's Managed model-id codec may append provider/runtime selectors.
    // Advisor capability is determined by the model head, never by its route.
    let model = model.split(';').next().unwrap_or(model);
    [
        ("claude-haiku-4-5", Family::Haiku45),
        ("claude-sonnet-4-6", Family::Sonnet46),
        ("claude-sonnet-5", Family::Sonnet5),
        ("claude-opus-4-6", Family::Opus46),
        ("claude-opus-4-7", Family::Opus47),
        ("claude-opus-4-8", Family::Opus48),
        ("claude-opus-5", Family::Opus5),
        ("claude-fable-5", Family::Fable5),
        ("claude-mythos-5", Family::Mythos5),
    ]
    .into_iter()
    .find_map(|(canonical, family)| {
        let dated_alias = model
            .strip_prefix(canonical)
            .and_then(|suffix| suffix.strip_prefix('-'))
            .is_some_and(|date| date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()));
        (model == canonical || dated_alias).then_some(family)
    })
}

/// Validate one executor/advisor pair against the Managed Agents surface.
///
/// The underlying Messages Advisor table is narrowed by one Managed-specific
/// rule: Fable 5 is temporarily unavailable in the advisor role. It remains a
/// valid executor, so the exclusion belongs to the advisor side of this table.
#[must_use]
pub fn managed_advisor_pair_supported(executor: &str, advisor: &str) -> bool {
    use ManagedAdvisorModelFamily as Family;

    let (Some(executor), Some(advisor)) = (
        managed_model_family(executor),
        managed_model_family(advisor),
    ) else {
        return false;
    };
    match executor {
        Family::Haiku45 | Family::Sonnet46 | Family::Sonnet5 | Family::Opus46 | Family::Opus47 => {
            matches!(
                advisor,
                Family::Opus47 | Family::Opus48 | Family::Opus5 | Family::Mythos5
            )
        }
        Family::Opus48 => matches!(advisor, Family::Opus48 | Family::Opus5 | Family::Mythos5),
        Family::Opus5 => matches!(advisor, Family::Opus5 | Family::Mythos5),
        Family::Fable5 => matches!(advisor, Family::Opus5),
        Family::Mythos5 => matches!(advisor, Family::Opus5 | Family::Mythos5),
    }
}

/// Whether an Advisor result must be opaque on every Managed client surface.
/// Unknown or malformed families fail closed; only the two documented
/// plaintext-capable advisor families may expose their ordinary public blocks.
pub(crate) fn managed_advisor_result_is_redacted(advisor: &str) -> bool {
    !matches!(
        managed_model_family(advisor),
        Some(ManagedAdvisorModelFamily::Opus47 | ManagedAdvisorModelFamily::Opus48)
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRosterReference {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: AgentRosterReferenceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AgentRosterReferenceKind {
    #[serde(rename = "agent")]
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfRosterReference {
    #[serde(rename = "type")]
    pub kind: SelfRosterReferenceKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SelfRosterReferenceKind {
    #[serde(rename = "self")]
    SelfReference,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MultiagentConfig {
    Coordinator { agents: Vec<MultiagentRosterEntry> },
}

/// A client's `model` input: a bare id string or a full `{id, speed?}` config
/// (the SDK's `string | BetaManagedAgentsModelConfig`). Normalized to the shared
/// [`ModelConfig`] via [`ModelInput::into_config`].
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ModelInput {
    Id(String),
    Config(ModelConfigParams),
}

impl ModelInput {
    pub fn into_config(self) -> ModelConfigParams {
        match self {
            ModelInput::Id(id) => ModelConfigParams::new(id),
            ModelInput::Config(config) => config,
        }
    }
}

/// `AgentCreateParams` — the `POST /v1/agents` body. Every statically-known SDK
/// union is decoded before the repository is called.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCreateParams {
    pub name: String,
    pub model: ModelInput,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub mcp_servers: Vec<AgentMcpServer>,
    #[serde(default)]
    pub skills: Vec<AgentSkill>,
    #[serde(default)]
    pub tools: Vec<AgentTool>,
    #[serde(default)]
    pub multiagent: Option<MultiagentConfig>,
}

/// `AgentUpdateParams` — a partial update under optimistic concurrency: `version`
/// must match the agent's current version. Other fields replace when present.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUpdateParams {
    #[serde(default)]
    pub version: Option<u64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<ModelInput>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub system: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub metadata: Option<Option<BTreeMap<String, Option<String>>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub mcp_servers: Option<Option<Vec<AgentMcpServer>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub skills: Option<Option<Vec<AgentSkill>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub tools: Option<Option<Vec<AgentTool>>>,
    #[serde(default, deserialize_with = "super::presence::double_option")]
    pub multiagent: Option<Option<MultiagentConfig>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentRetrieveParams {
    #[serde(default)]
    pub version: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentListParams {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(
        default,
        deserialize_with = "super::page::deserialize_optional_query_value"
    )]
    pub page: Option<String>,
    #[serde(
        default,
        rename = "created_at[gte]",
        deserialize_with = "super::page::deserialize_optional_query_value"
    )]
    pub created_at_gte: Option<String>,
    #[serde(
        default,
        rename = "created_at[lte]",
        deserialize_with = "super::page::deserialize_optional_query_value"
    )]
    pub created_at_lte: Option<String>,
    #[serde(default)]
    pub include_archived: bool,
}

impl AgentListParams {
    #[must_use]
    pub fn page_query(&self) -> crate::types::PageQuery {
        crate::types::PageQuery {
            limit: self.limit,
            page: self.page.clone(),
        }
    }
}

/// `BetaManagedAgentsAgentReference` — how an agent is *referenced* (by a
/// deployment, a session): `{ id, type: "agent", version }`. The single typed form
/// of the normalized reference; the deserialize-only input form a client may send
/// (a bare id string or `{id, version?}`) is [`super::session::AgentRef`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentReference {
    pub id: String,
    #[serde(rename = "type", skip_deserializing, default = "agent_reference_type")]
    pub object_type: &'static str,
    pub version: u64,
}

fn agent_reference_type() -> &'static str {
    "agent"
}

impl AgentReference {
    pub fn new(id: impl Into<String>, version: u64) -> Self {
        Self {
            id: id.into(),
            object_type: "agent",
            version,
        }
    }

    /// Normalize a client's input reference ([`super::session::AgentRef`], a bare id,
    /// `{id, version?}`, or an `agent_with_overrides` object) into the wire
    /// reference — `version` defaults to 1. The reference identifies the base agent
    /// and version; any per-session overrides are applied separately.
    pub fn from_input(input: &super::session::AgentRef) -> Self {
        Self::new(input.id(), input.version().unwrap_or(1))
    }
}

/// `BetaManagedAgentsAgent` — an agent configuration at a given version.
#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub id: String,
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub name: String,
    pub description: Option<String>,
    pub model: ModelConfig,
    pub system: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub mcp_servers: Vec<AgentMcpServer>,
    pub skills: Vec<AgentSkill>,
    pub tools: Vec<AgentTool>,
    pub multiagent: Option<MultiagentConfig>,
    pub version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_list_optional_query_values_match_both_official_sdk_spellings() {
        // MC/DC: Python omits empty optional fields while TypeScript emits an
        // empty query pair. Every independent field must converge to None; a
        // non-empty cursor/time must remain Some. This covers the complete
        // Agent-list adapter branch set derived from both official serializers.
        let omitted: AgentListParams = serde_urlencoded::from_str("").unwrap();
        let typescript_empty: AgentListParams =
            serde_urlencoded::from_str("page=&created_at%5Bgte%5D=&created_at%5Blte%5D=").unwrap();
        let populated: AgentListParams =
            serde_urlencoded::from_str("page=agent_1&created_at%5Bgte%5D=2026-01-01T00%3A00%3A00Z")
                .unwrap();
        assert_eq!(omitted.page, None);
        assert_eq!(typescript_empty.page, omitted.page);
        assert_eq!(typescript_empty.created_at_gte, None);
        assert_eq!(typescript_empty.created_at_lte, None);
        assert_eq!(populated.page.as_deref(), Some("agent_1"));
        assert_eq!(
            populated.created_at_gte.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
    }

    #[test]
    fn managed_advisor_model_policy_matches_the_official_pair_and_visibility_tables() {
        // Causes: the fixtures below establish `managed advisor model policy` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 executor family; C2 advisor family; C3 Fable is
        // requested as a Managed advisor; C4 either side is unknown/malformed;
        // C5 canonical versus dated/provider-qualified model spelling. Effects:
        // E1 admit exactly the official capability pairs; E2 reject Fable only
        // in the advisor role; E3 reject unknown pairs; E4 redacted 5-series
        // advisors are opaque while Opus 4.7/4.8 are plaintext-capable; E5 C5
        // does not change family capability.
        //
        // | Rule | Pair in Messages table | Advisor Fable | Known | Effect |
        // |---|---|---|---|---|
        // | M1 | yes | no | yes | E1 |
        // | M2 | yes | yes | yes | E2 |
        // | M3 | no | no | yes | E3 |
        // | M4 | any | any | no | E3, fail-closed redaction |
        // | M5 | M1 | no | dated/qualified | E1,E5 |
        let models = [
            "claude-haiku-4-5",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-fable-5",
            "claude-mythos-5",
        ];
        let allowed = std::collections::BTreeSet::from([
            ("claude-haiku-4-5", "claude-opus-4-7"),
            ("claude-haiku-4-5", "claude-opus-4-8"),
            ("claude-haiku-4-5", "claude-opus-5"),
            ("claude-haiku-4-5", "claude-mythos-5"),
            ("claude-sonnet-4-6", "claude-opus-4-7"),
            ("claude-sonnet-4-6", "claude-opus-4-8"),
            ("claude-sonnet-4-6", "claude-opus-5"),
            ("claude-sonnet-4-6", "claude-mythos-5"),
            ("claude-sonnet-5", "claude-opus-4-7"),
            ("claude-sonnet-5", "claude-opus-4-8"),
            ("claude-sonnet-5", "claude-opus-5"),
            ("claude-sonnet-5", "claude-mythos-5"),
            ("claude-opus-4-6", "claude-opus-4-7"),
            ("claude-opus-4-6", "claude-opus-4-8"),
            ("claude-opus-4-6", "claude-opus-5"),
            ("claude-opus-4-6", "claude-mythos-5"),
            ("claude-opus-4-7", "claude-opus-4-7"),
            ("claude-opus-4-7", "claude-opus-4-8"),
            ("claude-opus-4-7", "claude-opus-5"),
            ("claude-opus-4-7", "claude-mythos-5"),
            ("claude-opus-4-8", "claude-opus-4-8"),
            ("claude-opus-4-8", "claude-opus-5"),
            ("claude-opus-4-8", "claude-mythos-5"),
            ("claude-opus-5", "claude-opus-5"),
            ("claude-opus-5", "claude-mythos-5"),
            ("claude-fable-5", "claude-opus-5"),
            ("claude-mythos-5", "claude-opus-5"),
            ("claude-mythos-5", "claude-mythos-5"),
        ]);
        for executor in models {
            for advisor in models {
                assert_eq!(
                    managed_advisor_pair_supported(executor, advisor),
                    allowed.contains(&(executor, advisor)),
                    "M1-M3 executor={executor} advisor={advisor}"
                );
            }
        }
        assert!(
            managed_advisor_pair_supported(
                "claude-sonnet-5-20260801;provider=anthropic",
                "claude-opus-5-20260801;provider=anthropic"
            ),
            "M5/E1/E5"
        );
        for malformed in [
            ("unknown-executor", "claude-opus-5"),
            ("claude-sonnet-5", "claude-opus-5-preview"),
            ("claude-sonnet-5", "CLAUDE-OPUS-5"),
        ] {
            assert!(
                !managed_advisor_pair_supported(malformed.0, malformed.1),
                "M4/E3 {malformed:?}"
            );
        }
        for plaintext in [
            "claude-opus-4-7",
            "claude-opus-4-8-20260801;provider=anthropic",
        ] {
            assert!(
                !managed_advisor_result_is_redacted(plaintext),
                "M1-M5/E4 plaintext {plaintext}"
            );
        }
        for redacted in [
            "claude-opus-5",
            "claude-fable-5-20260801",
            "claude-mythos-5;provider=anthropic",
            "unknown-advisor",
        ] {
            assert!(
                managed_advisor_result_is_redacted(redacted),
                "M2-M5/E4 redacted {redacted}"
            );
        }
    }

    #[test]
    fn managed_agent_composites_follow_the_sdk_tagged_unions() {
        // Causal graph:
        // official SDK JSON -> tagged Managed DTO -> config-domain normalization.
        //
        // Decision table:
        // | input                                      | admission |
        // | every known discriminator + exact fields   | accept    |
        // | unknown discriminator                      | reject    |
        // | known discriminator + misspelled field     | reject    |
        // | custom input_schema extension keyword      | preserve  |
        // | non-standard extension field               | reject    |
        let valid = json!({
            "name": "typed",
            "model": "model-1;executor=acp:codex",
            "mcp_servers": [
                {"type":"url","name":"docs","url":"https://mcp.test"}
            ],
            "skills": [{"type":"custom","skill_id":"skill_1","version":"2"}],
            "tools": [
                {"type":"agent_toolset_20260401","configs":[{"name":"bash","enabled":false}]},
                {"type":"mcp_toolset","mcp_server_name":"docs"},
                {"type":"custom","name":"lookup","description":"Lookup", "input_schema":{
                    "type":"object","properties":{"id":{"type":"string"}},"additionalProperties":false
                }}
            ],
            "multiagent": {"type":"coordinator","agents":[
                "worker", {"type":"self"}, {"type":"advisor","model":"claude-opus-4-6"}
            ]},
        });
        let parsed: AgentCreateParams = serde_json::from_value(valid).expect("SDK union parses");
        let AgentTool::Custom { input_schema, .. } = &parsed.tools[2] else {
            panic!("custom tool retained its variant")
        };
        assert_eq!(input_schema.keywords["additionalProperties"], false);
        assert_eq!(parsed.mcp_servers[0].name, "docs");
        assert!(matches!(
            parsed.multiagent,
            Some(MultiagentConfig::Coordinator { ref agents })
                if matches!(agents[2], MultiagentRosterEntry::Advisor(_))
        ));
        assert_eq!(
            serde_json::to_value(&parsed.mcp_servers[0]).unwrap()["type"],
            "url"
        );

        let update: AgentUpdateParams = serde_json::from_value(json!({
            "version": 3,
            "mcp_servers": [{
                "type": "url",
                "name": "docs",
                "url": "https://mcp.test"
            }]
        }))
        .expect("update reuses the one URL MCP DTO");
        assert!(matches!(
            update.mcp_servers,
            Some(Some(ref servers)) if servers[0].url == "https://mcp.test"
        ));

        for invalid in [
            json!({"name":"x","model":"m","skills":[{"type":"unknown","skill_id":"s"}]}),
            json!({"name":"x","model":"m","mcp_servers":[{"name":"s","url":"https://x"}]}),
            json!({"name":"x","model":"m","mcp_servers":[{"type":"url","name":"s","uri":"https://x"}]}),
            json!({"name":"x","model":"m","mcp_servers":[{"type":"sandbox_stdio","name":"s","command":"tool"}]}),
            json!({"name":"x","model":"m","mcp_servers":[{"type":"url","name":"s","url":"https://x","prompts_as_skills":true}]}),
            json!({"name":"x","model":"m","tools":[{"type":"mcp_toolset","mcp_server":"s"}]}),
            json!({"name":"x","model":"m","max_steps":40}),
        ] {
            assert!(serde_json::from_value::<AgentCreateParams>(invalid).is_err());
        }
    }

    #[test]
    fn agent_response_contains_only_the_sdk_fields() {
        // Cause/effect decision table: official fields serialize; historical
        // Awaken-only lifecycle fields (`status`, `disabled_at`) have no DTO owner
        // and therefore cannot appear in the response object.
        let value = serde_json::to_value(Agent {
            id: "agent_1".into(),
            object_type: "agent",
            archived_at: None,
            created_at: "2026-08-15T00:00:00Z".into(),
            updated_at: "2026-08-15T00:00:00Z".into(),
            name: "assistant".into(),
            description: None,
            model: ModelConfig::new("claude-sonnet-4-6"),
            system: None,
            metadata: BTreeMap::new(),
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            tools: Vec::new(),
            multiagent: None,
            version: 1,
        })
        .unwrap();
        assert!(value.get("status").is_none());
        assert!(value.get("disabled_at").is_none());
        assert_eq!(value["type"], "agent");
    }
}
