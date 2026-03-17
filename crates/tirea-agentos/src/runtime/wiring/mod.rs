//! Agent resolution and wiring: compose behaviors, tools, and plugins into a
//! runnable agent configuration.

mod behavior;
pub(crate) mod bundle_merge;
#[cfg(feature = "plan")]
pub(crate) mod plan;
pub(crate) mod resolve;
#[cfg(feature = "skills")]
pub(crate) mod skills;

pub use behavior::compose_behaviors;
pub(super) use behavior::CompositeBehavior;
pub(super) use bundle_merge::{ensure_unique_behavior_ids, merge_wiring_bundles};
#[cfg(feature = "skills")]
pub(crate) use skills::SkillsSystemWiring;
