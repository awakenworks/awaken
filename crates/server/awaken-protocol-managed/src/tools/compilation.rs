// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: The versioned protocol codec preserves closed tool DTOs and delegates all Web
// @formal reason: semantics to the pure Web kernel; its original admission and projection tests move with it.
//! Managed tool compilation and projection at the versioned wire boundary.
use super::{
    AgentTool, AgentToolConfig, AgentToolDefaultConfig, AgentToolPermissionPolicy,
    AgentWebSearchUserLocation, AgentWebSearchUserLocationKind,
};
use awaken_agent_contract::{
    ClientToolDescriptor, ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride,
    ToolsetPolicy, ToolsetSource,
};
use awaken_session_configuration_contract::{
    BuiltinToolsetMember, SessionToolConfiguration, agent_toolset_members, is_agent_toolset_member,
    toolset_default_policy,
};
use awaken_web_policy_contract::{
    WebDomainFilter, WebExecutionConfiguration, WebFetchExecutionConfiguration,
    WebSearchExecutionConfiguration, WebSearchUserLocation,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedToolCompileError {
    pub path: String,
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for ManagedToolCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Preserve the existing public unknown-member response. Web semantic
        // failures retain their field path and stable classification below.
        if self.code == "unknown_agent_tool" {
            return self.message.fmt(f);
        }
        write!(f, "{} [{}]: {}", self.path, self.code, self.message)
    }
}
impl std::error::Error for ManagedToolCompileError {}
fn wire_error(path: &str, code: &str, message: impl Into<String>) -> ManagedToolCompileError {
    ManagedToolCompileError {
        path: path.into(),
        code: code.into(),
        message: message.into(),
    }
}
fn permission(value: AgentToolPermissionPolicy) -> ToolPermissionRequirement {
    match value {
        AgentToolPermissionPolicy::AlwaysAllow {} => ToolPermissionRequirement::AlwaysAllow,
        AgentToolPermissionPolicy::AlwaysAsk {} => ToolPermissionRequirement::AlwaysAsk,
        AgentToolPermissionPolicy::Auto {} => ToolPermissionRequirement::Auto,
    }
}
fn wire_permission(value: ToolPermissionRequirement) -> AgentToolPermissionPolicy {
    match value {
        ToolPermissionRequirement::AlwaysAllow => AgentToolPermissionPolicy::AlwaysAllow {},
        ToolPermissionRequirement::AlwaysAsk => AgentToolPermissionPolicy::AlwaysAsk {},
        ToolPermissionRequirement::Auto => AgentToolPermissionPolicy::Auto {},
    }
}
pub(super) fn managed_toolset_default_policy(
    source: &ToolsetSource,
    configured: Option<&AgentToolDefaultConfig>,
) -> ToolExecutionPolicy {
    toolset_default_policy(
        source,
        configured.and_then(|value| value.enabled),
        configured
            .and_then(|value| value.permission_policy)
            .map(permission),
    )
}

fn compile_config(
    config: &AgentToolConfig,
    agent: bool,
    clients: &std::collections::BTreeSet<&str>,
) -> Result<Option<serde_json::Value>, ManagedToolCompileError> {
    let member = agent
        .then(|| agent_toolset_members().find(|name| *name == config.name))
        .flatten();
    if agent && member.is_none() {
        return Err(wire_error(
            "name",
            "unknown_agent_tool",
            format!("unknown agent tool `{}`", config.name),
        ));
    }
    if config.kind.is_some() && config.kind.map(BuiltinToolsetMember::as_str) != member {
        return Err(wire_error(
            "type",
            "tool_type_name_mismatch",
            format!("tool config type does not match name `{}`", config.name),
        ));
    }
    let web_fields = [
        ("allowed_domains", config.allowed_domains.is_some()),
        ("blocked_domains", config.blocked_domains.is_some()),
        ("max_content_tokens", config.max_content_tokens.is_some()),
        ("url_sources", config.url_sources.is_some()),
        ("user_location", config.user_location.is_some()),
    ];
    for (field, present) in web_fields {
        let allowed = match member {
            Some("web_fetch") => field != "user_location",
            Some("web_search") => !matches!(field, "url_sources" | "max_content_tokens"),
            _ => false,
        };
        if present && !allowed {
            return Err(wire_error(
                field,
                "wrong_tool_configuration_field",
                format!("tool `{}` cannot define {field}", config.name),
            ));
        }
    }
    if !web_fields.iter().any(|(_, present)| *present) {
        return Ok(None);
    }
    let domains = WebDomainFilter::from_lists(
        config.allowed_domains.as_deref(),
        config.blocked_domains.as_deref(),
    )
    .map_err(|error| kernel_error(error, true))?;
    let configuration = match member {
        Some("web_fetch") => WebExecutionConfiguration::WebFetch(WebFetchExecutionConfiguration {
            domains,
            max_content_tokens: config.max_content_tokens,
            url_sources: config.url_sources.clone().flatten(),
        }),
        Some("web_search") => {
            WebExecutionConfiguration::WebSearch(WebSearchExecutionConfiguration {
                domains,
                user_location: config.user_location.as_ref().map(|location| {
                    WebSearchUserLocation {
                        city: location.city.clone(),
                        country: location.country.clone(),
                        region: location.region.clone(),
                        timezone: location.timezone.clone(),
                    }
                }),
            })
        }
        _ => unreachable!("wire fields were checked against their owner"),
    };
    let canonical = configuration
        .compile(Some(clients))
        .map_err(|error| kernel_error(error, config.allowed_domains.is_some()))?;
    // Explicit null url_sources remains a wire-presence fact, not a restriction.
    if config.allowed_domains.is_none()
        && config.blocked_domains.is_none()
        && config.max_content_tokens.is_none()
        && config.url_sources.as_ref().is_none_or(Option::is_none)
        && config.user_location.is_none()
    {
        return Ok(None);
    }
    Ok(Some(canonical))
}
fn kernel_error(
    error: awaken_web_policy_contract::WebConfigurationIssue,
    allowed: bool,
) -> ManagedToolCompileError {
    let domain_field = if allowed {
        "allowed_domains"
    } else {
        "blocked_domains"
    };
    let path = error
        .path
        .strip_prefix("domains.domains")
        .map(|tail| format!("{domain_field}{tail}"))
        .unwrap_or(error.path);
    wire_error(&path, error.code.as_str(), error.message)
}

/// Validate every authored config before first-duplicate-wins folding; return
/// one canonical neutral value and the exact custom-tool roster in this call.
pub fn compile_agent_tools(
    tools: &[AgentTool],
) -> Result<SessionToolConfiguration, ManagedToolCompileError> {
    let clients = tools
        .iter()
        .filter_map(|tool| match tool {
            AgentTool::Custom { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let mut toolsets = Vec::new();
    let mut client_tools = Vec::new();
    for (tool_index, tool) in tools.iter().enumerate() {
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
            AgentTool::Custom {
                name,
                description,
                input_schema,
            } => {
                client_tools.push(ClientToolDescriptor {
                    name: name.clone(),
                    description: description.clone(),
                    input_schema: serde_json::to_value(input_schema)
                        .expect("closed schema serializes"),
                });
                continue;
            }
        };
        let compiled = configs
            .iter()
            .enumerate()
            .map(|(index, config)| {
                compile_config(config, source == ToolsetSource::Agent, &clients).map_err(
                    |mut error| {
                        error.path = format!("tools[{tool_index}].configs[{index}].{}", error.path);
                        error
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let default = managed_toolset_default_policy(&source, default_config);
        let one = |name: &str, found: Option<usize>| {
            let config = found.map(|index| &configs[index]);
            ToolPolicyOverride::with_optional_configuration(
                name,
                ToolExecutionPolicy {
                    enabled: config
                        .and_then(|value| value.enabled)
                        .unwrap_or(default.enabled),
                    permission: config
                        .and_then(|value| value.permission_policy)
                        .map(permission)
                        .unwrap_or(default.permission),
                },
                found.and_then(|index| compiled[index].clone()),
            )
        };
        let overrides = match source {
            ToolsetSource::Agent => agent_toolset_members()
                .map(|name| one(name, configs.iter().position(|config| config.name == name)))
                .collect(),
            ToolsetSource::Mcp { .. } => configs
                .iter()
                .enumerate()
                .map(|(index, config)| one(&config.name, Some(index)))
                .collect(),
        };
        toolsets.push(ToolsetPolicy {
            source,
            default,
            overrides,
        });
    }
    Ok(SessionToolConfiguration {
        toolsets,
        client_tools,
    })
}

/// Interpret only Agent-owned Web values. Other opaque tool settings, including
/// same-name MCP configurations, are never decoded as Managed Web policy.
pub fn resolved_toolsets(
    policies: &[ToolsetPolicy],
) -> Result<Vec<AgentTool>, ManagedToolCompileError> {
    policies
        .iter()
        .enumerate()
        .map(|(index, policy)| {
            let configs = policy
                .overrides
                .iter()
                .filter(|entry| {
                    policy.source != ToolsetSource::Agent || is_agent_toolset_member(&entry.name)
                })
                .filter(|entry| entry.policy != policy.default || entry.configuration.is_some())
                .map(|entry| {
                    let kind = (policy.source == ToolsetSource::Agent).then(|| {
                        serde_json::from_value::<BuiltinToolsetMember>(serde_json::json!(
                            entry.name
                        ))
                        .expect("closed builtin member")
                    });
                    let mut config = AgentToolConfig {
                        name: entry.name.clone(),
                        kind,
                        enabled: Some(entry.policy.enabled),
                        permission_policy: Some(wire_permission(entry.policy.permission)),
                        allowed_domains: None,
                        blocked_domains: None,
                        max_content_tokens: None,
                        url_sources: (policy.source == ToolsetSource::Agent
                            && entry.name == "web_fetch")
                            .then_some(None),
                        user_location: None,
                    };
                    if policy.source == ToolsetSource::Agent
                        && matches!(entry.name.as_str(), "web_fetch" | "web_search")
                        && let Some(value) = &entry.configuration
                    {
                        let configuration =
                            WebExecutionConfiguration::parse(&entry.name, value, None).map_err(
                                |error| {
                                    let mut error = kernel_error(error, true);
                                    error.path =
                                        format!("toolsets[{index}].{}.{}", entry.name, error.path);
                                    error
                                },
                            )?;
                        let domains = match configuration {
                            WebExecutionConfiguration::WebFetch(value) => {
                                config.max_content_tokens = value.max_content_tokens;
                                config.url_sources = Some(value.url_sources);
                                value.domains
                            }
                            WebExecutionConfiguration::WebSearch(value) => {
                                config.user_location = value.user_location.map(|location| {
                                    AgentWebSearchUserLocation {
                                        kind: AgentWebSearchUserLocationKind::Approximate,
                                        city: location.city,
                                        country: location.country,
                                        region: location.region,
                                        timezone: location.timezone,
                                    }
                                });
                                value.domains
                            }
                        };
                        match domains {
                            Some(WebDomainFilter::Allow(domains)) => {
                                config.allowed_domains = Some(domains)
                            }
                            Some(WebDomainFilter::Block(domains)) => {
                                config.blocked_domains = Some(domains)
                            }
                            None => {}
                        }
                    }
                    Ok(config)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let default_config = Some(AgentToolDefaultConfig {
                enabled: Some(policy.default.enabled),
                permission_policy: Some(wire_permission(policy.default.permission)),
            });
            Ok(match &policy.source {
                ToolsetSource::Agent => AgentTool::AgentToolset20260401 {
                    configs,
                    default_config,
                },
                ToolsetSource::Mcp { server_name } => AgentTool::McpToolset {
                    mcp_server_name: server_name.clone(),
                    configs,
                    default_config,
                },
            })
        })
        .collect()
}
