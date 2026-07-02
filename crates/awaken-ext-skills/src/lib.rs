//! Skills as a runtime extension, fronted by a single tool.
//!
//! A skill is specialized, repository-specific procedure the model can pull in on
//! demand. This crate implements the Claude-Code shape adapted to the runtime's
//! neutral seams (ADR-0036, superseding ADR-0035 D4):
//!
//! - **One tool, not per-skill tools.** The whole skill set is fronted by the
//!   single [`SkillTool`] (id [`SKILL_TOOL_ID`]). A per-skill tool never exists —
//!   the model's tool face carries at most one skill entry regardless of how many
//!   skills are offered.
//! - **Discovery is data.** The activatable-skill *catalog* (name + description +
//!   when-to-use) is rendered into the tool descriptor by [`skill_tool_descriptor`],
//!   so the model reads what is available; it is not a list of callable tools.
//! - **Activation is a tool result.** Calling `Skill { skill, args? }` returns the
//!   skill's instruction body, which the runtime injects into the transcript as an
//!   ordinary tool result. The kernel never learns the concept "skill".
//!
//! The registry ([`SkillRegistry`] / [`InMemorySkillRegistry`]) is the source of
//! both the catalog and the activation body. Filesystem/MCP-backed registries,
//! `allowed_tools` gating, and conditional (`paths`) activation are later slices.

mod registry;
mod spec;
mod tool;

pub use registry::{InMemorySkillRegistry, SkillRegistry};
pub use spec::{SkillSpec, parse_skill_md};
pub use tool::{SKILL_TOOL_ID, SkillTool, skill_tool_descriptor};
