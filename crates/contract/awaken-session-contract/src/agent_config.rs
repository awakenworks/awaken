//! Managed Agents tool wire vocabulary retained temporarily by Session storage.

use serde::{Deserialize, Serialize};

fn deserialize_present_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
use std::collections::BTreeMap;

/// Durable Managed Agent tool projection. This is the single serialized owner
/// shared by the wire adapter and Session store; JSON Schema keywords remain the
/// only deliberately open part of the value.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentTool {
    #[serde(rename = "agent_toolset_20260401")]
    AgentToolset20260401 {
        #[serde(default)]
        configs: Vec<AgentToolConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_config: Option<AgentToolDefaultConfig>,
    },
    McpToolset {
        mcp_server_name: String,
        #[serde(default)]
        configs: Vec<AgentToolConfig>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_config: Option<AgentToolDefaultConfig>,
    },
    Custom {
        name: String,
        description: String,
        input_schema: CustomToolInputSchema,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomToolInputSchema {
    #[serde(rename = "type")]
    pub kind: ObjectSchemaKind,
    #[serde(flatten)]
    pub keywords: BTreeMap<String, serde_json::Value>,
}

impl CustomToolInputSchema {
    pub fn from_value(value: serde_json::Value) -> Result<Self, String> {
        serde_json::from_value(value).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ObjectSchemaKind {
    #[serde(rename = "object")]
    Object,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentToolPermissionPolicy {
    AlwaysAllow,
    AlwaysAsk,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentToolConfig {
    pub name: String,
    #[serde(
        rename = "type",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_non_null"
    )]
    pub kind: Option<AgentToolsetMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_policy: Option<AgentToolPermissionPolicy>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_non_null"
    )]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_non_null"
    )]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<AgentWebSearchUserLocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentWebSearchUserLocation {
    #[serde(rename = "type")]
    pub kind: AgentWebSearchUserLocationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentWebSearchUserLocationKind {
    #[serde(rename = "approximate")]
    Approximate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentToolDefaultConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_policy: Option<AgentToolPermissionPolicy>,
}

/// Closed discriminant for the official versioned Runtime capability surface.
/// The finite enum keeps symbolic proofs away from unbounded string comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolsetMember {
    Bash,
    Read,
    Write,
    Edit,
    Glob,
    Grep,
    WebFetch,
    WebSearch,
}

impl AgentToolsetMember {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::WebFetch => "web_fetch",
            Self::WebSearch => "web_search",
        }
    }
}

const AGENT_TOOLSET_MEMBERS: [AgentToolsetMember; 8] = [
    AgentToolsetMember::Bash,
    AgentToolsetMember::Read,
    AgentToolsetMember::Write,
    AgentToolsetMember::Edit,
    AgentToolsetMember::Glob,
    AgentToolsetMember::Grep,
    AgentToolsetMember::WebFetch,
    AgentToolsetMember::WebSearch,
];

/// The official Managed Agent toolset's versioned membership. It lives beside
/// the wire value so every adapter normalizes the same closed set.
pub const AGENT_TOOLSET_TOOL_IDS: [&str; 8] = [
    AgentToolsetMember::Bash.as_str(),
    AgentToolsetMember::Read.as_str(),
    AgentToolsetMember::Write.as_str(),
    AgentToolsetMember::Edit.as_str(),
    AgentToolsetMember::Glob.as_str(),
    AgentToolsetMember::Grep.as_str(),
    AgentToolsetMember::WebFetch.as_str(),
    AgentToolsetMember::WebSearch.as_str(),
];

/// Allocation-free traversal of the complete versioned Runtime capability
/// projection. Keeping the iterator beside the closed array prevents adapters
/// from maintaining a second list or accidentally omitting the final member.
#[must_use]
pub fn agent_toolset_members() -> impl ExactSizeIterator<Item = &'static str> + DoubleEndedIterator
{
    bounded_agent_toolset_members().map(AgentToolsetMember::as_str)
}

#[must_use]
fn bounded_agent_toolset_members() -> core::array::IntoIter<AgentToolsetMember, 8> {
    AGENT_TOOLSET_MEMBERS.into_iter()
}

#[must_use]
pub fn is_agent_toolset_member(name: &str) -> bool {
    agent_toolset_members().any(|member| member == name)
}

fn valid_domain(value: &str, allow_path: bool) -> bool {
    let (host, path) = value.split_once('/').unwrap_or((value, ""));
    !host.is_empty()
        && (allow_path || path.is_empty())
        && !value.contains("://")
        && !host.contains(':')
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn validate_domains(
    name: &str,
    allowed: Option<&Vec<String>>,
    blocked: Option<&Vec<String>>,
    allow_path: bool,
) -> Result<(), String> {
    if allowed.is_some() && blocked.is_some() {
        return Err(format!(
            "{name} cannot combine allowed_domains with blocked_domains"
        ));
    }
    for (field, domains) in [("allowed_domains", allowed), ("blocked_domains", blocked)] {
        if let Some(domains) = domains {
            if domains.is_empty() || domains.len() > 64 {
                return Err(format!("{name}.{field} must contain 1 to 64 domains"));
            }
            if domains
                .iter()
                .any(|domain| !valid_domain(domain, allow_path))
            {
                return Err(format!("{name}.{field} contains an invalid domain"));
            }
        }
    }
    Ok(())
}

/// Validate the official per-tool discriminated input before normalization.
/// Callers must run this once at the Agent admission boundary so invalid Web
/// fields cannot be lost while producing the closed executable configuration.
pub fn validate_agent_tools(tools: &[AgentTool]) -> Result<(), String> {
    for tool in tools {
        let (agent_toolset, configs) = match tool {
            AgentTool::AgentToolset20260401 { configs, .. } => (true, configs),
            AgentTool::McpToolset { configs, .. } => (false, configs),
            AgentTool::Custom { .. } => continue,
        };
        for config in configs {
            let member = agent_toolset
                .then(|| {
                    AGENT_TOOLSET_MEMBERS
                        .iter()
                        .copied()
                        .find(|member| member.as_str() == config.name)
                })
                .flatten();
            if agent_toolset && member.is_none() {
                return Err(format!("unknown agent tool `{}`", config.name));
            }
            if config.kind.is_some() && config.kind != member {
                return Err(format!(
                    "tool config type does not match name `{}`",
                    config.name
                ));
            }
            let has_web_fields = config.allowed_domains.is_some()
                || config.blocked_domains.is_some()
                || config.max_content_tokens.is_some()
                || config.user_location.is_some();
            match member {
                Some(AgentToolsetMember::WebFetch) => {
                    validate_domains(
                        "web_fetch",
                        config.allowed_domains.as_ref(),
                        config.blocked_domains.as_ref(),
                        false,
                    )?;
                    if config.user_location.is_some() {
                        return Err("web_fetch cannot define user_location".into());
                    }
                }
                Some(AgentToolsetMember::WebSearch) => {
                    validate_domains(
                        "web_search",
                        config.allowed_domains.as_ref(),
                        config.blocked_domains.as_ref(),
                        true,
                    )?;
                    if config.max_content_tokens.is_some() {
                        return Err("web_search cannot define max_content_tokens".into());
                    }
                    if let Some(location) = &config.user_location
                        && (location.country.as_ref().is_some_and(|country| {
                            country.len() != 2
                                || !country.bytes().all(|byte| byte.is_ascii_uppercase())
                        }) || location
                            .city
                            .as_ref()
                            .is_some_and(|value| value.trim().is_empty())
                            || location
                                .region
                                .as_ref()
                                .is_some_and(|value| value.trim().is_empty())
                            || location
                                .timezone
                                .as_ref()
                                .is_some_and(|value| value.trim().is_empty()))
                    {
                        return Err("web_search.user_location is invalid".into());
                    }
                }
                _ if has_web_fields => {
                    return Err(format!(
                        "tool `{}` cannot define Web tool configuration",
                        config.name
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn domain_filter(config: &AgentToolConfig) -> Option<serde_json::Value> {
    config
        .allowed_domains
        .clone()
        .map(|domains| serde_json::json!({"type":"allow", "domains":domains}))
        .or_else(|| {
            config
                .blocked_domains
                .clone()
                .map(|domains| serde_json::json!({"type":"block", "domains":domains}))
        })
}

fn execution_configuration(config: &AgentToolConfig) -> Option<serde_json::Value> {
    match config.name.as_str() {
        "web_fetch"
            if config.allowed_domains.is_some()
                || config.blocked_domains.is_some()
                || config.max_content_tokens.is_some() =>
        {
            let mut value = serde_json::Map::from_iter([(
                "type".to_string(),
                serde_json::Value::String("web_fetch".to_string()),
            )]);
            if let Some(domains) = domain_filter(config) {
                value.insert("domains".to_string(), domains);
            }
            if let Some(max_content_tokens) = config.max_content_tokens {
                value.insert(
                    "max_content_tokens".to_string(),
                    serde_json::Value::from(max_content_tokens),
                );
            }
            Some(serde_json::Value::Object(value))
        }
        "web_search"
            if config.allowed_domains.is_some()
                || config.blocked_domains.is_some()
                || config.user_location.is_some() =>
        {
            let mut value = serde_json::Map::from_iter([(
                "type".to_string(),
                serde_json::Value::String("web_search".to_string()),
            )]);
            if let Some(domains) = domain_filter(config) {
                value.insert("domains".to_string(), domains);
            }
            if let Some(location) = &config.user_location {
                let mut projected = serde_json::Map::new();
                for (name, field) in [
                    ("city", &location.city),
                    ("country", &location.country),
                    ("region", &location.region),
                    ("timezone", &location.timezone),
                ] {
                    if let Some(field) = field {
                        projected
                            .insert(name.to_string(), serde_json::Value::String(field.clone()));
                    }
                }
                value.insert(
                    "user_location".to_string(),
                    serde_json::Value::Object(projected),
                );
            }
            Some(serde_json::Value::Object(value))
        }
        _ => None,
    }
}

/// Normalize the durable wire vocabulary into the one neutral execution policy.
#[must_use]
pub fn toolset_policies(tools: &[AgentTool]) -> Vec<awaken_agent_contract::ToolsetPolicy> {
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    let permission = |value: Option<AgentToolPermissionPolicy>,
                      fallback: ToolPermissionRequirement| match value {
        Some(AgentToolPermissionPolicy::AlwaysAllow) => ToolPermissionRequirement::AlwaysAllow,
        Some(AgentToolPermissionPolicy::AlwaysAsk) => ToolPermissionRequirement::AlwaysAsk,
        None => fallback,
    };
    let default_permission = |source: &ToolsetSource| match source {
        ToolsetSource::Agent => ToolPermissionRequirement::AlwaysAllow,
        // Managed Agents defaults MCP to confirmation so a tool added later by
        // the remote server cannot silently gain execution authority.
        ToolsetSource::Mcp { .. } => ToolPermissionRequirement::AlwaysAsk,
    };
    tools
        .iter()
        .filter_map(|tool| {
            let (source, configs, default_config) = match tool {
                AgentTool::AgentToolset20260401 {
                    configs,
                    default_config,
                } => (ToolsetSource::Agent, configs, default_config.as_ref()),
                AgentTool::McpToolset {
                    mcp_server_name,
                    configs,
                    default_config,
                } => (
                    ToolsetSource::Mcp {
                        server_name: mcp_server_name.clone(),
                    },
                    configs,
                    default_config.as_ref(),
                ),
                AgentTool::Custom { .. } => return None,
            };
            let default = ToolExecutionPolicy {
                enabled: default_config
                    .and_then(|value| value.enabled)
                    .unwrap_or(true),
                permission: permission(
                    default_config.and_then(|value| value.permission_policy),
                    default_permission(&source),
                ),
            };
            let overrides = match &source {
                ToolsetSource::Agent => agent_toolset_members()
                    .map(|name| {
                        let config = configs.iter().find(|config| config.name == name);
                        ToolPolicyOverride::with_optional_configuration(
                            name,
                            ToolExecutionPolicy {
                                enabled: config
                                    .and_then(|value| value.enabled)
                                    .unwrap_or(default.enabled),
                                permission: config
                                    .and_then(|value| value.permission_policy)
                                    .map(|value| permission(Some(value), default.permission))
                                    .unwrap_or(default.permission),
                            },
                            config.and_then(execution_configuration),
                        )
                    })
                    .collect(),
                ToolsetSource::Mcp { .. } => configs
                    .iter()
                    .map(|config| {
                        ToolPolicyOverride::new(
                            config.name.clone(),
                            ToolExecutionPolicy {
                                enabled: config.enabled.unwrap_or(default.enabled),
                                permission: config
                                    .permission_policy
                                    .map(|value| permission(Some(value), default.permission))
                                    .unwrap_or(default.permission),
                            },
                        )
                    })
                    .collect(),
            };
            Some(ToolsetPolicy {
                source,
                default,
                overrides,
            })
        })
        .collect()
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(9)]
fn bounded_runtime_capability_iterator_uses_exact_member_projection() {
    let expected = [
        AgentToolsetMember::Bash,
        AgentToolsetMember::Read,
        AgentToolsetMember::Write,
        AgentToolsetMember::Edit,
        AgentToolsetMember::Glob,
        AgentToolsetMember::Grep,
        AgentToolsetMember::WebFetch,
        AgentToolsetMember::WebSearch,
    ];
    let mut projected = bounded_agent_toolset_members();
    let mut index = 0usize;
    while index < expected.len() {
        assert_eq!(projected.next(), Some(expected[index]));
        index += 1;
    }
    assert_eq!(projected.next(), None);
}

#[cfg(kani)]
#[kani::proof]
fn runtime_capability_member_projection_is_total_exact_and_bounded() {
    let index: u8 = kani::any();
    kani::assume(index <= 8);
    let projected = bounded_agent_toolset_members().nth(usize::from(index));
    let expected = match index {
        0 => Some(AgentToolsetMember::Bash),
        1 => Some(AgentToolsetMember::Read),
        2 => Some(AgentToolsetMember::Write),
        3 => Some(AgentToolsetMember::Edit),
        4 => Some(AgentToolsetMember::Glob),
        5 => Some(AgentToolsetMember::Grep),
        6 => Some(AgentToolsetMember::WebFetch),
        7 => Some(AgentToolsetMember::WebSearch),
        _ => None,
    };
    assert_eq!(projected, expected);
}

/// Project a resolved policy back to its canonical durable wire value.
#[must_use]
pub fn resolved_toolsets(policies: &[awaken_agent_contract::ToolsetPolicy]) -> Vec<AgentTool> {
    use awaken_agent_contract::{ToolPermissionRequirement, ToolsetSource};

    let permission = |value| match value {
        ToolPermissionRequirement::AlwaysAllow => AgentToolPermissionPolicy::AlwaysAllow,
        ToolPermissionRequirement::AlwaysAsk => AgentToolPermissionPolicy::AlwaysAsk,
    };
    policies
        .iter()
        .map(|policy| {
            let configs = policy
                .overrides
                .iter()
                // A neutral Session policy may also contain Runtime-only Agent
                // tools (for example Dream's move/delete capabilities). They
                // remain executable policy but have no representation in the
                // closed, versioned Managed Agent toolset.
                .filter(|entry| {
                    policy.source != ToolsetSource::Agent || is_agent_toolset_member(&entry.name)
                })
                .filter(|entry| entry.policy != policy.default || entry.configuration.is_some())
                .map(|entry| {
                    let configuration = entry
                        .configuration
                        .as_ref()
                        .map(|value| execution_configuration_object(&entry.name, value));
                    AgentToolConfig {
                        name: entry.name.clone(),
                        kind: (policy.source == ToolsetSource::Agent).then(|| {
                            AGENT_TOOLSET_MEMBERS
                                .iter()
                                .copied()
                                .find(|member| member.as_str() == entry.name)
                                .expect("validated Agent toolset member")
                        }),
                        enabled: Some(entry.policy.enabled),
                        permission_policy: Some(permission(entry.policy.permission)),
                        allowed_domains: projected_domains(configuration, "allow"),
                        blocked_domains: projected_domains(configuration, "block"),
                        max_content_tokens: configuration
                            .and_then(|value| value.get("max_content_tokens"))
                            .and_then(serde_json::Value::as_u64),
                        user_location: configuration
                            .and_then(|value| value.get("user_location"))
                            .map(projected_user_location),
                    }
                })
                .collect();
            let default_config = Some(AgentToolDefaultConfig {
                enabled: Some(policy.default.enabled),
                permission_policy: Some(permission(policy.default.permission)),
            });
            match &policy.source {
                ToolsetSource::Agent => AgentTool::AgentToolset20260401 {
                    configs,
                    default_config,
                },
                ToolsetSource::Mcp { server_name } => AgentTool::McpToolset {
                    mcp_server_name: server_name.clone(),
                    configs,
                    default_config,
                },
            }
        })
        .collect()
}

fn execution_configuration_object<'a>(
    tool_name: &str,
    value: &'a serde_json::Value,
) -> &'a serde_json::Map<String, serde_json::Value> {
    let object = value
        .as_object()
        .expect("validated tool execution configuration is an object");
    assert_eq!(
        object.get("type").and_then(serde_json::Value::as_str),
        Some(tool_name),
        "validated tool execution configuration matches its policy owner"
    );
    object
}

fn projected_domains(
    configuration: Option<&serde_json::Map<String, serde_json::Value>>,
    expected_kind: &str,
) -> Option<Vec<String>> {
    let filter = configuration?
        .get("domains")?
        .as_object()
        .expect("validated Web execution configuration carries an object domain filter");
    (filter.get("type").and_then(serde_json::Value::as_str) == Some(expected_kind)).then(|| {
        serde_json::from_value(
            filter
                .get("domains")
                .expect("validated Web domain filter carries domains")
                .clone(),
        )
        .expect("validated Web domains are strings")
    })
}

fn projected_user_location(value: &serde_json::Value) -> AgentWebSearchUserLocation {
    let value = value
        .as_object()
        .expect("validated WebSearch user location is an object");
    let field = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    AgentWebSearchUserLocation {
        kind: AgentWebSearchUserLocationKind::Approximate,
        city: field("city"),
        country: field("country"),
        region: field("region"),
        timezone: field("timezone"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_agent_toolset_has_one_exact_eight_member_source() {
        // Cause/effect graph: the official versioned toolset declaration is the
        // closed membership source; policy normalization must materialize every
        // member once and must not create a second WebSearch representation.
        //
        // Decision table:
        // | Rule | authored overrides      | effects                         |
        // | T1   | none                    | exact eight unique defaults     |
        // | T2   | web_search disabled     | same member, disabled once      |
        // | T3   | unknown name            | admission rejects; no policy   |
        // Constraints/invariants: the versioned declaration is the sole member
        // authority and each official name is materialized exactly once.
        assert_eq!(
            AGENT_TOOLSET_TOOL_IDS,
            [
                "bash",
                "read",
                "write",
                "edit",
                "glob",
                "grep",
                "web_fetch",
                "web_search",
            ],
            "T1"
        );
        let policies = toolset_policies(&[AgentTool::AgentToolset20260401 {
            configs: vec![AgentToolConfig {
                name: "web_search".into(),
                kind: None,
                enabled: Some(false),
                permission_policy: None,
                allowed_domains: None,
                blocked_domains: None,
                max_content_tokens: None,
                user_location: None,
            }],
            default_config: None,
        }]);
        let overrides = &policies[0].overrides;
        assert_eq!(overrides.len(), 8, "T1");
        assert_eq!(
            overrides
                .iter()
                .filter(|entry| entry.name == "web_search")
                .count(),
            1,
            "T2"
        );
        assert!(
            !overrides
                .iter()
                .find(|entry| entry.name == "web_search")
                .expect("official member")
                .policy
                .enabled,
            "T2"
        );
        let unknown = AgentTool::AgentToolset20260401 {
            configs: vec![AgentToolConfig {
                name: "parallel_web_search".into(),
                kind: None,
                enabled: None,
                permission_policy: None,
                allowed_domains: None,
                blocked_domains: None,
                max_content_tokens: None,
                user_location: None,
            }],
            default_config: None,
        };
        assert!(!is_agent_toolset_member("parallel_web_search"), "T3");
        assert_eq!(
            validate_agent_tools(std::slice::from_ref(&unknown)).unwrap_err(),
            "unknown agent tool `parallel_web_search`",
            "T3"
        );
    }

    #[test]
    fn managed_toolset_permission_defaults_and_overrides_match_the_wire_contract() {
        // Cause/effect graph: C1 Agent vs MCP source selects the Managed default;
        // C2 an explicit toolset default replaces it; C3 an exact tool config
        // replaces that default. Effects are the one normalized policies used by
        // model visibility, Native authorization, and ACP authorization.
        //
        // | Rule | source | explicit default | exact override | effect |
        // | P1 | Agent | absent | absent | always_allow |
        // | P2 | MCP | absent | absent | always_ask |
        // | P3 | MCP | always_allow | absent | always_allow |
        // | P4 | MCP | always_allow | always_ask | always_ask |
        // FMECA: treating P2 like P1 silently authorizes tools added later by an
        // MCP server; applying P3 after P4 discards the most-specific authoring.
        // Constraints/invariants: exact-tool overrides outrank toolset defaults,
        // and an absent MCP policy remains fail-closed as always-ask.
        use awaken_agent_contract::ToolPermissionRequirement::{AlwaysAllow, AlwaysAsk};

        let policies = toolset_policies(&[
            AgentTool::AgentToolset20260401 {
                configs: Vec::new(),
                default_config: None,
            },
            AgentTool::McpToolset {
                mcp_server_name: "safe-default".into(),
                configs: Vec::new(),
                default_config: None,
            },
            AgentTool::McpToolset {
                mcp_server_name: "overridden".into(),
                configs: vec![AgentToolConfig {
                    name: "delete_issue".into(),
                    kind: None,
                    enabled: None,
                    permission_policy: Some(AgentToolPermissionPolicy::AlwaysAsk),
                    allowed_domains: None,
                    blocked_domains: None,
                    max_content_tokens: None,
                    user_location: None,
                }],
                default_config: Some(AgentToolDefaultConfig {
                    enabled: None,
                    permission_policy: Some(AgentToolPermissionPolicy::AlwaysAllow),
                }),
            },
        ]);

        assert_eq!(policies[0].default.permission, AlwaysAllow, "P1");
        assert_eq!(policies[1].default.permission, AlwaysAsk, "P2");
        assert_eq!(
            policies[1].policy_for("future_tool").permission,
            AlwaysAsk,
            "P2"
        );
        assert_eq!(policies[2].default.permission, AlwaysAllow, "P3");
        assert_eq!(
            policies[2].policy_for("delete_issue").permission,
            AlwaysAsk,
            "P4"
        );
    }

    #[test]
    fn managed_projection_omits_runtime_only_agent_policy_members() {
        // Cause/effect graph: C1 a neutral Session Agent policy contains one
        // official Managed member and C2 one Runtime-only member. E1 the official
        // member is projected with its closed discriminant; E2 the Runtime-only
        // member remains authorized by the source policy but is absent from the
        // Managed wire value.
        //
        // Decision table:
        // | Rule | policy member | Managed member | wire effect |
        // | M1   | read          | yes            | typed config |
        // | M2   | move          | no             | omitted      |
        // Constraint: Session policy is execution authority; the versioned
        // Managed declaration is the sole public membership authority.
        let policy = awaken_agent_contract::ToolsetPolicy {
            source: awaken_agent_contract::ToolsetSource::Agent,
            default: awaken_agent_contract::ToolExecutionPolicy {
                enabled: false,
                permission: awaken_agent_contract::ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: ["read", "move"]
                .into_iter()
                .map(|name| {
                    awaken_agent_contract::ToolPolicyOverride::new(
                        name,
                        awaken_agent_contract::ToolExecutionPolicy::default(),
                    )
                })
                .collect(),
        };

        let projected = resolved_toolsets(std::slice::from_ref(&policy));
        let AgentTool::AgentToolset20260401 { configs, .. } = &projected[0] else {
            panic!("M1 Agent toolset projection");
        };
        assert_eq!(configs.len(), 1, "M1/M2");
        assert_eq!(configs[0].name, "read", "M1");
        assert_eq!(configs[0].kind, Some(AgentToolsetMember::Read), "M1");
        assert!(policy.policy_for("move").enabled, "M2 source policy");
    }

    #[test]
    fn web_tool_discriminants_and_nullable_boundaries_match_sdk_input() {
        // Cause/effect graph: optional non-null discriminants and domain lists
        // enter the closed per-tool validator; nullable max_content_tokens enters
        // normalization. Invalid presence/type combinations fail before policy
        // construction, while a zero numeric cap survives the exact projection.
        //
        // Decision table:
        // | Rule | type | allowed_domains | max | effect |
        // | W1 | omitted | valid list | 0 | accepted, typed output keeps 0 |
        // | W2 | null | omitted | omitted | serde reject |
        // | W3 | matching | null | omitted | serde reject |
        // | W4 | mismatched | omitted | omitted | validation reject |
        // | W5 | matching | allowlist | 0 | durable JSON shape unchanged |
        // Constraints/invariants: optional is not nullable, discriminants must
        // match their member name, numeric zero remains supplied, and the one
        // opaque durable slot preserves the pre-migration wire representation.
        let authored: AgentTool = serde_json::from_value(serde_json::json!({
            "type": "agent_toolset_20260401",
            "configs": [{
                "name": "web_fetch",
                "allowed_domains": ["docs.example.com"],
                "max_content_tokens": 0
            }]
        }))
        .expect("W1");
        validate_agent_tools(std::slice::from_ref(&authored)).expect("W1");
        let policies = toolset_policies(&[authored]);
        assert_eq!(
            policies[0].configuration_for("web_fetch"),
            Some(&serde_json::json!({
                "type":"web_fetch",
                "domains":{"type":"allow", "domains":["docs.example.com"]},
                "max_content_tokens":0
            })),
            "W5"
        );
        let projected = resolved_toolsets(&policies);
        let AgentTool::AgentToolset20260401 { configs, .. } = &projected[0] else {
            panic!("W1 Agent toolset projection");
        };
        assert_eq!(configs[0].kind, Some(AgentToolsetMember::WebFetch), "W1");
        assert_eq!(configs[0].max_content_tokens, Some(0), "W1");

        for (rule, config) in [
            (
                "W2",
                serde_json::json!({ "name": "web_fetch", "type": null }),
            ),
            (
                "W3",
                serde_json::json!({
                    "name": "web_fetch", "type": "web_fetch", "allowed_domains": null
                }),
            ),
        ] {
            assert!(
                serde_json::from_value::<AgentToolConfig>(config).is_err(),
                "{rule}"
            );
        }
        let mismatched: AgentToolConfig = serde_json::from_value(serde_json::json!({
            "name": "web_fetch", "type": "web_search"
        }))
        .expect("W4 decodes before cross-field validation");
        assert!(
            validate_agent_tools(&[AgentTool::AgentToolset20260401 {
                configs: vec![mismatched],
                default_config: None,
            }])
            .is_err(),
            "W4"
        );
    }
}
