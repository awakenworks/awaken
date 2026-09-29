//! File values an environment hands back to its consumers.

use awaken_provisioning_contract as pc;

/// One process-local Artifact capture. The Sandbox remains the byte source and
/// the Artifact publisher remains the durable owner; this value only keeps
/// metadata and the exact bytes from one harvest pass together. Consumer: the
/// Host's Artifact harvest.
pub struct CapturedArtifact {
    pub metadata: pc::Artifact,
    pub bytes: Vec<u8>,
}

/// A `SKILL.md`-bearing directory discovered under the environment. Neutral file
/// data — no skill semantics — so the sandbox stays unaware of the skill model
/// (the Host parses it). `dir` is a **logical** path under the root (usable by
/// jailed tools and for `${SKILL_DIR}`), never a host absolute path. Consumer:
/// the Host's Skill discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSkillFile {
    pub id: String,
    pub content: String,
    pub dir: String,
}
