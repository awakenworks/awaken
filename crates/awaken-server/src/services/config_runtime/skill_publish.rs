use std::collections::HashSet;

use awaken_contract::{
    AgentSpec, PreparedSkillSpecs, SkillSpec, is_skill_allowed_tool_pattern,
    parse_skill_allowed_tools, validate_skill_allowed_tool_pattern,
};
use awaken_runtime::AgentResolver;
use awaken_runtime::registry::resolve::RegistrySetResolver;
use awaken_runtime::registry::{
    RegistryDiagnostic, RegistrySet, RegistryValidationError, diagnose_agent_spec,
};

use crate::services::agent_catalog::check_catalog_errors;

use super::{ConfigRuntimeError, ConfigRuntimeManager};

impl ConfigRuntimeManager {
    pub(super) fn prepare_skill_specs(
        &self,
        skills: &[SkillSpec],
    ) -> Result<Option<Box<dyn PreparedSkillSpecs>>, ConfigRuntimeError> {
        match (&self.skill_spec_sink, skills.is_empty()) {
            (Some(sink), _) => sink
                .prepare_skill_specs(skills.to_vec())
                .map(Some)
                .map_err(ConfigRuntimeError::InvalidConfig),
            (None, true) => Ok(None),
            (None, false) => Err(ConfigRuntimeError::InvalidConfig(
                "skills config records require a configured skill_spec_sink".into(),
            )),
        }
    }

    pub(super) fn validate_skill_allowed_tools(
        &self,
        known_tool_ids: &HashSet<String>,
        skills: &[SkillSpec],
    ) -> Result<(), ConfigRuntimeError> {
        for skill in skills {
            for token in &skill.allowed_tools {
                let parsed = parse_skill_allowed_tools(token).map_err(|error| {
                    ConfigRuntimeError::InvalidConfig(format!(
                        "skill '{}' has invalid allowed_tools entry '{}': {error}",
                        skill.id, token
                    ))
                })?;
                if parsed.len() != 1 || parsed[0].raw != *token {
                    return Err(ConfigRuntimeError::InvalidConfig(format!(
                        "skill '{}' allowed_tools entry '{}' must contain exactly one token",
                        skill.id, token
                    )));
                }
                let parsed = &parsed[0];
                if is_skill_allowed_tool_pattern(&parsed.tool_id) {
                    validate_skill_allowed_tool_pattern(&parsed.tool_id).map_err(|error| {
                        ConfigRuntimeError::InvalidConfig(format!(
                            "skill '{}' has invalid allowed_tools matcher '{}': {error}",
                            skill.id, token
                        ))
                    })?;
                    continue;
                }
                if !known_tool_ids.contains(&parsed.tool_id) {
                    return Err(ConfigRuntimeError::InvalidConfig(format!(
                        "skill '{}' allowed_tools references unknown tool '{}'",
                        skill.id, parsed.tool_id
                    )));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_candidate(
        &self,
        candidate: &RegistrySet,
        local_agents: &[AgentSpec],
        skills: &[SkillSpec],
    ) -> Result<(), ConfigRuntimeError> {
        check_catalog_errors(local_agents).map_err(ConfigRuntimeError::InvalidConfig)?;
        let mut diagnostics = Vec::new();
        for model_id in candidate.models.model_ids() {
            let Some(model) = candidate.models.get_model(&model_id) else {
                continue;
            };
            let provider_id = model.provider_id;
            if candidate.providers.get_provider(&provider_id).is_none() {
                diagnostics.push(RegistryDiagnostic::ModelMissingProvider {
                    model_id,
                    provider_id,
                });
            }
        }
        for agent in local_agents {
            diagnostics.extend(diagnose_agent_spec(candidate, agent));
        }
        if !diagnostics.is_empty() {
            let err = RegistryValidationError::from_diagnostics(diagnostics).to_string();
            return Err(ConfigRuntimeError::InvalidConfig(err));
        }

        let resolver = RegistrySetResolver::new(candidate.clone());
        let mut known_tool_ids = candidate
            .tools
            .tool_ids()
            .into_iter()
            .collect::<HashSet<_>>();
        for agent in local_agents {
            if agent.endpoint.is_some() {
                continue;
            }
            let resolved = resolver.resolve(&agent.id).map_err(|error| {
                ConfigRuntimeError::InvalidConfig(format!("{}: {error}", agent.id))
            })?;
            known_tool_ids.extend(resolved.tools.keys().cloned());
        }
        self.validate_skill_allowed_tools(&known_tool_ids, skills)?;
        Ok(())
    }
}
