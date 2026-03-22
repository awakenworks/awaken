//! Skill subsystem (agentskills-style).
//!
//! This module provides:
//! - `Skill`: per-skill trait with IO capabilities (read instructions, load resources, run scripts)
//! - `FsSkill`: filesystem-backed skill with directory discovery
//! - `EmbeddedSkill`: compile-time embedded skill from static content
//! - Tools: activate skill, load reference, run script
//! - `SkillDiscoveryPlugin`: inject skills catalog before inference

mod discovery_plugin;
mod embedded_registry;
mod materialize;
mod mcp_registry;
mod registry;
mod skill_md;
mod subsystem;
mod tools;
mod types;

pub const SKILLS_PLUGIN_ID: &str = "skills";
pub const SKILLS_BUNDLE_ID: &str = SKILLS_PLUGIN_ID;
pub const SKILLS_DISCOVERY_PLUGIN_ID: &str = "skills_discovery";

pub const SKILL_ACTIVATE_TOOL_ID: &str = "skill";
pub const SKILL_LOAD_RESOURCE_TOOL_ID: &str = "load_skill_resource";
pub const SKILL_SCRIPT_TOOL_ID: &str = "skill_script";

pub use discovery_plugin::SkillDiscoveryPlugin;
pub use embedded_registry::{EmbeddedSkill, EmbeddedSkillData};
pub use mcp_registry::{McpPromptSkill, McpPromptSkillRegistryManager};
pub use registry::{
    CompositeSkillRegistry, DiscoveryResult, FsSkill, FsSkillRegistryManager,
    InMemorySkillRegistry, SkillRegistry, SkillRegistryError, SkillRegistryManagerError,
};
pub use subsystem::{SkillSubsystem, SkillSubsystemError};
pub use tools::{LoadSkillResourceTool, SkillActivateTool, SkillScriptTool};
pub use types::{
    collect_skills, material_key, LoadedAsset, LoadedReference, ScriptResult, Skill,
    SkillActivation, SkillError, SkillMaterializeError, SkillMeta, SkillResource,
    SkillResourceKind, SkillState, SkillStateAction, SkillWarning,
};
