//! Compilation: a pure config → runnable-config function.

use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::runnable::RunnableConfig;
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

/// Compile an agent config against an available tool catalog into a
/// content-addressed [`RunnableConfig`] the runtime can run. Each `tool_id` must
/// resolve (unknown references are rejected, fail-closed). The fingerprint is the
/// sha256 of the canonical config, so the same config always compiles to the same
/// runnable config.
///
/// This is a thin wrapper over [`RunnableConfig::builder`]: it resolves tool ids to
/// descriptors and stamps the content hash; the builder does the assembly, so the
/// compiled path and the direct path share one assembly (no duplication).
pub fn compile(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
) -> Result<RunnableConfig, CompileError> {
    let mut descriptors = Vec::with_capacity(config.tool_ids.len());
    for id in &config.tool_ids {
        let descriptor =
            tools
                .iter()
                .find(|t| &t.id == id)
                .ok_or_else(|| CompileError::UnknownTool {
                    agent: config.id.clone(),
                    tool: id.clone(),
                })?;
        descriptors.push(descriptor.clone());
    }

    Ok(RunnableConfig::builder(&config.id)
        .instructions(&config.instructions)
        .model(config.model_binding.clone())
        .max_steps(config.max_steps)
        .tools(descriptors)
        .plugins(config.plugin_ids.clone())
        .plugin_config(config.plugin_config.clone())
        .fingerprint(fingerprint_of(config)?)
        .build())
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

    fn config(tools: &[&str]) -> AgentConfig {
        AgentConfig {
            id: "agent-1".to_string(),
            instructions: "be helpful".to_string(),
            max_steps: 8,
            model_binding: ModelBinding::new("p", "m", "b"),
            tool_ids: tools.iter().map(|s| s.to_string()).collect(),
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
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
        let fp = a.snapshot().fingerprint.0.clone();
        assert_eq!(
            fp,
            b.snapshot().fingerprint.0,
            "same config, same fingerprint"
        );

        // The snapshot and install agree on the fingerprint the runtime validates.
        assert_eq!(a.snapshot().resolved_spec.catalog_fingerprint.0, fp);
        assert_eq!(a.install().fingerprint.0, fp);

        // A different config yields a different fingerprint.
        let mut other = config(&["echo"]);
        other.instructions = "be terse".to_string();
        assert_ne!(
            compile(&other, &tools).unwrap().snapshot().fingerprint.0,
            fp
        );
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

    #[test]
    fn compile_carries_plugin_ids_and_config_sections() {
        let mut cfg = config(&["echo"]);
        cfg.plugin_ids = vec!["state_machine".to_string()];
        cfg.plugin_config.insert(
            "state_machine".to_string(),
            serde_json::json!({"machines": []}),
        );
        let runnable = compile(&cfg, &[tool("echo")]).unwrap();
        let spec = &runnable.snapshot().resolved_spec;
        assert_eq!(spec.plugin_ids, vec!["state_machine".to_string()]);
        assert_eq!(
            spec.plugin_config.get("state_machine"),
            Some(&serde_json::json!({"machines": []}))
        );
        // The sections are part of the fingerprinted config surface.
        let mut other = config(&["echo"]);
        other.plugin_config.insert(
            "state_machine".to_string(),
            serde_json::json!({"machines": [{"name": "m"}]}),
        );
        assert_ne!(
            runnable.snapshot().fingerprint.0,
            compile(&other, &[tool("echo")])
                .unwrap()
                .snapshot()
                .fingerprint
                .0
        );
    }
}
