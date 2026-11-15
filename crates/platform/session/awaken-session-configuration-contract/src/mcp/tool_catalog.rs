// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Shared discovery snapshots use exact Workspace and immutable Environment coordinates.
//! Rebuildable stdio discovery shared by Control preview and Session startup.

use awaken_agent_contract::mcp_target::{CachedMcpTool, SandboxStdioMcpTarget};
use awaken_provisioning_contract::EnvironmentKind;
use serde::{Deserialize, Serialize};

use crate::{EnvironmentSnapshot, SessionNetworkPolicy};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StdioToolCatalogKey {
    workspace_hash: String,
    inputs_hash: String,
}

impl StdioToolCatalogKey {
    /// Select the latest observation for this authored Environment revision.
    /// Session consumers additionally compare its immutable launch fingerprint.
    #[must_use]
    pub fn for_preview(
        workspace: &str,
        target: &SandboxStdioMcpTarget,
        environment_id: &str,
        revision: u64,
    ) -> Self {
        Self {
            workspace_hash: awaken_agent_contract::content_fingerprint(workspace)
                .expect("workspace id serializes"),
            inputs_hash: awaken_agent_contract::content_fingerprint(&(
                "stdio-tools-v1",
                &target.command,
                &target.args,
                environment_id,
                revision,
            ))
            .expect("MCP launch coordinates serialize"),
        }
    }

    /// Only self-contained declarations using an immutable image may share
    /// observations. The Session must separately prove no private overlays.
    pub fn for_environment(
        workspace: &str,
        target: &SandboxStdioMcpTarget,
        environment: &EnvironmentSnapshot,
    ) -> Option<(Self, String)> {
        if workspace.is_empty()
            || !target.share_tool_catalog
            || !environment.sandbox.host_access.is_isolated()
            || !matches!(&environment.network, SessionNetworkPolicy::None)
        {
            return None;
        }
        let image = environment.prepared_image.as_deref().or_else(|| {
            match environment.sandbox.environment.as_ref()? {
                EnvironmentKind::Image { reference } => Some(reference.as_str()),
                _ => None,
            }
        })?;
        let (repository, digest) = image.rsplit_once("@sha256:")?;
        if repository.is_empty()
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        if environment.prepared_image.is_none()
            && environment
                .packages
                .manager_packages()
                .into_iter()
                .any(|(_, packages)| !packages.is_empty())
        {
            return None;
        }
        let key = Self::for_preview(
            workspace,
            target,
            &environment.environment_id,
            environment.revision.0,
        );
        let launch_fingerprint =
            awaken_agent_contract::content_fingerprint(&(&environment.config_fingerprint, image))
                .expect("frozen environment coordinates serialize");
        Some((key, launch_fingerprint))
    }

    #[must_use]
    pub fn storage_path(&self) -> String {
        format!(
            "v1/workspaces/{}/stdio-tools/{}.json",
            self.workspace_hash, self.inputs_hash
        )
    }
}

pub const STDIO_CATALOG_TTL_MS: u64 = 15 * 60 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdioToolCatalog {
    pub observed_at_unix_ms: u64,
    pub launch_fingerprint: String,
    pub tools: Vec<CachedMcpTool>,
}

impl StdioToolCatalog {
    #[must_use]
    pub fn is_fresh(&self, now: u64) -> bool {
        self.observed_at_unix_ms <= now && now - self.observed_at_unix_ms < STDIO_CATALOG_TTL_MS
    }
}

pub struct VersionedStdioToolCatalog {
    pub version: String,
    pub catalog: StdioToolCatalog,
}

#[derive(Debug, thiserror::Error)]
pub enum StdioToolCatalogError {
    #[error("tool catalog storage is unavailable")]
    Unavailable,
}

#[async_trait::async_trait]
pub trait StdioToolDiscovery: Send + Sync {
    async fn get(
        &self,
        key: &StdioToolCatalogKey,
    ) -> Result<Option<VersionedStdioToolCatalog>, StdioToolCatalogError>;

    /// Create when expected is absent, otherwise replace only that exact version.
    /// A false result means a concurrent discovery already published an observation.
    async fn put_if_version(
        &self,
        key: &StdioToolCatalogKey,
        expected: Option<&str>,
        catalog: &StdioToolCatalog,
    ) -> Result<bool, StdioToolCatalogError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::McpTarget;

    #[test]
    fn catalog_identity_and_admission_are_bound_to_authorized_launch_inputs() {
        let mut environment = EnvironmentSnapshot {
            environment_id: "env".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: false,
            config_fingerprint: crate::EnvironmentFingerprint("frozen".into()),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: Some(format!("registry/image@sha256:{}", "a".repeat(64))),
            network: SessionNetworkPolicy::None,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        let McpTarget::SandboxStdio(mut target) =
            McpTarget::sandbox_stdio("/bin/mcp", vec![]).unwrap()
        else {
            unreachable!()
        };
        assert!(StdioToolCatalogKey::for_environment("a", &target, &environment).is_none());
        target.share_tool_catalog = true;
        let (key, fingerprint) =
            StdioToolCatalogKey::for_environment("a", &target, &environment).unwrap();
        assert_eq!(
            key,
            StdioToolCatalogKey::for_preview("a", &target, "env", 1)
        );
        assert_ne!(
            key,
            StdioToolCatalogKey::for_preview("b", &target, "env", 1)
        );
        assert_ne!(
            key,
            StdioToolCatalogKey::for_preview("a", &target, "env", 2)
        );
        environment.prepared_image = Some(format!("registry/image@sha256:{}", "b".repeat(64)));
        assert_ne!(
            fingerprint,
            StdioToolCatalogKey::for_environment("a", &target, &environment)
                .unwrap()
                .1
        );
        environment.prepared_image = Some("registry/image:latest".into());
        assert!(StdioToolCatalogKey::for_environment("a", &target, &environment).is_none());
    }

    #[test]
    fn an_empty_catalog_is_a_fresh_observation_with_a_bounded_lifetime() {
        let entry = StdioToolCatalog {
            observed_at_unix_ms: 100,
            launch_fingerprint: "frozen".into(),
            tools: vec![],
        };
        assert!(!entry.is_fresh(99));
        assert!(entry.is_fresh(100));
        assert!(entry.is_fresh(99 + STDIO_CATALOG_TTL_MS));
        assert!(!entry.is_fresh(100 + STDIO_CATALOG_TTL_MS));
    }
}
