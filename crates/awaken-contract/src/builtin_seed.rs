//! Carrier types for binary-supplied built-in specs ("seed").
//!
//! At startup a binary may declare a set of specs it wants present in
//! ConfigStore. The seed protocol (see `awaken_server::services::builtin_seed`)
//! upserts these idempotently and prunes obsolete builtins, while leaving
//! user-written entries untouched. This module defines only the data
//! describing a seed; the protocol lives in `awaken-server`.

use serde::{Deserialize, Serialize};

use crate::registry_spec::{AgentSpec, McpServerSpec, ModelSpec, ProviderSpec};
use crate::skill_spec::SkillSpec;
use crate::tool_spec::ToolSpec;

/// A single spec the binary wants to seed into ConfigStore.
///
/// The variant determines the target ConfigStore namespace.
///
/// `Agent` is heap-allocated via `Box` because `AgentSpec` is significantly
/// larger than the other variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuiltinSpec {
    Agent(Box<AgentSpec>),
    Provider(ProviderSpec),
    Model(ModelSpec),
    McpServer(McpServerSpec),
    Tool(ToolSpec),
    Skill(SkillSpec),
}

impl BuiltinSpec {
    /// Wrap an `AgentSpec` (heap-allocates per `clippy::large_enum_variant`).
    pub fn agent(spec: AgentSpec) -> Self {
        Self::Agent(Box::new(spec))
    }

    /// Wrap a `ProviderSpec`.
    pub fn provider(spec: ProviderSpec) -> Self {
        Self::Provider(spec)
    }

    /// Wrap a `ModelSpec`.
    pub fn model(spec: ModelSpec) -> Self {
        Self::Model(spec)
    }

    /// Wrap a `McpServerSpec`.
    pub fn mcp_server(spec: McpServerSpec) -> Self {
        Self::McpServer(spec)
    }

    /// Wrap a `ToolSpec`.
    pub fn tool(spec: ToolSpec) -> Self {
        Self::Tool(spec)
    }

    /// Wrap a `SkillSpec`.
    pub fn skill(spec: SkillSpec) -> Self {
        Self::Skill(spec)
    }

    /// ConfigStore namespace this spec belongs to.
    ///
    /// Must match the namespace strings used by `ConfigService` /
    /// `ConfigNamespace::as_str()`.
    pub fn namespace(&self) -> &'static str {
        match self {
            Self::Agent(_) => "agents",
            Self::Provider(_) => "providers",
            Self::Model(_) => "models",
            Self::McpServer(_) => "mcp-servers",
            Self::Tool(_) => "tools",
            Self::Skill(_) => "skills",
        }
    }

    /// ID under which the spec is stored in its namespace.
    pub fn id(&self) -> &str {
        match self {
            Self::Agent(s) => &s.id,
            Self::Provider(s) => &s.id,
            Self::Model(s) => &s.id,
            Self::McpServer(s) => &s.id,
            Self::Tool(s) => &s.id,
            Self::Skill(s) => &s.id,
        }
    }
}

/// A complete seed payload: the binary's version tag plus all specs it
/// wants present.
///
/// `binary_version` is compared against existing Builtin records' version
/// tag to decide whether to refresh them on this boot. Two binaries with
/// the same version string but different seed contents will trigger the
/// "same-version edit" path (acceptable for dev loop; production releases
/// should bump the version string).
#[derive(Debug, Clone)]
pub struct BuiltinSeedSet {
    pub binary_version: String,
    pub specs: Vec<BuiltinSpec>,
}

impl BuiltinSeedSet {
    /// Convenience: an empty seed for a given version (useful in tests
    /// and for binaries that ship no built-ins).
    pub fn empty(binary_version: impl Into<String>) -> Self {
        Self {
            binary_version: binary_version.into(),
            specs: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_agent_wraps_in_box() {
        let spec = AgentSpec {
            id: "a1".to_owned(),
            model_id: "gpt-4o".to_owned(),
            system_prompt: "hi".to_owned(),
            ..Default::default()
        };
        assert!(matches!(BuiltinSpec::agent(spec), BuiltinSpec::Agent(_)));
    }

    #[test]
    fn constructor_provider_wraps() {
        let spec = ProviderSpec {
            id: "p1".to_owned(),
            adapter: "openai".to_owned(),
            ..Default::default()
        };
        assert!(matches!(
            BuiltinSpec::provider(spec),
            BuiltinSpec::Provider(_)
        ));
    }

    #[test]
    fn constructor_model_wraps() {
        let spec = ModelSpec::new("m1", "openai", "gpt-4o");
        assert!(matches!(BuiltinSpec::model(spec), BuiltinSpec::Model(_)));
    }

    #[test]
    fn constructor_mcp_server_wraps() {
        let spec = McpServerSpec {
            id: "mcp1".to_owned(),
            ..Default::default()
        };
        assert!(matches!(
            BuiltinSpec::mcp_server(spec),
            BuiltinSpec::McpServer(_)
        ));
    }

    #[test]
    fn constructor_tool_wraps() {
        let spec = crate::tool_spec::ToolSpec {
            id: "t1".into(),
            name: "Tool 1".into(),
            description: "x".into(),
            ..Default::default()
        };
        assert!(matches!(BuiltinSpec::tool(spec), BuiltinSpec::Tool(_)));
    }

    #[test]
    fn tool_namespace_and_id() {
        let spec = crate::tool_spec::ToolSpec {
            id: "t1".into(),
            name: "x".into(),
            description: "x".into(),
            ..Default::default()
        };
        let bi = BuiltinSpec::tool(spec);
        assert_eq!(bi.namespace(), "tools");
        assert_eq!(bi.id(), "t1");
    }

    #[test]
    fn constructor_skill_wraps() {
        let spec = crate::skill_spec::SkillSpec {
            id: "s1".into(),
            name: "Skill 1".into(),
            description: "x".into(),
            instructions_md: "Use the skill.".into(),
            ..Default::default()
        };
        let bi = BuiltinSpec::skill(spec);
        assert_eq!(bi.namespace(), "skills");
        assert_eq!(bi.id(), "s1");
    }
}
