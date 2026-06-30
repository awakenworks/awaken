//! Compilation: a pure, content-addressed config → publication function.

use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::RuntimeCatalogInstall;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ResolvedSpec, ToolDescriptor};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use sha2::{Digest, Sha256};

use crate::config::AgentConfig;

/// A compilation failure, before anything is published (the design's Failure
/// Rules: reject, never partially publish).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CompileError {
    #[error("agent {agent} references unknown tool {tool:?}")]
    UnknownTool { agent: String, tool: String },
    #[error("serialize config: {0}")]
    Serialize(String),
}

/// A compiled, content-addressed publication: the runtime install candidate and
/// the executable snapshot, both carrying the same fingerprint (ADR-0031).
#[derive(Debug, Clone)]
pub struct Publication {
    pub publication_id: String,
    pub fingerprint: String,
    pub snapshot: ExecutableAgentSnapshot,
    pub install: RuntimeCatalogInstall,
}

/// Compile an agent config against an available tool catalog. Each `tool_id` must
/// resolve (unknown references are rejected, fail-closed). The fingerprint is the
/// sha256 of the canonical config, so the same config always yields the same
/// publication — a publication is identified by its content, and the snapshot,
/// spec, and install all carry that fingerprint for the runtime to re-validate.
pub fn compile(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
) -> Result<Publication, CompileError> {
    let mut tool_descriptors = Vec::with_capacity(config.tool_ids.len());
    for id in &config.tool_ids {
        let descriptor =
            tools
                .iter()
                .find(|t| &t.id == id)
                .ok_or_else(|| CompileError::UnknownTool {
                    agent: config.id.clone(),
                    tool: id.clone(),
                })?;
        tool_descriptors.push(descriptor.clone());
    }

    let fingerprint = fingerprint_of(config)?;
    let fp = CatalogFingerprint(fingerprint.clone());

    let snapshot = ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(config.id.clone()),
        root_agent_id: AgentId(config.id.clone()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fp.clone(),
            instructions: config.instructions.clone(),
            max_steps: config.max_steps,
            model_binding: config.model_binding.clone(),
            tool_descriptors,
            plugin_ids: Vec::new(),
        },
        fingerprint: fp.clone(),
    };

    let install = RuntimeCatalogInstall {
        publication_id: fingerprint.clone(),
        fingerprint: fp,
        source_revisions: vec![config.id.clone()],
        capabilities: RuntimeCapabilityCatalog {
            catalog_fingerprint: CatalogFingerprint(fingerprint.clone()),
            runtime_version: env!("CARGO_PKG_VERSION").to_string(),
            tools: Vec::new(),
            plugins: Vec::new(),
        },
    };

    Ok(Publication {
        publication_id: fingerprint.clone(),
        fingerprint,
        snapshot,
        install,
    })
}

/// The canonical fingerprint of a config: sha256 of its serialization. The config
/// has no maps, so serialization is deterministic across runs.
fn fingerprint_of(config: &AgentConfig) -> Result<String, CompileError> {
    let bytes =
        serde_json::to_vec(config).map_err(|err| CompileError::Serialize(err.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::ModelBinding;

    fn binding() -> ModelBinding {
        ModelBinding {
            provider_instance_ref: "p".to_string(),
            model_ref: "m".to_string(),
            backend_ref: "b".to_string(),
        }
    }

    fn config(tools: &[&str]) -> AgentConfig {
        AgentConfig {
            id: "agent-1".to_string(),
            instructions: "be helpful".to_string(),
            max_steps: 8,
            model_binding: binding(),
            tool_ids: tools.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor::pinned("test", id, "a tool", serde_json::json!({"type": "object"}))
    }

    #[test]
    fn compile_is_deterministic_and_content_addressed() {
        let tools = vec![tool("echo")];
        let a = compile(&config(&["echo"]), &tools).unwrap();
        let b = compile(&config(&["echo"]), &tools).unwrap();
        assert_eq!(
            a.fingerprint, b.fingerprint,
            "same config, same fingerprint"
        );
        // The snapshot and install agree on the fingerprint the runtime validates.
        assert_eq!(a.snapshot.fingerprint.0, a.fingerprint);
        assert_eq!(
            a.snapshot.resolved_spec.catalog_fingerprint.0,
            a.fingerprint
        );
        assert_eq!(a.install.fingerprint.0, a.fingerprint);

        // A different config yields a different fingerprint.
        let mut other = config(&["echo"]);
        other.instructions = "be terse".to_string();
        assert_ne!(compile(&other, &tools).unwrap().fingerprint, a.fingerprint);
    }

    #[test]
    fn unknown_tool_reference_is_rejected() {
        let err = compile(&config(&["ghost"]), &[tool("echo")]).unwrap_err();
        assert_eq!(
            err,
            CompileError::UnknownTool {
                agent: "agent-1".to_string(),
                tool: "ghost".to_string(),
            }
        );
    }
}
