//! Skill subsystem for awaken.
//!
//! This module provides:
//! - `Skill`: per-skill trait with IO capabilities (read instructions, load resources, run scripts)
//! - `FsSkill`: filesystem-backed skill with directory discovery
//! - `EmbeddedSkill`: compile-time embedded skill from static content
//! - Tools: activate skill, load reference, run script
//! - `SkillDiscoveryPlugin`: inject skills catalog before inference

mod embedded;
pub mod error;
mod materialize;
#[cfg(feature = "mcp")]
pub mod mcp_bridge;
pub mod plugin;
pub mod registry;
pub mod skill;
pub mod skill_md;
pub mod state;
pub mod tools;
pub mod visibility;

/// Re-export the `AddContextMessage` scheduled action type from awaken-runtime.
pub(crate) use awaken_runtime::agent::state::AddContextMessage;

pub const SKILLS_PLUGIN_ID: &str = "skills";
pub const SKILLS_BUNDLE_ID: &str = SKILLS_PLUGIN_ID;
pub const SKILLS_DISCOVERY_PLUGIN_ID: &str = "skills-discovery";
pub const SKILLS_ACTIVE_INSTRUCTIONS_PLUGIN_ID: &str = "skills-active-instructions";

pub const SKILL_ACTIVATE_TOOL_ID: &str = "skill";
pub const SKILL_LOAD_RESOURCE_TOOL_ID: &str = "load_skill_resource";
pub const SKILL_SCRIPT_TOOL_ID: &str = "skill_script";

pub use embedded::{EmbeddedSkill, EmbeddedSkillData};
pub use error::{
    SkillError, SkillMaterializeError, SkillRegistryError, SkillRegistryManagerError, SkillWarning,
};
pub use plugin::{ActiveSkillInstructionsPlugin, SkillDiscoveryPlugin, SkillSubsystem};
pub use registry::{
    CompositeSkillRegistry, DiscoveryResult, FsSkill, FsSkillRegistryManager,
    InMemorySkillRegistry, SkillRegistry,
};
pub use skill::SkillContext;
pub use skill::{
    LoadedAsset, LoadedReference, ScriptResult, Skill, SkillActivation, SkillMeta, SkillResource,
    SkillResourceKind, collect_skills, material_key,
};
pub use skill_md::SkillArgumentDef;
pub use state::{SkillState, SkillStateUpdate, SkillStateValue};
pub use tools::{LoadSkillResourceTool, SkillActivateTool, SkillScriptTool};
pub use visibility::{
    DefaultSkillVisibilityPolicy, SkillVisibility, SkillVisibilityAction, SkillVisibilityPolicy,
    SkillVisibilityStateKey, SkillVisibilityStateValue,
};

#[cfg(feature = "mcp")]
pub use mcp_bridge::McpPromptSkillRegistryManager;
