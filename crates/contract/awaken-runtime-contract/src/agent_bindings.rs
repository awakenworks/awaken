//! Published Agent bindings consumed by execution adapters.
//!
//! Agent authoring owns the external wire unions. Compilation normalizes the
//! executable subset into this typed contract. The bindings are a first-class
//! part of the immutable resolved configuration; they are never encoded into a
//! free-form plugin JSON section.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};

use crate::snapshot::AgentId;
pub use awaken_agent_contract::{
    ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
    ToolsetSource,
};
use serde::{Deserialize, Serialize};

/// Provider-neutral inference controls frozen into an Agent publication.
///
/// These are deliberately separate from `ModelBinding`: the binding is routing
/// identity used for exact candidate and credential lookup, whereas these values
/// tune each call made through that exact route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<InferenceSpeed>,
    /// Exact geographic placement constraint for provider inference. The
    /// adapter must honor or reject it before provider network I/O.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<InferenceGeography>,
}

impl InferenceOptions {
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Provider-neutral processing geography required for every model attempt.
///
/// The resolved runtime uses one closed, provider-neutral vocabulary. Anthropic's
/// official `inference_geo` remains `us | global`; other exact boundaries enter
/// through the Awaken extension before resolution. Candidates realize the
/// boundary through a provider request field, frozen regional endpoint,
/// deployment, or inference profile. Macro and country boundaries are
/// deliberately not treated as interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceGeography {
    Us,
    Eu,
    Apac,
    Cn,
    Jp,
    Au,
    Ca,
    Uk,
    Hk,
}

impl InferenceGeography {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Us => "us",
            Self::Eu => "eu",
            Self::Apac => "apac",
            Self::Cn => "cn",
            Self::Jp => "jp",
            Self::Au => "au",
            Self::Ca => "ca",
            Self::Uk => "uk",
            Self::Hk => "hk",
        }
    }
}

impl std::fmt::Display for InferenceGeography {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for InferenceGeography {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "us" => Ok(Self::Us),
            "eu" => Ok(Self::Eu),
            "apac" => Ok(Self::Apac),
            "cn" => Ok(Self::Cn),
            "jp" => Ok(Self::Jp),
            "au" => Ok(Self::Au),
            "ca" => Ok(Self::Ca),
            "uk" => Ok(Self::Uk),
            "hk" => Ok(Self::Hk),
            other => Err(format!("unsupported inference_geo `{other}`")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceSpeed {
    Standard,
    Fast,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHttpMcpTransport {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSandboxStdioMcpTransport {
    #[serde(rename = "type")]
    pub kind: AgentSandboxStdioMcpTransportKind,
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentSandboxStdioMcpTransportKind {
    #[serde(rename = "sandbox_stdio")]
    SandboxStdio,
}

/// Authoring transport for an Agent-owned MCP binding. HTTP retains its legacy
/// `{url}` shape; `sandbox_stdio` is explicit and can only be realized inside a
/// Session Environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMcpTransportBinding {
    SandboxStdio(AgentSandboxStdioMcpTransport),
    Http(AgentHttpMcpTransport),
}

impl AgentMcpTransportBinding {
    #[must_use]
    pub fn http(url: impl Into<String>) -> Self {
        Self::Http(AgentHttpMcpTransport { url: url.into() })
    }

    #[must_use]
    pub fn sandbox_stdio(command: impl Into<String>, args: Vec<String>) -> Self {
        Self::SandboxStdio(AgentSandboxStdioMcpTransport {
            kind: AgentSandboxStdioMcpTransportKind::SandboxStdio,
            command: command.into(),
            args,
        })
    }

    pub fn normalize(&self) -> Result<awaken_agent_contract::McpTarget, String> {
        match self {
            Self::Http(transport) => awaken_agent_contract::McpTarget::parse_http(&transport.url),
            Self::SandboxStdio(transport) => awaken_agent_contract::McpTarget::sandbox_stdio(
                &transport.command,
                transport.args.clone(),
            ),
        }
        .map_err(|error| error.to_string())
    }
}

/// One MCP server inherited by Sessions of the published Agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMcpServerBinding {
    #[serde(alias = "id")]
    pub name: String,
    #[serde(flatten)]
    pub transport: AgentMcpTransportBinding,
    /// Exact secret-free credential revision frozen into the publication.
    ///
    /// Runtime may materialize this revision but must never select a different
    /// credential. `None` denotes an intentionally unauthenticated server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<crate::credential::CredentialRef>,
    /// Explicitly project this server's MCP prompts into the Agent's unified
    /// Skill catalog. Off by default: ordinary MCP prompts remain prompts.
    #[serde(default, skip_serializing_if = "is_false")]
    pub prompts_as_skills: bool,
}

/// One immutable delegation edge compiled into an Agent publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDelegateBinding {
    pub agent_id: AgentId,
    /// Exact referenced Agent authoring revision. `None` is retained only for
    /// embedded publications that intentionally resolve current at Session setup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<u64>,
    /// The target is an intentional copy of the publication that owns this edge.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recursive_self: bool,
}

/// Advisor intent frozen beside the ordinary delegate roster. Model
/// route resolution is completed by the publication owner before execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAdvisorBinding {
    /// Public model id retained for exact Agent/Session projection.
    pub model: String,
    /// Complete execution route frozen by the same publication resolver as the
    /// primary model. Runtime never re-resolves advisor identity.
    pub candidate: crate::resolved::ResolvedModelCandidate,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AgentDelegateBindingWire {
    LegacyId(AgentId),
    Binding(AgentDelegateBinding),
}

fn deserialize_delegate_bindings<'de, D>(
    deserializer: D,
) -> Result<Vec<AgentDelegateBinding>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Vec::<AgentDelegateBindingWire>::deserialize(deserializer)?
        .into_iter()
        .map(|wire| match wire {
            AgentDelegateBindingWire::LegacyId(agent_id) => AgentDelegateBinding {
                agent_id,
                source_revision: None,
                recursive_self: false,
            },
            AgentDelegateBindingWire::Binding(binding) => binding,
        })
        .collect())
}

/// The normalized, executable subset of Agent integration configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBindings {
    #[serde(default)]
    pub mcp_servers: Vec<AgentMcpServerBinding>,
    #[serde(default, alias = "skill_ids")]
    pub skills: Vec<awaken_agent_contract::AgentSkillBinding>,
    /// Exact published Agent edges this Agent may invoke through `agent_run`.
    #[serde(
        default,
        alias = "delegate_ids",
        deserialize_with = "deserialize_delegate_bindings",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub delegates: Vec<AgentDelegateBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor: Option<AgentAdvisorBinding>,
    /// Exact tool availability/confirmation policy compiled from authoring.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub toolsets: Vec<ToolsetPolicy>,
}

impl AgentBindings {
    pub fn delegate_ids(&self) -> impl Iterator<Item = &AgentId> {
        self.delegates.iter().map(|binding| &binding.agent_id)
    }

    /// Resolve a concrete runtime tool id against the exact matching toolset.
    /// `None` means the tool is outside all authored toolsets and retains its
    /// ordinary exact-id capability behavior.
    ///
    /// The two sources are deliberately asymmetric, and the asymmetry is a
    /// safety property rather than an oversight.
    ///
    /// An MCP tool id *self-identifies*: `mcp__server__tool` names the server it
    /// belongs to, so membership is decidable from the id alone and the toolset
    /// default can be applied here. An Agent-toolset id is a bare name that
    /// carries no membership, and the closed roster that would decide it lives
    /// at the Session-contract altitude — above this crate, which must stay
    /// linkable as an embeddable SDK with no serving dependency.
    ///
    /// So only an *explicit* Agent override resolves here. Applying the Agent
    /// default without the roster would hand it to every non-MCP id — including
    /// tools that are not toolset members at all, such as skill discovery or
    /// semantic-memory reads — which is a fail-open. The roster-aware caller (the
    /// host) projects the Agent default per member instead.
    #[must_use]
    pub fn tool_policy(&self, tool_id: &str) -> Option<ToolExecutionPolicy> {
        if let Some(policy) = self
            .toolsets
            .iter()
            .find(|policy| policy.source == ToolsetSource::Agent)
            .and_then(|policy| {
                policy
                    .overrides
                    .iter()
                    .find(|entry| entry.name == tool_id)
                    .map(|entry| entry.policy)
            })
        {
            return Some(policy);
        }
        // The id carries the *encoded* server name and the toolset carries the
        // *configured* one. Comparing those two spellings directly is what let a
        // disabled toolset on `github-mcp` resolve to no policy at all, so the
        // identity type owns the comparison and encodes both sides.
        let mcp = awaken_agent_contract::McpToolId::parse(tool_id)?;
        let tool_name = mcp.tool_name();
        self.toolsets
            .iter()
            .find(|policy| {
                matches!(
                    &policy.source,
                    ToolsetSource::Mcp { server_name } if mcp.is_served_by(server_name)
                )
            })
            .map(|policy| policy.policy_for(&tool_name))
    }
}

/// Strongly typed resolved configuration carried by an executable snapshot.
///
/// Agent integration bindings have a fixed schema and therefore live in their
/// own field. Only extension-owned sections remain open JSON because each plugin
/// owns a different schema and validates its own section before execution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResolvedConfiguration {
    #[serde(default)]
    pub agent: AgentBindings,
    /// Typed model-call controls. This is snapshot data, never plugin-owned JSON.
    #[serde(default, skip_serializing_if = "InferenceOptions::is_default")]
    pub inference: InferenceOptions,
    #[serde(default)]
    plugins: BTreeMap<String, serde_json::Value>,
}

impl ResolvedConfiguration {
    #[must_use]
    pub fn new(agent: AgentBindings, plugins: BTreeMap<String, serde_json::Value>) -> Self {
        Self {
            agent,
            inference: InferenceOptions::default(),
            plugins,
        }
    }

    #[must_use]
    pub fn with_inference(mut self, inference: InferenceOptions) -> Self {
        self.inference = inference;
        self
    }

    #[must_use]
    pub fn plugins(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.plugins
    }
}

impl From<BTreeMap<String, serde_json::Value>> for ResolvedConfiguration {
    fn from(plugins: BTreeMap<String, serde_json::Value>) -> Self {
        Self {
            agent: AgentBindings::default(),
            inference: InferenceOptions::default(),
            plugins,
        }
    }
}

impl Deref for ResolvedConfiguration {
    type Target = BTreeMap<String, serde_json::Value>;

    fn deref(&self) -> &Self::Target {
        &self.plugins
    }
}

impl DerefMut for ResolvedConfiguration {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.plugins
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_agent_toolset_default_never_reaches_a_tool_it_does_not_list() {
        // Cause/effect graph. C1 an Agent toolset is authored with a permissive
        // default; C2 the looked-up id is an explicit override of that toolset;
        // C3 the id is a bare name the toolset never lists (skill discovery, a
        // semantic-memory read, any runtime-owned tool); C4 the id names an MCP
        // server this bindings set configures.
        //
        // E1 return the override verbatim. E2 return `None`, so the caller keeps
        // the tool's ordinary capability behavior instead of inheriting an
        // authority nobody granted it. E3 return the MCP toolset's own default,
        // because an `mcp__server__tool` id decides its own membership.
        //
        // | Rule | C2 | C3 | C4 | Effect |
        // |---|---|---|---|---|
        // | T1 | T | F | F | E1 the override |
        // | T2 | F | T | F | E2 None — this is the fail-open being guarded |
        // | T3 | F | F | T | E3 the MCP default |
        //
        // T2 is the load-bearing row. Adding an Agent default fallback here
        // would look like a completeness fix and would in fact grant that
        // default to every non-MCP id, because a bare name carries no
        // membership and the closed roster is not knowable at this altitude.
        let allow = ToolExecutionPolicy {
            enabled: true,
            permission: ToolPermissionRequirement::AlwaysAllow,
        };
        let ask = ToolExecutionPolicy {
            enabled: true,
            permission: ToolPermissionRequirement::AlwaysAsk,
        };
        let bindings = AgentBindings {
            toolsets: vec![
                ToolsetPolicy {
                    source: ToolsetSource::Agent,
                    default: allow,
                    overrides: vec![ToolPolicyOverride::new("bash", ask)],
                },
                ToolsetPolicy {
                    source: ToolsetSource::Mcp {
                        server_name: "github".into(),
                    },
                    default: ask,
                    overrides: Vec::new(),
                },
            ],
            ..AgentBindings::default()
        };

        assert_eq!(bindings.tool_policy("bash"), Some(ask), "T1");
        for unlisted in ["list_skills", "Skill", "read_memory", "read"] {
            assert_eq!(
                bindings.tool_policy(unlisted),
                None,
                "T2 `{unlisted}` must not inherit the Agent toolset default"
            );
        }
        assert_eq!(bindings.tool_policy("mcp__github__search"), Some(ask), "T3");
    }

    use super::{
        AgentBindings, AgentMcpServerBinding, AgentMcpTransportBinding, ToolExecutionPolicy,
        ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
    };
    use awaken_agent_contract::McpToolId;

    fn disabled_toolset(server_name: &str) -> AgentBindings {
        AgentBindings {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Mcp {
                    server_name: server_name.into(),
                },
                default: ToolExecutionPolicy {
                    enabled: false,
                    permission: ToolPermissionRequirement::AlwaysAsk,
                },
                overrides: Vec::new(),
            }],
            ..AgentBindings::default()
        }
    }

    #[test]
    fn a_disabled_toolset_governs_every_punctuated_server_spelling() {
        // The live bypass: an operator disables `github-mcp`, the producer emits
        // an encoded id, and the consumer used to compare that encoded segment
        // against the raw configured name — `None`, which means *no policy*.
        for server_name in ["github-mcp", "githubmcp", "github.mcp", "api.example.com"] {
            let bindings = disabled_toolset(server_name);
            let tool_id = McpToolId::new(server_name, "search").expect("id");
            let policy = bindings.tool_policy(tool_id.as_str()).unwrap_or_else(|| {
                panic!("disabled toolset on {server_name:?} must govern {tool_id}")
            });
            assert!(!policy.enabled, "{server_name:?} must stay disabled");
        }
    }

    #[test]
    fn one_servers_policy_never_leaks_onto_a_differently_punctuated_neighbour() {
        let bindings = disabled_toolset("a-b");
        for neighbour in ["a.b", "a_b", "ab"] {
            let tool_id = McpToolId::new(neighbour, "search").expect("id");
            assert_eq!(
                bindings.tool_policy(tool_id.as_str()),
                None,
                "{neighbour:?} must not inherit the policy authored for \"a-b\""
            );
        }
    }

    #[test]
    fn an_override_is_matched_by_the_tool_name_the_server_advertises() {
        let bindings = AgentBindings {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Mcp {
                    server_name: "github-mcp".into(),
                },
                default: ToolExecutionPolicy {
                    enabled: false,
                    permission: ToolPermissionRequirement::AlwaysAsk,
                },
                overrides: vec![ToolPolicyOverride::new(
                    "search.issues",
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAsk,
                    },
                )],
            }],
            ..AgentBindings::default()
        };
        let allowed = McpToolId::new("github-mcp", "search.issues").expect("id");
        assert_eq!(
            bindings.tool_policy(allowed.as_str()).map(|p| p.enabled),
            Some(true)
        );
        let other = McpToolId::new("github-mcp", "search_issues").expect("id");
        assert_eq!(
            bindings.tool_policy(other.as_str()).map(|p| p.enabled),
            Some(false),
            "a different tool name falls to the disabled toolset default"
        );
    }

    #[test]
    fn flattened_mcp_transport_accepts_binding_metadata_for_both_variants() {
        let http: AgentMcpServerBinding = serde_json::from_value(serde_json::json!({
            "name": "issues",
            "url": "https://mcp.example.test/issues",
            "prompts_as_skills": true
        }))
        .expect("HTTP MCP binding should deserialize");
        assert!(matches!(http.transport, AgentMcpTransportBinding::Http(_)));

        let stdio: AgentMcpServerBinding = serde_json::from_value(serde_json::json!({
            "type": "sandbox_stdio",
            "name": "browser",
            "command": "playwright-mcp",
            "args": ["--headless", "--isolated"],
            "prompts_as_skills": false
        }))
        .expect("sandbox stdio MCP binding should deserialize");
        assert!(matches!(
            stdio.transport,
            AgentMcpTransportBinding::SandboxStdio(_)
        ));
    }
}
