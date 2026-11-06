// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: The versioned protocol codec preserves closed tool DTOs and delegates all Web
// @formal reason: semantics to the pure Web kernel; its original admission and projection tests move with it.
//! Managed Agents protocol tool DTOs; no durable Session owner.
use awaken_session_configuration_contract::BuiltinToolsetMember as AgentToolsetMember;
use awaken_web_policy_contract::WebFetchUrlSources;
use serde::{Deserialize, Serialize};
// Preserve field presence once; T owns whether an explicit null is admissible.
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
use std::collections::BTreeMap;

/// Managed Agent tool wire projection. The protocol owns this versioned shape;
/// Session persistence uses neutral tool settings. JSON Schema keywords remain
/// the only deliberately open part of this value.
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
    // Struct variants, not unit ones. `deny_unknown_fields` on an internally
    // tagged enum governs a unit variant's siblings and not the variant: serde
    // reads the tag and discards every other key, so `{"type":"auto","x":1}`
    // would be accepted by a type that reads as closed. The braces encode
    // identically on the wire and make the rejection real.
    AlwaysAllow {},
    AlwaysAsk {},
    Auto {},
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentToolConfig {
    pub name: String,
    #[serde(
        rename = "type",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub kind: Option<AgentToolsetMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_policy: Option<AgentToolPermissionPolicy>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_content_tokens: Option<u64>,
    /// Absent authored field is retained; the Managed WebFetch response owns
    /// an explicit nullable field. Both non-null forms use one closed policy.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present"
    )]
    pub url_sources: Option<Option<WebFetchUrlSources>>,
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
