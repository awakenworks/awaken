// @formal entrypoint skill.invalid_workspace_or_hash_fails_closed
// @formal entrypoint skill.loaded_version_equals_session_pin
//! Exact frozen Skill bundle reads; catalog selection remains in the Host.

use super::ResourceStaging;
use crate::skill_delivery::anthropic_skill;
use awaken_agent_contract::AgentSkillKind;
use awaken_resource_contract::{SkillStoreError, SkillVersion};
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_configuration_contract::resource::ResolvedSkillBinding;
use awaken_session_realization_contract::ResourceOperationFence;

impl ResourceStaging {
    pub(crate) async fn load_pinned(
        &self,
        workspace: &str,
        bindings: &[ResolvedSkillBinding],
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization: Option<(&str, u64, &session_resource::SessionRealizationLease)>,
    ) -> Result<Vec<SkillVersion>, SkillStoreError> {
        if bindings.is_empty() {
            return Ok(Vec::new());
        }
        let mut versions = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let version = match binding.kind {
                AgentSkillKind::Anthropic => anthropic_skill(&binding.skill_id)
                    .filter(|version| version.version == binding.version)
                    .ok_or_else(|| SkillStoreError::NotFound(binding.skill_id.clone()))?,
                AgentSkillKind::Custom => {
                    let authority = match (claim, realization) {
                        (_, Some((session_id, revision, lease))) => Some(
                            ResourceOperationFence::SessionMaterialization(
                                awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new_skill(
                                    session_id,
                                    revision,
                                    binding.clone(),
                                    lease.clone(),
                                )
                                .map_err(|error| SkillStoreError::InvalidSelection {
                                    detail: error,
                                })?,
                            ),
                        ),
                        (Some(claim), None) => {
                            Some(ResourceOperationFence::Run(claim.clone()))
                        }
                        (None, None) => None,
                    };
                    self.bundle_source
                        .as_ref()
                        .ok_or_else(|| SkillStoreError::AuthorityUnavailable {
                            detail: "no Skill bundle source".into(),
                        })?
                        .load(workspace, binding, authority.as_ref())
                        .await
                        .map_err(|error| SkillStoreError::Indeterminate {
                            detail: error.to_string(),
                        })?
                        .ok_or_else(|| SkillStoreError::NotFound(binding.skill_id.clone()))?
                }
            };
            if version.bundle_sha256 != binding.bundle_sha256 {
                return Err(SkillStoreError::CorruptData {
                    detail: format!(
                        "Skill {} version {} hash changed",
                        binding.skill_id, binding.version
                    ),
                });
            }
            versions.push(version);
        }
        Ok(versions)
    }
}

#[cfg(test)]
mod tests;
