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
    /// The config's model is still `ModelSelection::Auto` (ADR-0052 D5): compile is
    /// pure and cannot reach the provider catalog, so an `Auto` binding must be
    /// resolved to a concrete one *before* compile (publish does this). Fail-closed.
    #[error("agent {agent} has an unresolved (auto) model binding; resolve it before compiling")]
    UnresolvedModel { agent: String },
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
    compile_with_resource_prompts(config, tools, &[])
}

/// Compile with per-binding **resource prompts** appended to the agent's system
/// prompt (ADR-0038 A3a): the config→runtime boundary is where a bound resource's
/// description — the outputs path, a memory store's instructions, a repo's branch, a
/// skill's purpose — is injected into the agent's effective instructions, not at
/// runtime per turn. Passing `&[]` is exactly [`compile`] (fingerprint included), so
/// bare compilation is byte-identical to before resources existed. Each fragment
/// also enters the content-address fingerprint, so a different prompt set compiles to
/// a different runnable (no stale cache hit on changed instructions).
pub fn compile_with_resource_prompts(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
    resource_prompts: &[String],
) -> Result<RunnableConfig, CompileError> {
    let mut descriptors = Vec::with_capacity(config.tool_ids.len());
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Exact tool ids: each must resolve (unknown references are rejected, fail-closed).
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
        seen.insert(id.clone());
    }
    // Glob patterns: permissively add catalog tools whose id matches, in catalog
    // order, skipping any already selected. A pattern that matches nothing is not
    // an error — it is a filter over the catalog, not a reference to a tool.
    for descriptor in tools {
        if seen.contains(&descriptor.id) {
            continue;
        }
        if config
            .tool_patterns
            .iter()
            .any(|pattern| glob_match(pattern, &descriptor.id))
        {
            descriptors.push(descriptor.clone());
            seen.insert(descriptor.id.clone());
        }
    }

    // The model must be concrete by now: `Auto` is resolved to a first-offering in
    // `ConfigService::publish` before compile (ADR-0052 D5). A bare compile of an
    // `Auto` config is fail-closed (`UnresolvedModel`), never a silent empty binding.
    let model = config
        .model_binding
        .resolved()
        .ok_or_else(|| CompileError::UnresolvedModel {
            agent: config.id.clone(),
        })?
        .clone();

    Ok(RunnableConfig::builder(&config.id)
        .instructions(compose_instructions(&config.instructions, resource_prompts))
        .model(model)
        .model_candidates(config.model_candidates.clone())
        .max_steps(config.max_steps)
        .tools(descriptors)
        .plugins(config.plugin_ids.clone())
        .plugin_config(config.plugin_config.clone())
        .context_policy(config.context_policy.clone())
        .fingerprint(fingerprint_of(config, resource_prompts)?)
        .build())
}

/// Match a tool id against a glob `pattern` whose only metacharacter is `*` (each
/// `*` matches any run of characters, including empty). Anchored at both ends, so
/// `fs_*` matches `fs_read` but not `net_fs`. Byte-wise ASCII matching — tool ids
/// are ASCII identifiers.
fn glob_match(pattern: &str, id: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            Some(&c) => !s.is_empty() && s[0] == c && go(&p[1..], &s[1..]),
        }
    }
    go(pattern.as_bytes(), id.as_bytes())
}

/// The agent's **effective** system prompt: its base `instructions` followed by one
/// block per bound-resource prompt, blank-line separated. Empty `resource_prompts`
/// returns the base verbatim (byte-identical to pre-resource behavior).
#[must_use]
pub fn compose_instructions(base: &str, resource_prompts: &[String]) -> String {
    if resource_prompts.is_empty() {
        return base.to_string();
    }
    let mut out = String::from(base);
    for fragment in resource_prompts {
        out.push_str("\n\n");
        out.push_str(fragment);
    }
    out
}

/// The canonical fingerprint: sha256 of the config serialization, extended by the
/// resource prompts when present. Empty prompts hash exactly the config bytes, so a
/// bare compile keeps its prior content address; a non-empty prompt set changes it
/// (a different effective system prompt is a different runnable). The config has no
/// maps, so serialization is deterministic across runs.
fn fingerprint_of(
    config: &AgentConfig,
    resource_prompts: &[String],
) -> Result<String, CompileError> {
    let mut bytes =
        serde_json::to_vec(config).map_err(|err| CompileError::Serialize(err.to_string()))?;
    if !resource_prompts.is_empty() {
        let extra = serde_json::to_vec(resource_prompts)
            .map_err(|err| CompileError::Serialize(err.to_string()))?;
        bytes.extend_from_slice(&extra);
    }
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::ModelBinding;

    use crate::config::ModelSelection;

    fn config(tools: &[&str]) -> AgentConfig {
        AgentConfig {
            id: "agent-1".to_string(),
            instructions: "be helpful".to_string(),
            max_steps: 8,
            model_binding: ModelSelection::pinned("p", "m", "b"),
            tool_ids: tools.iter().map(|s| s.to_string()).collect(),
            model_candidates: Vec::new(),
            plugin_ids: Vec::new(),
            plugin_config: Default::default(),
            context_policy: awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
            tool_patterns: Vec::new(),
            ..Default::default()
        }
    }

    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor::pinned("test", id, "a tool", serde_json::json!({"type": "object"}))
    }

    #[test]
    fn model_candidates_compile_into_the_resolved_pool_and_enter_the_fingerprint() {
        let tools = vec![tool("echo")];

        let mut pooled = config(&["echo"]);
        pooled.model_candidates = vec![ModelBinding::new("p", "fallback", "b")];
        let compiled = compile(&pooled, &tools).unwrap();
        let spec = &compiled.snapshot().resolved_spec;
        // The pool is carried onto the resolved spec: primary + one fallback.
        assert_eq!(spec.model_candidates.len(), 1);
        assert_eq!(spec.candidate_bindings().len(), 2);
        assert_eq!(spec.candidate_bindings()[1].model_ref, "fallback");

        // A pool enters the content address (a different pool is a different runnable);
        // an empty pool (the default) leaves the fingerprint byte-identical.
        let single = compile(&config(&["echo"]), &tools).unwrap();
        assert_ne!(
            compiled.snapshot().fingerprint.0,
            single.snapshot().fingerprint.0,
            "a pool must change the content address"
        );
        assert!(single.snapshot().resolved_spec.model_candidates.is_empty());
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
    fn context_policy_flows_into_the_compiled_spec_and_fingerprint() {
        use awaken_runtime_contract::resolved::ContextPolicy;
        let mut cfg = config(&[]);
        cfg.context_policy = ContextPolicy::KeepLast { keep_last: 3 };
        let compiled = compile(&cfg, &[]).unwrap();
        assert_eq!(
            compiled.snapshot().resolved_spec.context_policy,
            ContextPolicy::KeepLast { keep_last: 3 }
        );
        // The policy is part of the content address: changing it changes the hash.
        let default_fp = compile(&config(&[]), &[])
            .unwrap()
            .snapshot()
            .fingerprint
            .0
            .clone();
        assert_ne!(compiled.snapshot().fingerprint.0, default_fp);
    }

    #[test]
    fn tool_patterns_select_matching_catalog_tools_and_enter_the_fingerprint() {
        let catalog = vec![tool("fs_read"), tool("fs_write"), tool("net_get")];

        let mut cfg = config(&["net_get"]); // one exact id
        cfg.tool_patterns = vec!["fs_*".to_string()]; // plus a glob
        let spec = compile(&cfg, &catalog).unwrap();
        let ids: Vec<String> = spec
            .snapshot()
            .resolved_spec
            .tool_descriptors
            .iter()
            .map(|d| d.id.clone())
            .collect();
        // Exact id kept, both fs_* tools selected, net_get not double-added.
        assert_eq!(ids, vec!["net_get", "fs_read", "fs_write"]);

        // A pattern matching nothing is not an error (unlike an unknown tool_id).
        let mut nomatch = config(&[]);
        nomatch.tool_patterns = vec!["zzz_*".to_string()];
        assert!(compile(&nomatch, &catalog).is_ok());

        // Empty patterns keep the fingerprint byte-identical to before the field.
        let plain_fp = compile(&config(&["net_get"]), &catalog)
            .unwrap()
            .snapshot()
            .fingerprint
            .0
            .clone();
        // A non-empty pattern set enters the content address.
        assert_ne!(spec.snapshot().fingerprint.0, plain_fp);
    }

    #[test]
    fn auto_binding_fails_closed_at_compile() {
        // ADR-0052 D5: compile is pure and cannot resolve `Auto` — it must be
        // resolved to a concrete binding by publish first, so a bare compile of an
        // Auto config is rejected (never a silent empty binding).
        let mut cfg = config(&[]);
        cfg.model_binding = ModelSelection::Auto;
        assert_eq!(
            compile(&cfg, &[]).unwrap_err(),
            CompileError::UnresolvedModel {
                agent: "agent-1".to_string(),
            }
        );
    }

    #[test]
    fn pinned_selection_is_wire_identical_to_the_flat_triple() {
        // A pinned selection serializes as the bare triple it always was, so a
        // config's fingerprint is unchanged by the ModelSelection type (ADR-0052 D5).
        let selection = ModelSelection::pinned("p", "m", "b");
        let json = serde_json::to_value(&selection).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"provider_identity_ref": "p", "model_ref": "m", "backend_ref": "b"})
        );
        // Round-trips, and the historic flat triple still decodes as Pinned.
        assert_eq!(
            serde_json::from_value::<ModelSelection>(json).unwrap(),
            selection
        );
        // Auto is the only new wire shape.
        let auto = serde_json::to_value(ModelSelection::Auto).unwrap();
        assert_eq!(auto, serde_json::json!({"mode": "auto"}));
        assert_eq!(
            serde_json::from_value::<ModelSelection>(auto).unwrap(),
            ModelSelection::Auto
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
    fn compose_instructions_appends_fragments_and_preserves_base() {
        assert_eq!(compose_instructions("base", &[]), "base");
        let out = compose_instructions("base", &["r1".to_string(), "r2".to_string()]);
        assert_eq!(out, "base\n\nr1\n\nr2");
    }

    #[test]
    fn resource_prompts_flow_into_effective_instructions_and_change_the_fingerprint() {
        // ADR-0038 A3a: a bound resource's prompt is injected at compile time into the
        // agent's effective system prompt, and enters the content-address fingerprint.
        let cfg = config(&[]);
        // Fragments are opaque strings here; the resolve-side templates (per
        // ResourceKind) live in awaken-config-resolver.
        let frag = "Outputs are collected under `/mnt/session/outputs`.".to_string();
        let with = compile_with_resource_prompts(&cfg, &[], std::slice::from_ref(&frag)).unwrap();
        let spec = &with.snapshot().resolved_spec;
        assert!(spec.instructions.starts_with("be helpful"));
        assert!(spec.instructions.contains("/mnt/session/outputs"));
        // The prompt changes the runnable's content address (no stale cache hit).
        assert_ne!(
            with.snapshot().fingerprint.0,
            compile(&cfg, &[]).unwrap().snapshot().fingerprint.0
        );
    }

    #[test]
    fn empty_resource_prompts_are_byte_identical_to_bare_compile() {
        let cfg = config(&["echo"]);
        let tools = vec![tool("echo")];
        let bare = compile(&cfg, &tools).unwrap();
        let with_empty = compile_with_resource_prompts(&cfg, &tools, &[]).unwrap();
        assert_eq!(
            bare.snapshot().resolved_spec.instructions,
            with_empty.snapshot().resolved_spec.instructions
        );
        assert_eq!(
            bare.snapshot().fingerprint.0,
            with_empty.snapshot().fingerprint.0
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
