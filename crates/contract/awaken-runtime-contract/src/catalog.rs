use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCatalogInstall {
    pub publication_id: String,
    pub fingerprint: crate::resolved::CatalogFingerprint,
    pub source_revisions: Vec<String>,
    pub capabilities: crate::capability::RuntimeCapabilityCatalog,
}

impl RuntimeCatalogInstall {
    /// Derive a catalog install from an executable snapshot's own authority.
    ///
    /// A snapshot is content-addressed and self-describing: its `fingerprint` is
    /// the publication's content address and its `resolved_spec` already lists the
    /// resolved tools and plugins. A node with no config store of its own (a
    /// database-less worker) cannot warm-install the published catalog, so it has
    /// nothing to make [`active_fingerprint`](crate) equal the dispatched
    /// snapshot's — and the fail-closed resolution gate strands the run. This
    /// projects the snapshot into a consistent install carrying that same
    /// fingerprint, so such a node resolves against the snapshot it was handed.
    ///
    /// The capability catalog is advisory (advertisement/rendering only — the
    /// runtime executes off `resolved_spec`, not off these), so tools project to
    /// their ids and plugins to their ids with a default bound; the one invariant
    /// that matters is that every fingerprint slot agrees, which is stamped here by
    /// construction so [`RuntimeCatalogInstaller::install_catalog`] accepts it.
    #[must_use]
    pub fn from_snapshot(snapshot: &crate::snapshot::ExecutableAgentSnapshot) -> Self {
        let fingerprint = snapshot.fingerprint.clone();
        let spec = &snapshot.resolved_spec;
        RuntimeCatalogInstall {
            publication_id: fingerprint.0.clone(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec![snapshot.root_agent_id.0.clone()],
            capabilities: crate::capability::RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint,
                runtime_version: env!("CARGO_PKG_VERSION").to_string(),
                tools: spec
                    .tool_descriptors
                    .iter()
                    .map(|d| crate::capability::ToolCapability { id: d.id.clone() })
                    .collect(),
                plugins: spec
                    .plugin_ids
                    .iter()
                    .map(|id| crate::capability::PluginCapability {
                        id: id.clone(),
                        schema_keys: Vec::new(),
                        config_schema: None,
                        bound: crate::plugin::CapabilityBound::default(),
                    })
                    .collect(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledCatalog {
    pub fingerprint: crate::resolved::CatalogFingerprint,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("catalog install rejected: {0}")]
    Rejected(String),
}

pub trait RuntimeCatalogInstaller {
    fn install_catalog(&self, install: RuntimeCatalogInstall) -> Result<InstalledCatalog, Error>;
}

#[cfg(test)]
mod from_snapshot_tests {
    use super::*;
    use crate::capability::ToolCapability;
    use crate::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec, ToolDescriptor};
    use crate::snapshot::{AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId};

    fn snapshot(fp: &str) -> ExecutableAgentSnapshot {
        let fingerprint = CatalogFingerprint(fp.to_string());
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("assistant".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("assistant".to_string()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fingerprint.clone(),
                instructions: "be concise".to_string(),
                max_steps: 8,
                delegation_limits: Default::default(),
                model_binding: ModelBinding::new("demo", "stub", "stub"),
                model_candidates: Vec::new(),
                tool_descriptors: vec![ToolDescriptor {
                    id: "search".to_string(),
                    description: "search the web".to_string(),
                    parameters: serde_json::json!({}),
                    content_hash: "h".to_string(),
                    recovery_policy: Default::default(),
                }],
                plugin_ids: vec!["compact".to_string()],
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint,
        }
    }

    #[test]
    fn projects_a_consistent_installable_catalog_from_the_snapshot() {
        let snap = snapshot("fp-abc");
        let install = RuntimeCatalogInstall::from_snapshot(&snap);

        // Every fingerprint slot agrees with the snapshot's — the invariant
        // `install_catalog` enforces (top-level == capabilities catalog fingerprint).
        assert_eq!(install.fingerprint, snap.fingerprint);
        assert_eq!(install.capabilities.catalog_fingerprint, snap.fingerprint);
        assert_eq!(install.publication_id, "fp-abc");
        assert_eq!(install.source_revisions, vec!["assistant".to_string()]);

        // Tools and plugins project to their ids (advisory advertisement).
        assert_eq!(
            install.capabilities.tools,
            vec![ToolCapability {
                id: "search".to_string()
            }]
        );
        assert_eq!(install.capabilities.plugins.len(), 1);
        assert_eq!(install.capabilities.plugins[0].id, "compact");
    }
}
