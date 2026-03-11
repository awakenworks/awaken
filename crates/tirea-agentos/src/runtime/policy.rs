use crate::composition::AgentDefinition;
use crate::contracts::runtime::tool_call::Tool;
use std::collections::HashMap;
use std::sync::Arc;
use tirea_contract::RunPolicy;

pub(super) use tirea_contract::scope::{is_id_allowed, is_scope_allowed};

pub(super) fn filter_tools_in_place(
    tools: &mut HashMap<String, Arc<dyn Tool>>,
    allowed: Option<&[String]>,
    excluded: Option<&[String]>,
) {
    tools.retain(|id, _| is_id_allowed(id, allowed, excluded));
}

pub(super) fn set_runtime_policy_from_definition_if_absent(
    run_policy: &mut RunPolicy,
    definition: &AgentDefinition,
) {
    let policy = run_policy;
    policy.set_allowed_tools_if_absent(definition.allowed_tools.as_deref());
    policy.set_excluded_tools_if_absent(definition.excluded_tools.as_deref());
    policy.set_allowed_skills_if_absent(definition.allowed_skills.as_deref());
    policy.set_excluded_skills_if_absent(definition.excluded_skills.as_deref());
    policy.set_allowed_agents_if_absent(definition.allowed_agents.as_deref());
    policy.set_excluded_agents_if_absent(definition.excluded_agents.as_deref());
}
