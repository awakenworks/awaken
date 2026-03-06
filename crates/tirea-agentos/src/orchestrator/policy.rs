use super::AgentDefinition;
use crate::contracts::runtime::tool_call::Tool;
use std::collections::HashMap;
use std::sync::Arc;
use tirea_contract::RunConfig;

pub(super) use tirea_contract::scope::{
    is_id_allowed, is_scope_allowed, SCOPE_ALLOWED_AGENTS_KEY, SCOPE_ALLOWED_SKILLS_KEY,
    SCOPE_ALLOWED_TOOLS_KEY, SCOPE_EXCLUDED_AGENTS_KEY, SCOPE_EXCLUDED_SKILLS_KEY,
    SCOPE_EXCLUDED_TOOLS_KEY,
};

pub(super) fn filter_tools_in_place(
    tools: &mut HashMap<String, Arc<dyn Tool>>,
    allowed: Option<&[String]>,
    excluded: Option<&[String]>,
) {
    tools.retain(|id, _| is_id_allowed(id, allowed, excluded));
}

pub(super) fn set_scope_filter_if_absent(
    scope: &mut RunConfig,
    key: &str,
    values: Option<&[String]>,
) -> Result<(), tirea_contract::RunConfigError> {
    if scope.value(key).is_some() {
        return Ok(());
    }
    if let Some(values) = values {
        scope.set(key, values.to_vec())?;
    }
    Ok(())
}

pub(super) fn set_scope_filters_from_definition_if_absent(
    scope: &mut RunConfig,
    definition: &AgentDefinition,
) -> Result<(), tirea_contract::RunConfigError> {
    set_scope_filter_if_absent(
        scope,
        SCOPE_ALLOWED_TOOLS_KEY,
        definition.allowed_tools.as_deref(),
    )?;
    set_scope_filter_if_absent(
        scope,
        SCOPE_EXCLUDED_TOOLS_KEY,
        definition.excluded_tools.as_deref(),
    )?;
    set_scope_filter_if_absent(
        scope,
        SCOPE_ALLOWED_SKILLS_KEY,
        definition.allowed_skills.as_deref(),
    )?;
    set_scope_filter_if_absent(
        scope,
        SCOPE_EXCLUDED_SKILLS_KEY,
        definition.excluded_skills.as_deref(),
    )?;
    set_scope_filter_if_absent(
        scope,
        SCOPE_ALLOWED_AGENTS_KEY,
        definition.allowed_agents.as_deref(),
    )?;
    set_scope_filter_if_absent(
        scope,
        SCOPE_EXCLUDED_AGENTS_KEY,
        definition.excluded_agents.as_deref(),
    )?;
    Ok(())
}
