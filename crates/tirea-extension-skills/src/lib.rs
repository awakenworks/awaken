//! Skill subsystem (agentskills-style).
//!
//! This module provides:
//! - `Skill`: per-skill trait with IO capabilities (read instructions, load resources, run scripts)
//! - `FsSkill`: filesystem-backed skill with directory discovery
//! - `EmbeddedSkill`: compile-time embedded skill from static content
//! - Tools: activate skill, load reference, run script
//! - `SkillDiscoveryPlugin`: inject skills catalog before inference

mod combined_plugin;
mod discovery_plugin;
mod embedded_registry;
mod materialize;
mod registry;
mod runtime_plugin;
mod skill_md;
mod subsystem;
mod tool_filter;
mod tools;
mod types;

pub const SKILLS_PLUGIN_ID: &str = "skills";
pub const SKILLS_BUNDLE_ID: &str = SKILLS_PLUGIN_ID;
pub const SKILLS_DISCOVERY_PLUGIN_ID: &str = "skills_discovery";
pub const SKILLS_RUNTIME_PLUGIN_ID: &str = "skills_runtime";

pub const SKILL_ACTIVATE_TOOL_ID: &str = "skill";
pub const SKILL_LOAD_RESOURCE_TOOL_ID: &str = "load_skill_resource";
pub const SKILL_SCRIPT_TOOL_ID: &str = "skill_script";

pub use combined_plugin::SkillPlugin;
pub use discovery_plugin::SkillDiscoveryPlugin;
pub use embedded_registry::{EmbeddedSkill, EmbeddedSkillData};
pub use registry::{
    CompositeSkillRegistry, DiscoveryResult, FsSkill, FsSkillRegistryManager,
    InMemorySkillRegistry, SkillRegistry, SkillRegistryError, SkillRegistryManagerError,
};
pub use runtime_plugin::SkillRuntimePlugin;
pub use subsystem::{SkillSubsystem, SkillSubsystemError};
pub use tool_filter::{SCOPE_ALLOWED_SKILLS_KEY, SCOPE_EXCLUDED_SKILLS_KEY};
pub use tools::{LoadSkillResourceTool, SkillActivateTool, SkillScriptTool};
pub use types::{
    collect_skills, material_key, LoadedAsset, LoadedReference, ScriptResult, Skill, SkillError,
    SkillMaterializeError, SkillMeta, SkillResource, SkillResourceKind, SkillState,
    SkillStateAction, SkillWarning,
};
