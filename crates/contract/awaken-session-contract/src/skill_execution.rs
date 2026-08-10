//! Exact frozen-Skill realization port owned by the Session contract.

use awaken_agent_contract::AgentSkillKind;
use awaken_resource_contract::{SkillVersion, skill_bundle_sha256};

use crate::ResolvedSkillBinding;

/// Resources application capability needed while freezing and refreshing
/// Session Skill inputs. It deliberately exposes no SkillStore CRUD/purge port.
#[async_trait::async_trait]
pub trait SkillCatalogApplication: Send + Sync {
    async fn resolve_custom(
        &self,
        workspace_id: &str,
        skill_id: &str,
        selector: &str,
    ) -> Result<ResolvedSkillBinding, awaken_resource_contract::SkillStoreError>;

    async fn snapshot_latest(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillVersion>, awaken_resource_contract::SkillStoreError>;

    async fn publish_authored(
        &self,
        workspace_id: &str,
        raw_id: &str,
        name: &str,
        description: &str,
        content: &str,
    ) -> Result<(), awaken_resource_contract::SkillStoreError>;
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("Skill bundle source: {0}")]
pub struct SkillBundleSourceError(String);

impl SkillBundleSourceError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[async_trait::async_trait]
pub trait SkillBundleSource<C: Sync = ()>: Send + Sync {
    async fn load(
        &self,
        workspace_id: &str,
        binding: &ResolvedSkillBinding,
        fence: Option<&C>,
    ) -> Result<Option<SkillVersion>, SkillBundleSourceError>;
}

pub fn validate_skill_bundle(
    binding: &ResolvedSkillBinding,
    version: SkillVersion,
) -> Result<SkillVersion, SkillBundleSourceError> {
    if binding.kind != AgentSkillKind::Custom
        || version.skill_id.as_str() != binding.skill_id
        || version.version != binding.version
        || version.bundle_sha256 != binding.bundle_sha256
    {
        return Err(SkillBundleSourceError::new(
            "returned Skill does not match the frozen binding",
        ));
    }
    let actual = skill_bundle_sha256(&version.files);
    if actual != version.bundle_sha256 {
        return Err(SkillBundleSourceError::new(format!(
            "Skill bundle digest mismatch: expected {}, received {actual}",
            version.bundle_sha256
        )));
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use awaken_resource_contract::{SkillBundleFile, SkillVersion, skill_bundle_sha256};

    use super::validate_skill_bundle;
    use crate::ResolvedSkillBinding;

    fn version() -> SkillVersion {
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"safe".to_vec(),
            executable: false,
        }];
        SkillVersion {
            id: "version-1".into(),
            skill_id: "skill-1".into(),
            version: 1,
            name: "skill".into(),
            description: String::new(),
            directory: "/skills/skill-1".into(),
            bundle_sha256: skill_bundle_sha256(&files),
            files,
            created_unix_nanos: 0,
        }
    }

    #[test]
    fn frozen_skill_validation_covers_identity_and_bytes() {
        // FMECA cause/effect decision table:
        // | Rule | kind | id/version/pin | actual bytes | Effect |
        // | S1 | custom | exact | exact | accept immutable version |
        // | S2 | Anthropic | exact | exact | reject store substitution |
        // | S3 | custom | mismatch | exact | reject frozen-binding drift |
        // | S4 | custom | exact | mutated | reject digest substitution |
        let exact = version();
        let binding = ResolvedSkillBinding {
            skill_id: exact.skill_id.to_string(),
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            version: exact.version,
            bundle_sha256: exact.bundle_sha256.clone(),
        };
        assert!(validate_skill_bundle(&binding, exact.clone()).is_ok(), "S1");
        let mut builtin = binding.clone();
        builtin.kind = awaken_agent_contract::AgentSkillKind::Anthropic;
        assert!(
            validate_skill_bundle(&builtin, exact.clone()).is_err(),
            "S2"
        );
        let mut wrong_version = binding.clone();
        wrong_version.version += 1;
        assert!(
            validate_skill_bundle(&wrong_version, exact.clone()).is_err(),
            "S3"
        );
        let mut substituted = exact;
        substituted.files[0].content = b"substituted".to_vec();
        assert!(validate_skill_bundle(&binding, substituted).is_err(), "S4");
    }
}
