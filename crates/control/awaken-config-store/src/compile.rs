//! Compilation: a pure config → executable-snapshot function.

use awaken_runtime_contract::resolved::{ToolDescriptor, ToolFacet, ToolPresentation};
use awaken_runtime_contract::snapshot::AgentSnapshotMetadata;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
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
    #[error("invalid resolution manifest: {0}")]
    InvalidResolution(String),
    /// The config's model is still `ModelSelection::Auto` (ADR-0052 D5): compile is
    /// pure and cannot reach the provider catalog, so an `Auto` binding must be
    /// resolved to a concrete one *before* compile (publish does this). Fail-closed.
    #[error("agent {agent} has an unresolved (auto) model binding; resolve it before compiling")]
    UnresolvedModel { agent: String },
    /// A tool override (ADR-0053) is inconsistent: its `target` names no selected tool
    /// (and is not an MCP id), or its `alias` collides with another tool's model-facing
    /// id. Fail-closed — a presentation that would show the model a phantom or duplicate
    /// tool is rejected at compile, not silently dropped.
    #[error("agent {agent} has an invalid tool override: {reason}")]
    InvalidToolOverride { agent: String, reason: String },
    #[error("agent {agent} has an invalid tool recovery policy: {reason}")]
    InvalidToolRecovery { agent: String, reason: String },
    /// The config declares a capability its execution kind cannot honor (ADR-0057 D2):
    /// an A2A (remote) agent owns its own skills/MCP on the far side, so declaring them
    /// locally is a silent no-op at runtime — rejected at publish instead. `axis` is the
    /// unsupported capability (`"skills"` / `"mcp_servers"`).
    #[error("agent {agent} is a remote (a2a) agent and cannot honor local {axis}")]
    UnsupportedCapability { agent: String, axis: &'static str },
}

impl CompileError {
    /// The config field this error is about — the domain saying which part failed, so the
    /// validate surface can route the issue to the right section instead of re-deriving it
    /// from the message string. `""` means the whole config.
    #[must_use]
    pub fn field_path(&self) -> &'static str {
        match self {
            CompileError::UnknownTool { .. } => "tools",
            CompileError::UnresolvedModel { .. } => "model",
            CompileError::InvalidToolOverride { .. } => "tool_overrides",
            CompileError::InvalidToolRecovery { .. } => "recovery_policies",
            CompileError::UnsupportedCapability { axis, .. } => axis,
            CompileError::Serialize(_) | CompileError::InvalidResolution(_) => "",
        }
    }
}

/// Compile an agent config against an available tool catalog into a
/// content-addressed [`ExecutableAgentSnapshot`] the runtime can run. Each `tool_id` must
/// resolve (unknown references are rejected, fail-closed). The fingerprint is the
/// sha256 of the canonical config, so the same config always compiles to the same
/// executable snapshot.
///
/// This is a thin wrapper over [`ExecutableAgentSnapshot::builder`]: it resolves tool ids to
/// descriptors and stamps the content hash; the builder does the assembly, so the
/// compiled path and the direct path share one assembly (no duplication).
pub fn compile(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
) -> Result<ExecutableAgentSnapshot, CompileError> {
    compile_with_resource_prompts(config, tools, &[])
}

/// Compile with per-binding **resource prompts** appended to the agent's system
/// prompt (ADR-0038 A3a): the config→runtime boundary is where a bound resource's
/// description — the outputs path, a memory store's instructions, a repo's branch, a
/// skill's purpose — is injected into the agent's effective instructions, not at
/// runtime per turn. Passing `&[]` is exactly [`compile`] (fingerprint included), so
/// bare compilation is byte-identical to before resources existed. Each fragment
/// also enters the content-address fingerprint, so a different prompt set compiles to
/// a different snapshot (no stale cache hit on changed instructions).
pub fn compile_with_resource_prompts(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
    resource_prompts: &[String],
) -> Result<ExecutableAgentSnapshot, CompileError> {
    compile_resolved(
        config,
        tools,
        resource_prompts,
        AgentSnapshotMetadata::default(),
    )
}

/// Assemble the one immutable publication produced by configuration resolution.
/// `metadata` records every input the configuration plane read; runtime code never
/// calls this function and therefore cannot re-resolve or broaden those inputs.
pub fn compile_resolved(
    config: &AgentConfig,
    tools: &[ToolDescriptor],
    resource_prompts: &[String],
    mut metadata: AgentSnapshotMetadata,
) -> Result<ExecutableAgentSnapshot, CompileError> {
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

    // Execution recovery is keyed by canonical identity and is resolved into the
    // descriptor carried by the executable snapshot. Capability is intentionally
    // not trusted here: only the executor can attest it, so runtime resolution/
    // recovery performs the fail-closed capability check.
    for (target, policy) in &config.recovery_policies {
        if policy.max_attempts == 0 {
            return Err(CompileError::InvalidToolRecovery {
                agent: config.id.clone(),
                reason: format!("tool {target:?} has max_attempts = 0"),
            });
        }
        let Some(index) = descriptors.iter().position(|d| &d.id == target) else {
            return Err(CompileError::InvalidToolRecovery {
                agent: config.id.clone(),
                reason: format!("target {target:?} is not a selected static tool"),
            });
        };
        descriptors[index] = descriptors[index].clone().with_recovery(policy.clone());
    }

    // Tool presentation (ADR-0053): validate each override and project it into the
    // runtime `ToolPresentation`. A non-MCP `target` must name a selected tool; MCP
    // targets (`mcp__…`) are resolved at runtime, so they pass here (an override for an
    // MCP tool that never appears is inert). Empty overrides ⇒ empty presentation ⇒
    // byte-identical tool face.
    let alias_of: std::collections::BTreeMap<&str, &str> = config
        .tool_overrides
        .iter()
        .filter_map(|o| o.alias.as_deref().map(|a| (o.target.as_str(), a)))
        .collect();
    for ov in &config.tool_overrides {
        if !ov.target.starts_with("mcp__") && !descriptors.iter().any(|d| d.id == ov.target) {
            return Err(CompileError::InvalidToolOverride {
                agent: config.id.clone(),
                reason: format!("target {:?} is not a selected tool", ov.target),
            });
        }
        // The reserved `tool_open` id is minted by the runtime for deferred tools; an
        // alias must not shadow it.
        if ov.alias.as_deref() == Some(awaken_runtime_contract::resolved::TOOL_OPEN_ID) {
            return Err(CompileError::InvalidToolOverride {
                agent: config.id.clone(),
                reason: "alias uses the reserved tool_open id".to_string(),
            });
        }
    }
    // No two selected tools may share a model-facing id (a tool's alias if overridden,
    // else its id) — that would show the model two tools under one name.
    let mut facing: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for d in &descriptors {
        let facing_id = alias_of
            .get(d.id.as_str())
            .copied()
            .unwrap_or(d.id.as_str());
        if !facing.insert(facing_id) {
            return Err(CompileError::InvalidToolOverride {
                agent: config.id.clone(),
                reason: format!("model-facing tool id {facing_id:?} is not unique"),
            });
        }
    }
    let presentation = ToolPresentation::from_facets(config.tool_overrides.iter().map(|ov| {
        (
            ov.target.clone(),
            ToolFacet {
                alias: ov.alias.clone(),
                description: ov.description.clone(),
                defer: ov.defer,
            },
        )
    }));

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

    if !metadata.is_legacy_default() {
        let mut inputs = std::mem::take(&mut metadata.resolution.inputs);
        inputs.extend(
            descriptors
                .iter()
                .map(|tool| awaken_runtime_contract::ResolvedInputRef {
                    kind: "tool".into(),
                    id: tool.id.clone(),
                    version: awaken_runtime_contract::ResolvedInputVersion::ContentHash(
                        tool.content_hash.clone(),
                    ),
                }),
        );
        metadata.resolution = awaken_runtime_contract::ResolutionManifest::new(inputs)
            .map_err(|error| CompileError::InvalidResolution(error.to_string()))?;
    }

    // Capability gate (ADR-0057 D2): the execution kind — derived from the now-concrete
    // `backend_ref` — must be able to honor the declared capabilities. A remote (a2a)
    // agent runs everything on the far side, so local skills/MCP would be a silent
    // runtime no-op; reject at publish so the mistake surfaces at authoring time.
    if let awaken_runtime_contract::resolved::Backend::Remote { .. } =
        awaken_runtime_contract::resolved::Backend::from_ref(&model.backend_ref)
    {
        if !config.skills.is_empty() {
            return Err(CompileError::UnsupportedCapability {
                agent: config.id.clone(),
                axis: "skills",
            });
        }
        if !config.mcp_servers.is_empty() {
            return Err(CompileError::UnsupportedCapability {
                agent: config.id.clone(),
                axis: "mcp_servers",
            });
        }
    }

    let fingerprint = fingerprint_of(config, resource_prompts, &descriptors, &metadata)?;
    Ok(ExecutableAgentSnapshot::builder(&config.id)
        .instructions(compose_instructions(&config.instructions, resource_prompts))
        .model(model)
        .model_candidates(config.model_candidates.clone())
        .max_steps(config.max_steps)
        .delegation_limits(config.delegation_limits)
        .tools(descriptors)
        .plugins(config.plugin_ids.clone())
        .plugin_config(config.plugin_config.clone())
        .context_policy(config.context_policy.clone())
        .tool_presentation(presentation)
        .fingerprint(fingerprint)
        .metadata(metadata)
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

/// The canonical fingerprint: sha256 of the **behavioral** config serialization,
/// extended by the resource prompts when present. Empty prompts hash exactly the
/// behavioral config bytes, so a bare compile keeps its prior content address; a
/// non-empty prompt set changes it (a different effective system prompt is a
/// different snapshot). The config has no maps in its behavioral subset, so
/// serialization is deterministic across runs.
///
/// The Managed-Agent wire-identity metadata (`name` / `description` / `metadata`) is
/// **excluded**: it is authoring metadata the runtime never consumes (the snapshot is
/// compiled only from instructions/model/tools/plugins/policy), so it must not enter
/// the content-address. Otherwise editing a display `name` or a delegation
/// `description` would mint a new fingerprint for a byte-identical snapshot —
/// polluting the "same fingerprint ⇒ same behavior" contract. A config that never set
/// these fields hashes byte-identically to before (they `skip_serializing_if`-empty).
fn fingerprint_of(
    config: &AgentConfig,
    resource_prompts: &[String],
    tools: &[ToolDescriptor],
    metadata: &AgentSnapshotMetadata,
) -> Result<String, CompileError> {
    let mut behavioral = config.clone();
    behavioral.name = None;
    behavioral.description = None;
    behavioral.metadata.clear();
    let mut bytes =
        serde_json::to_vec(&behavioral).map_err(|err| CompileError::Serialize(err.to_string()))?;
    if !resource_prompts.is_empty() {
        let extra = serde_json::to_vec(resource_prompts)
            .map_err(|err| CompileError::Serialize(err.to_string()))?;
        bytes.extend_from_slice(&extra);
    }
    if !metadata.is_legacy_default() {
        bytes.extend_from_slice(
            &serde_json::to_vec(tools).map_err(|err| CompileError::Serialize(err.to_string()))?,
        );
        bytes.extend_from_slice(
            &serde_json::to_vec(metadata)
                .map_err(|err| CompileError::Serialize(err.to_string()))?,
        );
    }
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::tool::{ToolRecoveryMode, ToolRecoveryPolicy};

    use crate::config::ModelSelection;

    fn config(tools: &[&str]) -> AgentConfig {
        AgentConfig {
            id: "agent-1".to_string(),
            instructions: "be helpful".to_string(),
            max_steps: 8,
            delegation_limits: Default::default(),
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
    fn recovery_policy_is_pinned_and_invalid_targets_fail_closed() {
        let tools = vec![tool("echo")];
        let mut cfg = config(&["echo"]);
        cfg.recovery_policies.insert(
            "echo".into(),
            ToolRecoveryPolicy {
                mode: ToolRecoveryMode::Idempotent,
                max_attempts: 5,
            },
        );
        let compiled = compile(&cfg, &tools).unwrap();
        assert_eq!(
            compiled.resolved_spec.tool_descriptors[0].recovery_policy,
            cfg.recovery_policies["echo"]
        );

        cfg.recovery_policies
            .insert("ghost".into(), ToolRecoveryPolicy::default());
        let error = compile(&cfg, &tools).unwrap_err();
        assert!(matches!(error, CompileError::InvalidToolRecovery { .. }));
        assert_eq!(error.field_path(), "recovery_policies");

        cfg.recovery_policies.remove("ghost");
        cfg.recovery_policies.get_mut("echo").unwrap().max_attempts = 0;
        assert!(matches!(
            compile(&cfg, &tools),
            Err(CompileError::InvalidToolRecovery { .. })
        ));
    }

    #[test]
    fn delegation_limits_are_resolved_and_enter_the_fingerprint() {
        let base = config(&[]);
        let base_compiled = compile(&base, &[]).unwrap();
        let mut bounded = base;
        bounded.delegation_limits =
            awaken_runtime_contract::delegation::DelegationLimits::new(2, 3, 5);
        let bounded_compiled = compile(&bounded, &[]).unwrap();
        assert_eq!(
            bounded_compiled.resolved_spec.delegation_limits,
            bounded.delegation_limits
        );
        assert_ne!(bounded_compiled.fingerprint, base_compiled.fingerprint);
    }

    #[test]
    fn model_candidates_compile_into_the_resolved_pool_and_enter_the_fingerprint() {
        let tools = vec![tool("echo")];

        let mut pooled = config(&["echo"]);
        pooled.model_candidates = vec![ModelBinding::new("p", "fallback", "b")];
        let compiled = compile(&pooled, &tools).unwrap();
        let spec = &compiled.resolved_spec;
        // The pool is carried onto the resolved spec: primary + one fallback.
        assert_eq!(spec.model_candidates.len(), 1);
        assert_eq!(spec.candidate_bindings().len(), 2);
        assert_eq!(spec.candidate_bindings()[1].model_ref, "fallback");

        // A pool enters the content address (a different pool is a different snapshot);
        // an empty pool (the default) leaves the fingerprint byte-identical.
        let single = compile(&config(&["echo"]), &tools).unwrap();
        assert_ne!(
            compiled.fingerprint.0, single.fingerprint.0,
            "a pool must change the content address"
        );
        assert!(single.resolved_spec.model_candidates.is_empty());
    }

    #[test]
    fn tool_overrides_compile_into_the_presentation_and_enter_the_fingerprint() {
        use crate::config::ToolOverride;
        let tools = vec![tool("echo"), tool("mcp__gh__create_issue")];

        let mut cfg = config(&["echo", "mcp__gh__create_issue"]);
        cfg.tool_overrides = vec![
            ToolOverride {
                target: "echo".into(),
                alias: Some("say".into()),
                description: Some("Speak.".into()),
                defer: false,
            },
            ToolOverride {
                target: "mcp__gh__create_issue".into(),
                alias: None,
                description: None,
                defer: true,
            },
        ];
        let compiled = compile(&cfg, &tools).unwrap();
        let pres = &compiled.resolved_spec.tool_presentation;
        assert!(!pres.is_empty());
        // The alias reverse-maps back to the canonical id; the model face renames + defers.
        assert_eq!(pres.resolve("say"), "echo");
        let presented = pres.present(&tools);
        assert!(
            presented
                .face
                .iter()
                .any(|d| d.id == "say" && d.description == "Speak.")
        );
        assert!(
            presented
                .deferred
                .iter()
                .any(|d| d.id == "mcp__gh__create_issue")
        );

        // Overrides enter the content address; no overrides ⇒ byte-identical fingerprint.
        let bare = compile(&config(&["echo", "mcp__gh__create_issue"]), &tools).unwrap();
        assert_ne!(compiled.fingerprint.0, bare.fingerprint.0);
        assert!(bare.resolved_spec.tool_presentation.is_empty());
    }

    #[test]
    fn an_override_targeting_an_unselected_non_mcp_tool_is_rejected() {
        use crate::config::ToolOverride;
        let tools = vec![tool("echo")];
        let mut cfg = config(&["echo"]);
        cfg.tool_overrides = vec![ToolOverride {
            target: "ghost".into(),
            alias: Some("g".into()),
            ..Default::default()
        }];
        assert!(matches!(
            compile(&cfg, &tools),
            Err(CompileError::InvalidToolOverride { .. })
        ));

        // An MCP target that isn't in the compile catalog is allowed (resolved at runtime).
        let mut mcp = config(&["echo"]);
        mcp.tool_overrides = vec![ToolOverride {
            target: "mcp__x__y".into(),
            alias: Some("y".into()),
            ..Default::default()
        }];
        assert!(compile(&mcp, &tools).is_ok());

        // An alias colliding with another selected tool's id is rejected.
        let mut clash = config(&["echo", "read"]);
        clash.tool_overrides = vec![ToolOverride {
            target: "echo".into(),
            alias: Some("read".into()),
            ..Default::default()
        }];
        assert!(matches!(
            compile(&clash, &[tool("echo"), tool("read")]),
            Err(CompileError::InvalidToolOverride { .. })
        ));
    }

    #[test]
    fn compile_is_deterministic_and_content_addressed() {
        let tools = vec![tool("echo")];
        let a = compile(&config(&["echo"]), &tools).unwrap();
        let b = compile(&config(&["echo"]), &tools).unwrap();
        let fp = a.fingerprint.0.clone();
        assert_eq!(fp, b.fingerprint.0, "same config, same fingerprint");

        // The snapshot envelope and resolved payload agree on the fingerprint.
        assert_eq!(a.resolved_spec.catalog_fingerprint.0, fp);

        // A different config yields a different fingerprint.
        let mut other = config(&["echo"]);
        other.instructions = "be terse".to_string();
        assert_ne!(compile(&other, &tools).unwrap().fingerprint.0, fp);
    }

    #[test]
    fn context_policy_flows_into_the_compiled_spec_and_fingerprint() {
        use awaken_runtime_contract::resolved::ContextPolicy;
        let mut cfg = config(&[]);
        cfg.context_policy = ContextPolicy::KeepLast { keep_last: 3 };
        let compiled = compile(&cfg, &[]).unwrap();
        assert_eq!(
            compiled.resolved_spec.context_policy,
            ContextPolicy::KeepLast { keep_last: 3 }
        );
        // The policy is part of the content address: changing it changes the hash.
        let default_fp = compile(&config(&[]), &[]).unwrap().fingerprint.0.clone();
        assert_ne!(compiled.fingerprint.0, default_fp);
    }

    #[test]
    fn tool_patterns_select_matching_catalog_tools_and_enter_the_fingerprint() {
        let catalog = vec![tool("fs_read"), tool("fs_write"), tool("net_get")];

        let mut cfg = config(&["net_get"]); // one exact id
        cfg.tool_patterns = vec!["fs_*".to_string()]; // plus a glob
        let spec = compile(&cfg, &catalog).unwrap();
        let ids: Vec<String> = spec
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
            .fingerprint
            .0
            .clone();
        // A non-empty pattern set enters the content address.
        assert_ne!(spec.fingerprint.0, plain_fp);
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
    fn a2a_agent_declaring_skills_or_mcp_fails_the_capability_gate() {
        // ADR-0057 D2: a remote (a2a) agent honors neither locally — declaring them is a
        // silent runtime no-op, so publish rejects it. Skills reported first.
        let mut with_skills = config(&[]);
        with_skills.model_binding = ModelSelection::pinned("p", "m", "a2a:https://remote/agent");
        with_skills.skills = vec![serde_json::json!({"id": "review"})];
        assert_eq!(
            compile(&with_skills, &[]).unwrap_err(),
            CompileError::UnsupportedCapability {
                agent: "agent-1".to_string(),
                axis: "skills",
            }
        );

        let mut with_mcp = config(&[]);
        with_mcp.model_binding = ModelSelection::pinned("p", "m", "a2a:https://remote/agent");
        with_mcp.mcp_servers = vec![serde_json::json!({"name": "gh"})];
        assert_eq!(
            compile(&with_mcp, &[]).unwrap_err(),
            CompileError::UnsupportedCapability {
                agent: "agent-1".to_string(),
                axis: "mcp_servers",
            }
        );
    }

    #[test]
    fn native_and_acp_agents_may_declare_skills_and_mcp() {
        // The gate is A2A-only: Native and ACP kinds honor skills/MCP (in-process or via
        // the CLI's config-home/session), so they compile with them present.
        for backend in ["genai", "acp:claude"] {
            let mut cfg = config(&[]);
            cfg.model_binding = ModelSelection::pinned("p", "m", backend);
            cfg.skills = vec![serde_json::json!({"id": "review"})];
            cfg.mcp_servers = vec![serde_json::json!({"name": "gh"})];
            assert!(
                compile(&cfg, &[]).is_ok(),
                "backend `{backend}` must honor skills/mcp"
            );
        }
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
        let spec = &with.resolved_spec;
        assert!(spec.instructions.starts_with("be helpful"));
        assert!(spec.instructions.contains("/mnt/session/outputs"));
        // The prompt changes the snapshot's content address (no stale cache hit).
        assert_ne!(
            with.fingerprint.0,
            compile(&cfg, &[]).unwrap().fingerprint.0
        );
    }

    #[test]
    fn publication_pinned_inference_access_is_part_of_the_fingerprint() {
        let cfg = config(&[]);
        let metadata = |credential: &str| AgentSnapshotMetadata {
            source: awaken_runtime_contract::AgentConfigRevisionRef {
                agent_id: awaken_runtime_contract::snapshot::AgentId("agent-1".into()),
                revision: 1,
            },
            inference_access: Some(
                awaken_runtime_contract::InferenceAccess::resolved_credential(
                    credential,
                    1,
                    "workspace-a",
                    "anthropic@1",
                    "primary@1",
                    awaken_runtime_contract::InferenceEndpoint {
                        adapter_kind: "anthropic".into(),
                        base_url: "https://api.example/v1".into(),
                        upstream_model: "model-a".into(),
                    },
                ),
            ),
            ..Default::default()
        };
        let first = compile_resolved(&cfg, &[], &[], metadata("credential-a")).unwrap();
        let second = compile_resolved(&cfg, &[], &[], metadata("credential-b")).unwrap();
        assert_ne!(first.fingerprint, second.fingerprint);
        assert_eq!(
            first.metadata.inference_access.unwrap().reference,
            "credential-a"
        );
    }

    #[test]
    fn wire_identity_metadata_is_excluded_from_the_fingerprint() {
        // name / description / metadata are authoring metadata the runtime never
        // consumes — editing them must NOT mint a new content-address for a
        // byte-identical snapshot (so a delegation `description` edit is not a republish).
        let tools = vec![tool("echo")];
        let base = config(&["echo"]);
        let base_fp = compile(&base, &tools).unwrap().fingerprint.0.clone();

        let mut labeled = base.clone();
        labeled.description = Some("routes research questions".to_string());
        labeled.name = Some("Researcher".to_string());
        labeled
            .metadata
            .insert("team".to_string(), "research".to_string());
        let labeled_fp = compile(&labeled, &tools).unwrap().fingerprint.0.clone();
        assert_eq!(
            base_fp, labeled_fp,
            "name/description/metadata are excluded from the content-address"
        );

        // A genuinely behavioral change still moves the fingerprint.
        let mut rebehaved = base.clone();
        rebehaved.instructions = "be terse".to_string();
        let rebehaved_fp = compile(&rebehaved, &tools).unwrap().fingerprint.0.clone();
        assert_ne!(
            base_fp, rebehaved_fp,
            "instructions still enter the fingerprint"
        );
    }

    #[test]
    fn empty_resource_prompts_are_byte_identical_to_bare_compile() {
        let cfg = config(&["echo"]);
        let tools = vec![tool("echo")];
        let bare = compile(&cfg, &tools).unwrap();
        let with_empty = compile_with_resource_prompts(&cfg, &tools, &[]).unwrap();
        assert_eq!(
            bare.resolved_spec.instructions,
            with_empty.resolved_spec.instructions
        );
        assert_eq!(bare.fingerprint.0, with_empty.fingerprint.0);
    }

    #[test]
    fn compile_carries_plugin_ids_and_config_sections() {
        let mut cfg = config(&["echo"]);
        cfg.plugin_ids = vec!["state_machine".to_string()];
        cfg.plugin_config.insert(
            "state_machine".to_string(),
            serde_json::json!({"machines": []}),
        );
        let snapshot = compile(&cfg, &[tool("echo")]).unwrap();
        let spec = &snapshot.resolved_spec;
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
            snapshot.fingerprint.0,
            compile(&other, &[tool("echo")]).unwrap().fingerprint.0
        );
    }

    // --- CEG 03 / B1 (compile priority + override masks) ---------------------

    #[test]
    fn c3_an_alias_equal_to_the_reserved_tool_open_id_is_rejected() {
        // C3: the runtime mints `tool_open` for deferred tools; an override alias must
        // not shadow that reserved id, else the model sees two tools under one face.
        use crate::config::ToolOverride;
        use awaken_runtime_contract::resolved::TOOL_OPEN_ID;
        let tools = vec![tool("echo")];
        let mut cfg = config(&["echo"]);
        cfg.tool_overrides = vec![ToolOverride {
            target: "echo".into(),
            alias: Some(TOOL_OPEN_ID.to_string()),
            ..Default::default()
        }];
        let err = compile(&cfg, &tools).unwrap_err();
        assert!(
            matches!(err, CompileError::InvalidToolOverride { .. }),
            "reserved tool_open alias must be rejected, got {err:?}"
        );
    }

    #[test]
    fn c4_two_tools_aliased_to_one_visible_id_is_rejected() {
        // C4: two selected tools overridden to the same model-facing id would present
        // the model two tools under one name — fail-closed, not silently deduped.
        use crate::config::ToolOverride;
        let catalog = vec![tool("echo"), tool("read")];
        let mut cfg = config(&["echo", "read"]);
        cfg.tool_overrides = vec![
            ToolOverride {
                target: "echo".into(),
                alias: Some("same".into()),
                ..Default::default()
            },
            ToolOverride {
                target: "read".into(),
                alias: Some("same".into()),
                ..Default::default()
            },
        ];
        let err = compile(&cfg, &catalog).unwrap_err();
        assert!(
            matches!(err, CompileError::InvalidToolOverride { .. }),
            "a duplicate model-facing id must be rejected, got {err:?}"
        );
    }

    #[test]
    fn c8_unknown_tool_takes_priority_over_an_unresolved_auto_model() {
        // C8 (priority chain): tool resolution runs before model resolution, so an
        // Auto binding *and* an unknown tool must surface the UnknownTool error — the
        // earlier, more specific failure — not UnresolvedModel.
        let mut cfg = config(&["ghost"]);
        cfg.model_binding = ModelSelection::Auto;
        let err = compile(&cfg, &[tool("echo")]).unwrap_err();
        assert_eq!(
            err,
            CompileError::UnknownTool {
                agent: "agent-1".to_string(),
                tool: "ghost".to_string(),
            },
            "unknown tool must win over the unresolved model"
        );
    }

    #[test]
    fn c9_an_mcp_override_target_absent_from_the_catalog_passes() {
        // C9: the `mcp__` prefix masks the "target is not a selected tool" check — MCP
        // ids are resolved at runtime, so an override for an MCP tool that never appears
        // in the compile catalog is inert (passes), and being absent it does not enter
        // the model-facing uniqueness set either.
        use crate::config::ToolOverride;
        let tools = vec![tool("echo")];
        let mut cfg = config(&["echo"]);
        cfg.tool_overrides = vec![ToolOverride {
            target: "mcp__gh__create_issue".into(),
            alias: Some("file_issue".into()),
            description: Some("Open a GitHub issue.".into()),
            defer: true,
        }];
        let compiled = compile(&cfg, &tools).expect("missing MCP target must compile");
        // The override still projects into the presentation (applied at runtime).
        let pres = &compiled.resolved_spec.tool_presentation;
        assert!(!pres.is_empty());
        assert_eq!(pres.resolve("file_issue"), "mcp__gh__create_issue");
    }

    // --- CEG 03 / B2 (glob_match) --------------------------------------------

    #[test]
    fn glob_match_covers_prefix_middle_exact_and_empty() {
        // (a) trailing star matches a longer id sharing the prefix.
        assert!(glob_match("fs_*", "fs_read"));
        // (b) anchored at both ends: `fs_*` does not match a mid-string occurrence.
        assert!(!glob_match("fs_*", "net_fs"));
        // (c) the empty pattern matches only the empty string.
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
        // (d) a middle star spans any run (including empty).
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(glob_match("a*c", "abbbc"));
        assert!(!glob_match("a*c", "abd"));
        // (e) a pattern with no star is an exact match.
        assert!(glob_match("fs_read", "fs_read"));
        assert!(!glob_match("fs_read", "fs_reads"));
    }

    // --- CEG 03 / B5 (ModelSelection) ----------------------------------------

    #[test]
    fn model_selection_resolved_reports_pinned_and_auto() {
        // (a) Pinned exposes the concrete binding; (b) Auto is the None "resolve me"
        // signal. (c)/(d) wire shapes are covered by
        // `pinned_selection_is_wire_identical_to_the_flat_triple`.
        let pinned = ModelSelection::pinned("p", "m", "b");
        assert_eq!(pinned.resolved(), Some(&ModelBinding::new("p", "m", "b")));
        assert!(!pinned.is_auto());
        assert_eq!(ModelSelection::Auto.resolved(), None);
        assert!(ModelSelection::Auto.is_auto());
    }

    // --- CEG 03 / B6 (CompileError::field_path) ------------------------------

    #[test]
    fn compile_error_field_path_routes_each_variant() {
        assert_eq!(
            CompileError::UnknownTool {
                agent: "a".into(),
                tool: "t".into()
            }
            .field_path(),
            "tools"
        );
        assert_eq!(
            CompileError::UnresolvedModel { agent: "a".into() }.field_path(),
            "model"
        );
        assert_eq!(
            CompileError::InvalidToolOverride {
                agent: "a".into(),
                reason: "r".into()
            }
            .field_path(),
            "tool_overrides"
        );
        assert_eq!(CompileError::Serialize("boom".into()).field_path(), "");
    }
}
