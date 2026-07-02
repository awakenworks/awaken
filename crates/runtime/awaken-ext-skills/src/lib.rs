//! Skills as a runtime extension, fronted by two semantic tools.
//!
//! A skill is specialized, repository-specific procedure the model can pull in on
//! demand. This crate implements the Claude-Code / Hermes shape adapted to the
//! runtime's neutral seams (ADR-0036, superseding ADR-0035 D4):
//!
//! - **Two tools, never per-skill tools.** [`ListSkillsTool`] (id
//!   [`SKILL_LIST_TOOL_ID`]) discovers, [`SkillTool`] (id [`SKILL_TOOL_ID`])
//!   activates. The model's skill-specific tool face is fixed at two, regardless
//!   of how many skills are offered.
//! - **Discovery is data (tier 1).** `list_skills` returns the catalog (id +
//!   description + when-to-use + provenance) as a tool *result*, not baked into a
//!   descriptor — so a changing skill set never perturbs a pinned surface.
//! - **Activation is a tool result (tier 2).** `Skill { skill, args? }` returns the
//!   instruction body, injected into the transcript. The kernel never learns the
//!   concept "skill".
//!
//! References/scripts (tier 3) and authoring are done with the built-in
//! `read`/`bash`/`write` tools over materialized skill files — no dedicated tool.
//! The registry ([`SkillRegistry`] / [`InMemorySkillRegistry`]) is the source of
//! the catalog and the activation body. Sandbox materialization, `allowed_tools`
//! gating, and conditional activation are later slices.

mod registry;
mod spec;
mod tool;

pub use registry::{InMemorySkillRegistry, SkillRegistry};
pub use spec::{SkillProvenance, SkillSpec, parse_skill_md};
pub use tool::{
    ListSkillsTool, SKILL_LIST_TOOL_ID, SKILL_TOOL_ID, SkillTool, list_skills_tool_descriptor,
    skill_tool_descriptor,
};
