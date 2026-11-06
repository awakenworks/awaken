//! Agent Skill graph fixture constructors for the Session resource tests.

use super::*;

pub(super) fn skill_bindings(
    prefix: &str,
    count: usize,
) -> Vec<awaken_agent_contract::AgentSkillBinding> {
    (0..count)
        .map(|index| awaken_agent_contract::AgentSkillBinding::custom(format!("{prefix}-{index}")))
        .collect()
}

pub(super) fn skill_graph_source(root_count: usize, child_count: usize) -> SkillGraphAgent {
    SkillGraphAgent {
        root_count,
        child_count,
    }
}
