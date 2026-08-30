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

    /// Whether the controlled-modification authoring projection requires human
    /// approval for this capability. This versioned member enum is the single
    /// owner; UI capability metadata and Assistant authoring only project it.
    #[must_use]
    pub const fn controlled_modification(self) -> bool {
        matches!(self, Self::Bash | Self::Write | Self::Edit)
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
pub const AGENT_TOOLSET_TOOL_IDS: [&str; 8] = agent_toolset_member_ids();

const fn agent_toolset_member_ids() -> [&'static str; 8] {
    let mut ids = [""; 8];
    let mut index = 0;
    while index < AGENT_TOOLSET_MEMBERS.len() {
        ids[index] = AGENT_TOOLSET_MEMBERS[index].as_str();
        index += 1;
    }
    ids
}

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

/// Preserve Agent overrides that the closed Managed wire cannot represent.
/// `current` is the version-fenced durable authority; `replacement` contains the
/// caller's complete wire-authored canonical policy. The caller supplies ids
/// proved by its current scoped catalog and typed semantic role, so retired,
/// unknown, client, or dynamic name collisions cannot survive merely because
/// the closed wire cannot represent them. This function never creates an opaque
/// override from wire input and never changes an admitted override's bytes.
pub fn preserve_runtime_agent_overrides(
    current: &[awaken_agent_contract::ToolsetPolicy],
    replacement: &mut Vec<awaken_agent_contract::ToolsetPolicy>,
    allowed_runtime_ids: &std::collections::BTreeSet<String>,
) {
    use awaken_agent_contract::ToolsetSource;

    let Some(current_agent) = current
        .iter()
        .find(|toolset| toolset.source == ToolsetSource::Agent)
    else {
        return;
    };
    let opaque = current_agent
        .overrides
        .iter()
        .filter(|entry| {
            !is_agent_toolset_member(&entry.name)
                && allowed_runtime_ids.contains(entry.name.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    if opaque.is_empty() {
        return;
    }
    if let Some(replacement_agent) = replacement
        .iter_mut()
        .find(|toolset| toolset.source == ToolsetSource::Agent)
    {
        replacement_agent.overrides.extend(opaque);
    } else {
        replacement.push(awaken_agent_contract::ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: current_agent.default,
            overrides: opaque,
        });
    }
}

#[must_use]
pub fn is_controlled_modification_member(name: &str) -> bool {
    AGENT_TOOLSET_MEMBERS
        .iter()
        .copied()
        .find(|member| member.as_str() == name)
        .is_some_and(AgentToolsetMember::controlled_modification)
}

/// Transient authoring intent. This value is consumed while producing the one
/// typed Toolset policy and is never stored in an Agent configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPermissionPreset {
    ControlledModifications,
}

/// Borrowed fields of the existing AgentConfig authority. Keeping the pure
/// projection beside the closed member roster lets HTTP, Web, and Assistant
/// entry points share one transform without introducing another config type.
pub struct AgentPermissionPresetTarget<'a> {
    pub tool_ids: &'a mut Vec<String>,
    pub toolsets: &'a mut Vec<awaken_agent_contract::ToolsetPolicy>,
    pub mcp_servers: &'a [awaken_runtime_contract::agent_bindings::AgentMcpServerBinding],
    pub plugin_ids: &'a mut Vec<String>,
    pub plugin_config: &'a mut BTreeMap<String, serde_json::Value>,
    /// Runtime-only ids proven by the caller's current scoped catalog and typed
    /// semantic roles. Historical opaque names are not compatibility authority.
    pub allowed_runtime_override_ids: &'a std::collections::BTreeSet<String>,
}

/// Ensure every authored MCP binding has one typed fail-closed Toolset. A
/// historical generic permission document cannot be inferred and is rejected
/// before mutation; fresh bindings receive the canonical default-ask policy.
pub fn ensure_typed_mcp_toolset_policies(
    toolsets: &mut Vec<awaken_agent_contract::ToolsetPolicy>,
    mcp_servers: &[awaken_runtime_contract::agent_bindings::AgentMcpServerBinding],
    has_legacy_permission: bool,
) -> Result<(), String> {
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolsetPolicy, ToolsetSource,
    };

    let typed_servers = toolsets
        .iter()
        .filter_map(|toolset| match &toolset.source {
            ToolsetSource::Mcp { server_name } => Some(server_name.as_str()),
            ToolsetSource::Agent => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    let missing = mcp_servers
        .iter()
        .filter(|server| !typed_servers.contains(server.name.as_str()))
        .map(|server| server.name.clone())
        .collect::<Vec<_>>();
    if has_legacy_permission && !missing.is_empty() {
        return Err(format!(
            "permission migration_required: MCP servers {missing:?} have no typed MCP policy; historical plugin_config.permission semantics cannot be inferred"
        ));
    }
    for server_name in missing {
        toolsets.push(ToolsetPolicy {
            source: ToolsetSource::Mcp { server_name },
            default: ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAsk,
            },
            overrides: Vec::new(),
        });
    }
    Ok(())
}

/// Project one transient preset into the canonical typed Toolset authority.
/// Bash is enabled; write/edit retain their authored enablement; all three
/// require confirmation.
/// Git subcommands remain opaque Bash data; this transform does not parse or
/// invent a Git-specific capability.
pub fn apply_agent_permission_preset(
    target: AgentPermissionPresetTarget<'_>,
    preset: AgentPermissionPreset,
) -> Result<(), String> {
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    ensure_typed_mcp_toolset_policies(
        target.toolsets,
        target.mcp_servers,
        target.plugin_config.contains_key("permission"),
    )?;

    let existing = target
        .toolsets
        .iter()
        .find(|toolset| toolset.source == ToolsetSource::Agent)
        .cloned();
    let selected_exact = target
        .tool_ids
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut overrides = Vec::new();
    for member in bounded_agent_toolset_members() {
        let name = member.as_str();
        let existing_override = existing
            .as_ref()
            .and_then(|toolset| toolset.overrides.iter().find(|entry| entry.name == name));
        let existing_policy = existing.as_ref().map(|toolset| toolset.policy_for(name));
        let previously_enabled =
            selected_exact.contains(name) || existing_policy.is_some_and(|policy| policy.enabled);
        let enabled = match (preset, member) {
            (AgentPermissionPreset::ControlledModifications, AgentToolsetMember::Bash) => true,
            _ => previously_enabled,
        };

        if !member.controlled_modification()
            && let Some(existing_override) = existing_override
        {
            overrides.push(existing_override.clone());
            continue;
        }
        if !enabled && !member.controlled_modification() {
            continue;
        }
        let permission = match (preset, member) {
            (
                AgentPermissionPreset::ControlledModifications,
                AgentToolsetMember::Bash | AgentToolsetMember::Write | AgentToolsetMember::Edit,
            ) => ToolPermissionRequirement::AlwaysAsk,
            _ => existing_policy
                .map(|policy| policy.permission)
                .unwrap_or(ToolPermissionRequirement::AlwaysAllow),
        };
        overrides.push(ToolPolicyOverride::with_optional_configuration(
            name,
            ToolExecutionPolicy {
                enabled,
                permission,
            },
            existing
                .as_ref()
                .and_then(|toolset| toolset.configuration_for(name))
                .cloned(),
        ));
    }
    target
        .tool_ids
        .retain(|name| !is_agent_toolset_member(name));
    let mut replacement = vec![ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy {
            enabled: false,
            permission: ToolPermissionRequirement::AlwaysAllow,
        },
        overrides,
    }];
    preserve_runtime_agent_overrides(
        target.toolsets,
        &mut replacement,
        target.allowed_runtime_override_ids,
    );
    target
        .toolsets
        .retain(|toolset| toolset.source != ToolsetSource::Agent);
    target
        .toolsets
        .insert(0, replacement.pop().expect("one Agent replacement"));
    target.plugin_config.remove("permission");
    target.plugin_ids.retain(|id| id != "permission");
    Ok(())
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

/// Validate the shared MCP ownership invariant after an API adapter has applied
/// replacement/inheritance semantics: each effective server has exactly one
/// effective MCP ToolSet, and no ToolSet points outside that server set.
pub fn validate_mcp_toolset_pairing(
    server_names: &[String],
    toolset_server_names: &[String],
) -> Result<(), String> {
    let servers = server_names
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if servers.len() != server_names.len() {
        return Err("mcp_server names must be unique".into());
    }
    let toolsets = toolset_server_names
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if toolsets.len() != toolset_server_names.len() {
        return Err("each mcp_server must be referenced by exactly one mcp_toolset".into());
    }
    if servers != toolsets {
        return Err("each mcp_server must be referenced by exactly one mcp_toolset".into());
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
                // tools. They remain executable policy but have no representation
                // in the closed, versioned Managed Agent toolset.
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
    fn opaque_runtime_override_preservation_has_one_closed_member_owner() {
        // Cause/effect table:
        // | rule | current opaque | replacement Agent | effect |
        // | O1   | allowed agent_run + retired names | present | retain agent_run only |
        // | O2   | allowed agent_run + retired names | absent  | add owner with agent_run only |
        // | O3   | no allowed Runtime id             | either  | replacement unchanged |
        // Canonical members remain wire-owned, retired/unknown names cannot
        // survive by opacity, and MCP policies are never touched.
        use awaken_agent_contract::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };

        let default = ToolExecutionPolicy {
            enabled: false,
            permission: ToolPermissionRequirement::AlwaysAllow,
        };
        let canonical = ToolPolicyOverride::new("read", ToolExecutionPolicy::default());
        let opaque = ToolPolicyOverride::with_optional_configuration(
            "agent_run",
            ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAsk,
            },
            Some(serde_json::json!({ "runtime": "exact" })),
        );
        let current = vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default,
            overrides: vec![
                canonical,
                opaque.clone(),
                ToolPolicyOverride::new("delete", ToolExecutionPolicy::default()),
                ToolPolicyOverride::new("custom_dynamic", ToolExecutionPolicy::default()),
            ],
        }];
        let mcp = ToolsetPolicy {
            source: ToolsetSource::Mcp {
                server_name: "docs".into(),
            },
            default: ToolExecutionPolicy::default(),
            overrides: Vec::new(),
        };
        let mut present = vec![
            ToolsetPolicy {
                source: ToolsetSource::Agent,
                default,
                overrides: vec![ToolPolicyOverride::new(
                    "write",
                    ToolExecutionPolicy::default(),
                )],
            },
            mcp.clone(),
        ];
        let allowed = std::collections::BTreeSet::from(["agent_run".to_string()]);
        preserve_runtime_agent_overrides(&current, &mut present, &allowed);
        assert_eq!(present[0].overrides.len(), 2, "O1 canonical is not revived");
        assert_eq!(present[0].overrides[1], opaque, "O1 exact opaque");
        assert_eq!(present[1], mcp, "O1 MCP unchanged");

        let mut absent = vec![mcp.clone()];
        preserve_runtime_agent_overrides(&current, &mut absent, &allowed);
        assert_eq!(absent[0], mcp, "O2 MCP unchanged");
        assert_eq!(absent[1].source, ToolsetSource::Agent, "O2");
        assert_eq!(absent[1].default, default, "O2");
        assert_eq!(absent[1].overrides, vec![opaque], "O2");

        let mut unchanged = vec![mcp.clone()];
        preserve_runtime_agent_overrides(
            &[ToolsetPolicy {
                source: ToolsetSource::Agent,
                default,
                overrides: vec![ToolPolicyOverride::new(
                    "read",
                    ToolExecutionPolicy::default(),
                )],
            }],
            &mut unchanged,
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(unchanged, vec![mcp], "O3");
    }

    #[test]
    fn effective_mcp_pairing_covers_the_complete_bijection_table() {
        // This is shared by Agent authoring and Session-local replacements.
        // The table covers absence, exact pairing, missing, dangling, and both
        // duplicate axes so an adapter cannot accidentally weaken the contract.
        for (servers, toolsets, valid) in [
            (vec![], vec![], true),
            (vec!["docs"], vec!["docs"], true),
            (vec!["docs"], vec![], false),
            (vec![], vec!["docs"], false),
            (vec!["docs", "docs"], vec!["docs"], false),
            (vec!["docs"], vec!["docs", "docs"], false),
        ] {
            let servers = servers.into_iter().map(str::to_string).collect::<Vec<_>>();
            let toolsets = toolsets.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(
                validate_mcp_toolset_pairing(&servers, &toolsets).is_ok(),
                valid,
                "servers={servers:?}, toolsets={toolsets:?}"
            );
        }
    }

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
        // Controlled-modification cause/effect table: bash/write/edit -> ask
        // authoring metadata; every other official member -> ordinary metadata;
        // unknown ids -> false. All adapters project this one closed enum fact.
        assert_eq!(
            agent_toolset_members()
                .filter(|name| is_controlled_modification_member(name))
                .collect::<Vec<_>>(),
            vec!["bash", "write", "edit"],
            "T1 controlled projection"
        );
        assert!(!is_controlled_modification_member("parallel_web_search"));
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
    fn controlled_preset_has_one_typed_fail_closed_projection() {
        // Cause/effect graph: C1 a selected generic Bash exists; C2 read/write
        // are selected; C3 retired permission-plugin residue exists. Effects:
        // E1 Bash and write remain enabled+ask; E2 read stays enabled+allow; E3
        // no command-specific pseudo member is invented; E4 legacy residue is
        // removed; E5 replay is stable. The adjacent roster test owns exact
        // eight-member vocabulary coverage.
        //
        // Decision table:
        // | rule | member             | selected | enabled | permission |
        // | P1   | bash               | yes      | true    | ask        |
        // | P2   | read               | yes      | true    | allow      |
        // | P3   | write              | yes      | true    | ask        |
        use awaken_agent_contract::{ToolPermissionRequirement, ToolsetSource};

        let mut tool_ids = vec![
            "bash".into(),
            "read".into(),
            "write".into(),
            "custom".into(),
        ];
        let mut toolsets = Vec::new();
        let mut plugin_ids = vec!["permission".into(), "memory".into()];
        let mut plugin_config = BTreeMap::from([
            (
                "permission".into(),
                serde_json::json!({ "default_behavior": "ask" }),
            ),
            ("memory".into(), serde_json::json!({ "enabled": true })),
        ]);
        let allowed_runtime_override_ids = std::collections::BTreeSet::new();
        let apply = |tool_ids: &mut Vec<String>,
                     toolsets: &mut Vec<awaken_agent_contract::ToolsetPolicy>,
                     plugin_ids: &mut Vec<String>,
                     plugin_config: &mut BTreeMap<String, serde_json::Value>| {
            apply_agent_permission_preset(
                AgentPermissionPresetTarget {
                    tool_ids,
                    toolsets,
                    mcp_servers: &[],
                    plugin_ids,
                    plugin_config,
                    allowed_runtime_override_ids: &allowed_runtime_override_ids,
                },
                AgentPermissionPreset::ControlledModifications,
            )
            .expect("preset projection")
        };
        apply(
            &mut tool_ids,
            &mut toolsets,
            &mut plugin_ids,
            &mut plugin_config,
        );
        let agent = toolsets
            .iter()
            .find(|toolset| toolset.source == ToolsetSource::Agent)
            .expect("one Agent Toolset");
        for (rule, name, enabled, permission) in [
            ("P1", "bash", true, ToolPermissionRequirement::AlwaysAsk),
            ("P2", "read", true, ToolPermissionRequirement::AlwaysAllow),
            ("P3", "write", true, ToolPermissionRequirement::AlwaysAsk),
        ] {
            let policy = agent.policy_for(name);
            assert_eq!(
                (policy.enabled, policy.permission),
                (enabled, permission),
                "{rule}"
            );
        }
        assert_eq!(tool_ids, ["custom"], "P1-P3 use one typed owner");
        assert!(
            agent
                .overrides
                .iter()
                .all(|entry| !matches!(entry.name.as_str(), "push" | "delete")),
            "official vocabulary has no Git-specialized member"
        );
        assert_eq!(plugin_ids, ["memory"], "E4");
        assert!(!plugin_config.contains_key("permission"), "E4");
        let stable = (
            tool_ids.clone(),
            toolsets.clone(),
            plugin_ids.clone(),
            plugin_config.clone(),
        );
        apply(
            &mut tool_ids,
            &mut toolsets,
            &mut plugin_ids,
            &mut plugin_config,
        );
        assert_eq!(
            (tool_ids, toolsets, plugin_ids, plugin_config),
            stable,
            "E5"
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
        // | M2   | custom_runtime_tool | no       | omitted      |
        // Constraint: Session policy is execution authority; the versioned
        // Managed declaration is the sole public membership authority.
        let policy = awaken_agent_contract::ToolsetPolicy {
            source: awaken_agent_contract::ToolsetSource::Agent,
            default: awaken_agent_contract::ToolExecutionPolicy {
                enabled: false,
                permission: awaken_agent_contract::ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: ["read", "custom_runtime_tool"]
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
        assert!(
            policy.policy_for("custom_runtime_tool").enabled,
            "M2 source policy"
        );
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
