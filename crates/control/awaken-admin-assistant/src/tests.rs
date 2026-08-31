//! Unit tests for the four management tools. The `CapabilityReader` /
//! `DraftValidator` / `DraftStore` ports are exercised with in-crate test doubles; the
//! real adapters (over the shared catalog and `ConfigService`/`ConfigPlane`) are wired
//! and tested in the host.

use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

fn call(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        tool_id: id.into(),
        arguments: args,
    }
}

struct FakeCaps;
#[async_trait]
impl CapabilityReader for FakeCaps {
    async fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            agents: vec!["assistant".into()],
            models: vec!["m-1".into(), "m-2".into()],
            providers: vec!["openai".into()],
            tools: vec!["read".into(), "glob".into()],
            plugins: vec![PluginInfo {
                id: "state_machine".into(),
                schema_keys: vec!["state_machine".into()],
                config_schema: Some(serde_json::json!({ "type": "object" })),
            }],
            skills: vec!["greet".into()],
            mcp_servers: vec![],
            memory_stores: vec![],
        }
    }
}

/// Records the last environment authored, so a test can assert the tool persisted it.
#[derive(Default)]
struct FakeEnvAuthor {
    commands: std::sync::Mutex<Vec<awaken_environment_contract::CreateEnvironmentCommand>>,
}
#[async_trait]
impl EnvironmentAuthor for FakeEnvAuthor {
    async fn create_environment(
        &self,
        command: awaken_environment_contract::CreateEnvironmentCommand,
    ) -> Result<String, String> {
        let mut commands = self.commands.lock().unwrap();
        if let Some((index, existing)) = commands
            .iter()
            .enumerate()
            .find(|(_, existing)| existing.command_id == command.command_id)
        {
            if existing.fingerprint() != command.fingerprint() {
                return Err("conflicting environment command id".into());
            }
            return Ok(format!("env_test_{index}"));
        }
        let index = commands.len();
        commands.push(command);
        Ok(format!("env_test_{index}"))
    }
}

/// Rejects any draft that names a tool id containing "ghost" (stands in for the real
/// compile-time `UnknownTool` fence).
struct FakeValidator;
#[async_trait]
impl DraftValidator for FakeValidator {
    async fn validate(&self, draft: &AgentConfig) -> Result<(), String> {
        if draft.tool_ids.iter().any(|t| t.contains("ghost")) {
            Err("references unknown tool".into())
        } else {
            Ok(())
        }
    }

    async fn runtime_agent_override_ids(
        &self,
        draft: &AgentConfig,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        // The fake catalog contains one AgentDelegation descriptor. Mirror the
        // production catalog+semantic-role projection without giving arbitrary
        // regular/custom names compatibility authority.
        Ok(draft
            .multiagent
            .as_ref()
            .is_some_and(awaken_agent_config::MultiagentConfig::has_delegation_target)
            .then(|| "agent_run".to_string())
            .into_iter()
            .collect())
    }
}

/// An in-memory stand-in for the host's `ConfigServiceDraftStore`: `put` overwrites by
/// id, `get` reads back. Lets a test assert exactly what was persisted (unpublished).
/// The `resources` map stands in for the SEPARATE data-plane resource store so a test
/// can assert a binding was authored alongside the config.
#[derive(Default)]
struct MemDraftStore {
    configs: Mutex<HashMap<String, AgentConfigRevision>>,
    resources: Mutex<HashMap<String, Vec<InputSpec>>>,
    audits: Mutex<HashMap<String, (AdminAuditEvent, bool)>>,
    concurrent_config_before_audited_write: Mutex<Option<AgentConfig>>,
    commit_audit_on_audit_read: Mutex<Option<(String, usize)>>,
    reconcile_calls: Mutex<usize>,
}

#[async_trait]
impl DraftStore for MemDraftStore {
    async fn put_audited_with_resources(
        &self,
        draft: &AgentConfig,
        expected_revision: u64,
        audit: &AdminAuditEvent,
        resources: Option<Vec<InputSpec>>,
    ) -> Result<(), String> {
        let audit_key = format!("{}:{}", audit.tool, audit.call_id);
        {
            let audits = self.audits.lock().unwrap();
            match audits.get(&audit_key) {
                Some((existing, _)) if existing != audit => {
                    return Err("conflicting audit id".into());
                }
                Some((_, true)) => return Ok(()),
                Some((_, false)) => {}
                None => {
                    return Err(
                        "audited config transaction requires a pre-recorded management audit"
                            .into(),
                    );
                }
            }
        }
        let concurrent = self
            .concurrent_config_before_audited_write
            .lock()
            .unwrap()
            .take();
        if let Some(concurrent) = concurrent {
            self.seed(&concurrent).await?;
        }
        let mut configs = self.configs.lock().unwrap();
        let current = configs.get(&draft.id).cloned();
        let current_revision = current.as_ref().map_or(0, |current| current.revision);
        if current_revision != expected_revision {
            return Err(format!(
                "agent `{}` changed concurrently (current revision: {:?})",
                draft.id,
                (current_revision != 0).then_some(current_revision)
            ));
        }
        let draft = draft
            .canonicalize_mutable_authoring_against(current.as_ref().map(|entry| &entry.config))?;
        let draft_id = draft.id.clone();
        configs.insert(
            draft_id.clone(),
            AgentConfigRevision {
                revision: current_revision + 1,
                config: draft,
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
            },
        );
        drop(configs);
        if let Some(resources) = resources {
            self.resources.lock().unwrap().insert(draft_id, resources);
        }
        self.audits
            .lock()
            .unwrap()
            .get_mut(&audit_key)
            .expect("pre-recorded audit remains present")
            .1 = true;
        Ok(())
    }
    async fn record_audit(&self, audit: &AdminAuditEvent) -> Result<AuditedConfigWrite, String> {
        let audit_key = format!("{}:{}", audit.tool, audit.call_id);
        let mut audits = self.audits.lock().unwrap();
        match audits.get(&audit_key) {
            Some((existing, _)) if existing != audit => Err("conflicting audit id".into()),
            Some(_) => Ok(AuditedConfigWrite::Replayed),
            None => {
                audits.insert(audit_key, (audit.clone(), false));
                Ok(AuditedConfigWrite::Applied)
            }
        }
    }
    async fn get_audit(
        &self,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, String> {
        let audit_key = format!("{tool}:{call_id}");
        let commit_now = {
            let mut scheduled = self.commit_audit_on_audit_read.lock().unwrap();
            match scheduled.as_mut() {
                Some((key, remaining)) if key == &audit_key && *remaining == 1 => {
                    scheduled.take();
                    true
                }
                Some((key, remaining)) if key == &audit_key => {
                    *remaining -= 1;
                    false
                }
                _ => false,
            }
        };
        if commit_now && let Some((_, committed)) = self.audits.lock().unwrap().get_mut(&audit_key)
        {
            *committed = true;
        }
        Ok(self
            .audits
            .lock()
            .unwrap()
            .get(&audit_key)
            .map(|(record, business_committed)| ManagementAuditEntry {
                record: record.clone(),
                business_committed: *business_committed,
            }))
    }
    async fn reconcile_pending_effects(&self) -> Result<(), String> {
        *self.reconcile_calls.lock().unwrap() += 1;
        Ok(())
    }
    async fn get_versioned(&self, id: &str) -> Result<Option<AgentConfigRevision>, String> {
        Ok(self.configs.lock().unwrap().get(id).cloned())
    }
}

impl MemDraftStore {
    async fn seed(&self, draft: &AgentConfig) -> Result<(), String> {
        let mut configs = self.configs.lock().unwrap();
        let revision = configs
            .get(&draft.id)
            .map_or(1, |current| current.revision + 1);
        configs.insert(
            draft.id.clone(),
            AgentConfigRevision {
                revision,
                config: draft.clone(),
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
            },
        );
        Ok(())
    }

    fn stored(&self, id: &str) -> Option<AgentConfig> {
        self.configs
            .lock()
            .unwrap()
            .get(id)
            .map(|revision| revision.config.clone())
    }
    fn is_empty(&self) -> bool {
        self.configs.lock().unwrap().is_empty()
    }
    fn stored_resources(&self, id: &str) -> Vec<InputSpec> {
        self.resources
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    fn race_next_audited_write_with(&self, concurrent: AgentConfig) {
        *self.concurrent_config_before_audited_write.lock().unwrap() = Some(concurrent);
    }

    fn commit_audit_on_audit_read(&self, tool: &str, call_id: &str, read: usize) {
        assert!(read > 0);
        *self.commit_audit_on_audit_read.lock().unwrap() =
            Some((format!("{tool}:{call_id}"), read));
    }
}

/// Captures durable change records so tests can assert mutations, but not reads,
/// enter the config-store idempotency path.
#[derive(Default)]
struct CapturingAudit(Mutex<Vec<AdminAuditEvent>>);
impl AuditSink for CapturingAudit {
    fn record(&self, event: AdminAuditEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// The parsed `{ config, note }` envelope a draft/patch tool returns on success.
fn parse_saved(content: &str) -> AgentConfig {
    let v: serde_json::Value = serde_json::from_str(content).expect("saved envelope is JSON");
    assert!(
        v.get("note").and_then(|n| n.as_str()).is_some(),
        "envelope carries a pointer note: {content}"
    );
    serde_json::from_value(v["config"].clone()).expect("envelope carries the config")
}

/// A full toolset over a shared store + audit so a test can inspect both.
struct Harness {
    tools: Vec<Arc<dyn RawTool>>,
    store: Arc<MemDraftStore>,
    audit: Arc<CapturingAudit>,
}

impl Harness {
    fn new() -> Self {
        Self::with_validator(Arc::new(FakeValidator))
    }
    fn with_validator(validator: Arc<dyn DraftValidator>) -> Self {
        let store = Arc::new(MemDraftStore::default());
        let audit = Arc::new(CapturingAudit::default());
        let tools = admin_tools(
            Arc::new(FakeCaps),
            validator,
            store.clone(),
            Arc::new(FakeEnvAuthor::default()),
            audit.clone(),
        );
        Self {
            tools,
            store,
            audit,
        }
    }
    fn tool(&self, id: &str) -> Arc<dyn RawTool> {
        self.tools
            .iter()
            .find(|t| t.id() == id)
            .expect("tool exists")
            .clone()
    }
}

#[test]
fn descriptors_are_the_admin_tools_and_carry_no_publish_tool() {
    let ids: Vec<String> = admin_tool_descriptors()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    assert_eq!(
        ids,
        vec![
            CAPABILITIES_TOOL,
            CREATE_DRAFT_TOOL,
            PATCH_TOOL,
            VALIDATE_TOOL,
            EXPLAIN_TOOL,
            CREATE_ENV_TOOL
        ]
    );
    // There is deliberately no publish tool (D4): publication is a console action.
    assert!(!ids.iter().any(|id| id.contains("publish")));
    // Every descriptor is namespaced under the admin owner prefix.
    assert!(
        admin_tool_descriptors()
            .iter()
            .all(|d| d.content_hash().starts_with("admin:"))
    );
    // No descriptor schema uses `additionalProperties` (Gemini rejects it).
    for d in admin_tool_descriptors() {
        let schema = serde_json::to_string(&d.parameters).unwrap();
        assert!(
            !schema.contains("additionalProperties"),
            "{}: {schema}",
            d.id
        );
    }
    // The executable set matches the advertised descriptor set one-to-one.
    let exec_ids: Vec<String> = Harness::new()
        .tools
        .iter()
        .map(|t| t.id().to_string())
        .collect();
    assert_eq!(exec_ids, ids);
}

#[tokio::test]
async fn capabilities_returns_the_redacted_org_shared_view() {
    let h = Harness::new();
    let out = h
        .tool(CAPABILITIES_TOOL)
        .invoke(call(CAPABILITIES_TOOL, serde_json::json!({})))
        .await
        .unwrap();
    assert!(!out.is_error);
    let caps: PlatformCapabilities = serde_json::from_str(&out.text()).unwrap();
    assert_eq!(caps.models, vec!["m-1", "m-2"]);
    assert_eq!(caps.plugins[0].id, "state_machine");
    // No secret-bearing field exists on the view — it is redacted by construction.
    assert!(!out.text().contains("api_key") && !out.text().contains("credential"));
}

#[tokio::test]
async fn draft_agent_persists_an_unpublished_draft_and_returns_it() {
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "support",
                "instructions": "be helpful",
                "tool_ids": ["read"],
                "max_steps": 5
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());
    let returned = parse_saved(&out.text());
    assert_eq!(returned.id, "support");
    assert_eq!(returned.max_steps, 5);
    assert_eq!(returned.tool_ids, vec!["read".to_string()]);
    // Omitting `model` leaves the draft auto-bound (D5).
    assert!(returned.model_binding.is_auto());
    // It was PERSISTED (unpublished): the store holds the same config.
    let stored = h.store.stored("support").expect("persisted");
    assert_eq!(stored, returned);
}

#[tokio::test]
async fn draft_agent_pins_a_model_when_given_one() {
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "a", "instructions": "hi", "model": "m-1" }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error);
    let stored = h.store.stored("a").unwrap();
    assert_eq!(
        stored.model_binding,
        ModelSelection::pinned("default", "m-1", "default")
    );
}

#[tokio::test]
async fn draft_agent_round_trips_tool_overrides() {
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "authoring",
                "instructions": "author configs",
                "tool_ids": ["read"],
                "tool_overrides": [
                    {
                        "target": "read",
                        "alias": "peek",
                        "description": "look",
                        "exposure": "on_demand"
                    }
                ]
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());
    // Causal graph: C1 the strict admin-tool boundary decodes a closed exposure
    // enum; C2 validation succeeds. E1 the exact authored value is persisted;
    // E2 the response projection is lossless. An unknown/boolean exposure is
    // rejected by `deny_unknown_fields` + enum deserialization before mutation.
    let stored = h.store.stored("authoring").expect("persisted");
    assert_eq!(stored.tool_overrides.len(), 1);
    let ov = &stored.tool_overrides[0];
    assert_eq!(ov.target, "read");
    assert_eq!(ov.alias.as_deref(), Some("peek"));
    assert_eq!(
        ov.exposure,
        Some(awaken_runtime_contract::resolved::ToolExposure::OnDemand)
    );
    // And the returned envelope reflects it too.
    let returned = parse_saved(&out.text());
    assert_eq!(returned.tool_overrides, stored.tool_overrides);
}

#[tokio::test]
async fn draft_agent_binds_a_resource_into_the_separate_store() {
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "researcher",
                "instructions": "research",
                "resources": [
                    { "kind": "memory_store", "resource_id": "mem_1", "access": "read_write" }
                ]
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());
    // The config persisted, AND the binding landed in the SEPARATE resource store.
    assert!(h.store.stored("researcher").is_some());
    let bound = h.store.stored_resources("researcher");
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].kind, "memory_store");
    assert_eq!(bound[0].resource_id, "mem_1");
    assert_eq!(bound[0].access.as_deref(), Some("read_write"));
}

#[tokio::test]
async fn draft_agent_round_trips_mcp_skills_multiagent_and_metadata() {
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "full",
                "instructions": "do it all",
                "mcp_servers": [{ "id": "github", "url": "https://mcp.example" }],
                "skills": [{ "id": "greet" }],
                "multiagent": { "type": "coordinator", "agents": ["a", "b"] },
                "metadata": { "team": "platform", "tier": "gold" }
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());
    let stored = h.store.stored("full").unwrap();
    assert_eq!(
        stored.mcp_servers,
        vec![
            awaken_runtime_contract::agent_bindings::AgentMcpServerBinding {
                name: "github".into(),
                transport: awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::http(
                    "https://mcp.example",
                ),
                credential: None,
                prompts_as_skills: false,
            }
        ]
    );
    assert_eq!(stored.skills[0].skill_id, "greet");
    assert_eq!(
        stored.multiagent.as_ref().map(|config| {
            config
                .agents
                .iter()
                .map(|target| target.resolved_id("full"))
                .collect::<Vec<_>>()
        }),
        Some(vec!["a", "b"])
    );
    assert_eq!(
        stored.metadata.get("team").map(String::as_str),
        Some("platform")
    );
    assert_eq!(
        stored.metadata.get("tier").map(String::as_str),
        Some("gold")
    );
    // No resources named → the separate store stays empty for this agent.
    assert!(h.store.stored_resources("full").is_empty());
}

#[tokio::test]
async fn patch_agent_replaces_the_whole_resource_set() {
    let h = Harness::new();
    // Draft with one memory-store binding.
    h.tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "r",
                "instructions": "hi",
                "resources": [{ "kind": "memory_store", "resource_id": "mem_1" }]
            }),
        ))
        .await
        .unwrap();
    assert_eq!(h.store.stored_resources("r").len(), 1);

    // Patch replaces the whole set with a different binding.
    let out = h
        .tool(PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({
                "id": "r",
                "patch": {
                    "resources": [
                        { "kind": "repository", "resource_id": "repo_1", "access": "read_only" }
                    ]
                }
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());
    let bound = h.store.stored_resources("r");
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].kind, "repository");
    assert_eq!(bound[0].resource_id, "repo_1");
    assert_eq!(bound[0].access.as_deref(), Some("read_only"));
}

#[tokio::test]
async fn patch_agent_leaves_resources_untouched_when_absent() {
    let h = Harness::new();
    h.tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "r",
                "instructions": "hi",
                "resources": [{ "kind": "memory_store", "resource_id": "mem_1" }]
            }),
        ))
        .await
        .unwrap();
    // A patch with no `resources` key must not disturb the existing binding.
    let out = h
        .tool(PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({ "id": "r", "patch": { "max_steps": 3 } }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());
    let bound = h.store.stored_resources("r");
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].resource_id, "mem_1");
}

#[tokio::test]
async fn draft_agent_derives_plugin_ids_and_size_bounds_sections() {
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "p",
                "instructions": "hi",
                "plugin_config": { "state_machine": { "machines": [] } }
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());
    let stored = h.store.stored("p").unwrap();
    // plugin_ids are derived from the plugin_config keys.
    assert_eq!(stored.plugin_ids, vec!["state_machine".to_string()]);

    // A section over the byte cap is rejected and NOT persisted.
    let big = "x".repeat(MAX_PLUGIN_CONFIG_BYTES + 1);
    let mut oversized_call = call(
        CREATE_DRAFT_TOOL,
        serde_json::json!({
            "id": "toobig",
            "instructions": "hi",
            "plugin_config": { "state_machine": { "blob": big } }
        }),
    );
    oversized_call.call_id = "c2".into();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(oversized_call)
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text().contains("over the"));
    assert!(
        h.store.stored("toobig").is_none(),
        "oversized not persisted"
    );
}

#[tokio::test]
async fn assistant_authors_only_typed_controlled_permissions_and_never_legacy_plugin_policy() {
    // Cause/effect graph: C1 a fresh draft requests the controlled-modification
    // preset; C2 a fresh draft tries the retired permission plugin section; C3 an
    // historical mutable draft already contains that legacy section and an explicit
    // controlled patch replaces it; C4 a fresh draft declares MCP; C5 a historical
    // legacy permission owns an MCP binding without typed MCP policy; C6 an existing
    // draft already has typed MCP and client-executed policy; C7 that draft also
    // carries a catalog-backed Agent-delegation override; C8 a canonical
    // noncontrolled member is disabled but has execution configuration; C9
    // retired/unknown overrides collide with a same-named client tool. Effects:
    // E1 one Agent Toolset owns permission;
    // E2 Bash/write/edit are enabled as authored and always_ask; E3 no preset
    // marker is persisted; E4 direct
    // legacy authoring fails before the config write; E5 explicit replacement removes
    // the legacy section instead of combining two permission owners; E6 fresh MCP gets
    // one fail-closed default-ask Toolset; E7 unrepresentable legacy MCP fails with
    // no write; E8 existing typed MCP policy and client tool remain byte-identical;
    // E9 the delegation override remains byte-identical for the Runtime gate;
    // E10 the disabled execution configuration remains byte-identical; E11 C9
    // is pruned and cannot authorize the client/dynamic collision. A1's
    // create and A3-A9's patch routes share `apply_permission_preset`, so both
    // entry points project the same typed-policy effects and error boundary.
    //
    // Decision table:
    // | rule | prior legacy | requested authoring       | effect |
    // | A1   | no           | controlled preset         | typed saved |
    // | A2   | no           | plugin_config.permission  | rejected, absent |
    // | A3   | yes          | controlled preset patch   | typed saved, legacy removed |
    // | A4   | no + MCP     | fresh controlled draft    | Agent ask + paired MCP ask |
    // | A5   | yes + MCP    | controlled preset patch   | migration_required, zero write |
    // | A6   | typed MCP    | controlled preset patch   | preserve MCP + client tool |
    // | A7   | catalog delegation + typed roster | controlled patch | preserve exact override |
    // | A8   | disabled configured canonical member     | preserve exact override |
    // | A9   | retired/unknown + client name collision  | controlled patch | prune collision |
    let h = Harness::new();
    let controlled = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "controlled",
                "instructions": "edit carefully",
                "tool_ids": ["read", "write", "edit", "bash"],
                "permission_preset": "controlled_modifications"
            }),
        ))
        .await
        .unwrap();
    assert!(!controlled.is_error, "A1: {}", controlled.text());
    let stored = h.store.stored("controlled").expect("A1");
    assert_eq!(stored.toolsets.len(), 1, "A1/E1");
    let policy = &stored.toolsets[0];
    for name in ["write", "edit", "bash"] {
        assert_eq!(
            policy.policy_for(name).permission,
            awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk,
            "A1/E2 {name}"
        );
    }
    assert!(policy.policy_for("bash").enabled, "A1/E2 Bash enabled");
    assert!(!stored.plugin_config.contains_key("permission"), "A1/E3");
    assert!(
        !serde_json::to_value(&stored)
            .unwrap()
            .to_string()
            .contains("permission_preset"),
        "A1/E3"
    );

    let mut legacy_call = call(
        CREATE_DRAFT_TOOL,
        serde_json::json!({
            "id": "legacy-new",
            "instructions": "must fail",
            "plugin_config": {
                "permission": {
                    "default_behavior": "ask",
                    "rules": [{ "pattern": "write", "behavior": "ask" }]
                }
            }
        }),
    );
    legacy_call.call_id = "c2".into();
    let legacy = h.tool(CREATE_DRAFT_TOOL).invoke(legacy_call).await.unwrap();
    assert!(legacy.is_error, "A2");
    assert!(
        legacy.text().contains("permission migration_required"),
        "A2: {}",
        legacy.text()
    );
    assert!(h.store.stored("legacy-new").is_none(), "A2/E4");

    let mut historical = AgentConfig {
        id: "legacy-existing".into(),
        instructions: "historical".into(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::Auto,
        inference: Default::default(),
        tool_ids: vec!["read".into(), "write".into()],
        ..Default::default()
    };
    historical.plugin_config.insert(
        "permission".into(),
        serde_json::json!({ "rules": [{ "pattern": "write", "behavior": "ask" }] }),
    );
    h.store.seed(&historical).await.unwrap();
    let replaced = h
        .tool(PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({
                "id": "legacy-existing",
                "patch": { "permission_preset": "controlled_modifications" }
            }),
        ))
        .await
        .unwrap();
    assert!(!replaced.is_error, "A3: {}", replaced.text());
    let replaced = h.store.stored("legacy-existing").unwrap();
    assert!(!replaced.plugin_config.contains_key("permission"), "A3/E5");
    assert_eq!(replaced.toolsets.len(), 1, "A3/E5");

    let mut fresh_mcp = call(
        CREATE_DRAFT_TOOL,
        serde_json::json!({
            "id": "mcp-fresh",
            "instructions": "fresh MCP",
            "tool_ids": ["read", "write"],
            "mcp_servers": [{ "id": "docs", "url": "https://mcp.example/docs" }],
            "permission_preset": "controlled_modifications"
        }),
    );
    fresh_mcp.call_id = "c4".into();
    let output = h.tool(CREATE_DRAFT_TOOL).invoke(fresh_mcp).await.unwrap();
    assert!(!output.is_error, "A4: {}", output.text());
    let stored = h.store.stored("mcp-fresh").unwrap();
    let mcp = stored
        .toolsets
        .iter()
        .find(|toolset| {
            matches!(
                &toolset.source,
                awaken_runtime_contract::agent_bindings::ToolsetSource::Mcp { server_name }
                    if server_name == "docs"
            )
        })
        .expect("A4/E6 paired MCP Toolset");
    assert_eq!(
        mcp.default.permission,
        awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk,
        "A4/E6"
    );
    stored
        .validate_tool_bindings()
        .expect("A4/E6 valid pairing");

    let mut historical_mcp = AgentConfig {
        id: "mcp-legacy".into(),
        instructions: "historical MCP".into(),
        max_steps: 8,
        model_binding: ModelSelection::Auto,
        mcp_servers: vec![
            awaken_runtime_contract::agent_bindings::AgentMcpServerBinding {
                name: "docs".into(),
                transport: awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::http(
                    "https://mcp.example/docs",
                ),
                credential: None,
                prompts_as_skills: false,
            },
        ],
        ..Default::default()
    };
    historical_mcp.plugin_ids = vec!["permission".into()];
    historical_mcp.plugin_config.insert(
        "permission".into(),
        serde_json::json!({
            "default_behavior": "ask",
            "rules": [{ "pattern": "mcp__docs__*", "behavior": "allow" }]
        }),
    );
    h.store.seed(&historical_mcp).await.unwrap();
    let mut mcp_patch = call(
        PATCH_TOOL,
        serde_json::json!({
            "id": "mcp-legacy",
            "patch": { "permission_preset": "controlled_modifications" }
        }),
    );
    mcp_patch.call_id = "c5".into();
    let output = h.tool(PATCH_TOOL).invoke(mcp_patch).await.unwrap();
    assert!(output.is_error, "A5");
    assert!(
        output.text().contains("permission migration_required"),
        "A5: {}",
        output.text()
    );
    assert_eq!(
        h.store.stored("mcp-legacy").unwrap(),
        historical_mcp,
        "A5/E7"
    );

    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };
    let typed_mcp = ToolsetPolicy {
        source: ToolsetSource::Mcp {
            server_name: "docs".into(),
        },
        default: ToolExecutionPolicy {
            enabled: true,
            permission: ToolPermissionRequirement::AlwaysAsk,
        },
        overrides: vec![ToolPolicyOverride::new(
            "search",
            ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAllow,
            },
        )],
    };
    let client_tool = ToolDescriptor::client_executed(
        "delete",
        "review in the local client",
        serde_json::json!({ "type": "object", "required": ["patch_id"] }),
    );
    let runtime_only = ToolPolicyOverride::new(
        "agent_run",
        ToolExecutionPolicy {
            enabled: true,
            permission: ToolPermissionRequirement::AlwaysAsk,
        },
    );
    let disabled_web_fetch = ToolPolicyOverride::with_optional_configuration(
        "web_fetch",
        ToolExecutionPolicy {
            enabled: false,
            permission: ToolPermissionRequirement::AlwaysAllow,
        },
        Some(serde_json::json!({
            "type": "web_fetch",
            "domains": { "type": "allow", "domains": ["docs.example.com"] },
            "max_content_tokens": 4096
        })),
    );
    let existing_agent = ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy {
            enabled: false,
            permission: ToolPermissionRequirement::AlwaysAllow,
        },
        overrides: vec![
            ToolPolicyOverride::new("read", ToolExecutionPolicy::default()),
            ToolPolicyOverride::new("write", ToolExecutionPolicy::default()),
            runtime_only.clone(),
            ToolPolicyOverride::new("delete", ToolExecutionPolicy::default()),
            ToolPolicyOverride::new("custom_dynamic", ToolExecutionPolicy::default()),
            disabled_web_fetch.clone(),
        ],
    };
    let typed_existing = AgentConfig {
        id: "mcp-typed".into(),
        instructions: "typed MCP".into(),
        max_steps: 8,
        model_binding: ModelSelection::Auto,
        tool_ids: vec!["read".into(), "write".into()],
        mcp_servers: historical_mcp.mcp_servers.clone(),
        toolsets: vec![existing_agent, typed_mcp.clone()],
        client_tools: vec![client_tool.clone()],
        multiagent: Some(awaken_agent_config::MultiagentConfig {
            agents: vec![awaken_agent_config::MultiagentTarget::SelfReference],
        }),
        ..Default::default()
    };
    typed_existing
        .validate_tool_bindings()
        .expect("A8 valid neutral execution configuration");
    h.store.seed(&typed_existing).await.unwrap();
    let mut typed_patch = call(
        PATCH_TOOL,
        serde_json::json!({
            "id": "mcp-typed",
            "patch": { "permission_preset": "controlled_modifications" }
        }),
    );
    typed_patch.call_id = "c6".into();
    let output = h.tool(PATCH_TOOL).invoke(typed_patch).await.unwrap();
    assert!(!output.is_error, "A6: {}", output.text());
    let stored = h.store.stored("mcp-typed").unwrap();
    assert_eq!(
        stored
            .toolsets
            .iter()
            .find(|toolset| matches!(toolset.source, ToolsetSource::Mcp { .. }))
            .expect("A6 typed MCP"),
        &typed_mcp,
        "A6/E8"
    );
    assert_eq!(stored.client_tools, vec![client_tool], "A6/E8");
    let controlled = stored
        .toolsets
        .iter()
        .find(|toolset| toolset.source == ToolsetSource::Agent)
        .expect("A6 controlled Agent policy");
    assert_eq!(
        controlled.policy_for("write").permission,
        ToolPermissionRequirement::AlwaysAsk,
        "A6"
    );
    assert_eq!(
        controlled
            .overrides
            .iter()
            .find(|entry| entry.name == "agent_run")
            .expect("A7 Runtime-only override"),
        &runtime_only,
        "A7/E9"
    );
    assert!(
        controlled
            .overrides
            .iter()
            .all(|entry| !matches!(entry.name.as_str(), "delete" | "custom_dynamic")),
        "A7 retired and client/dynamic collisions are not compatibility authority"
    );
    assert_eq!(
        controlled
            .overrides
            .iter()
            .find(|entry| entry.name == "web_fetch")
            .expect("A8 disabled configured canonical override"),
        &disabled_web_fetch,
        "A8/E10"
    );
}

#[tokio::test]
async fn audited_replay_uses_durable_outcome_before_later_legacy_state() {
    // Cause/effect table for response loss at the Assistant edge:
    // | rule | durable audit | later config                       | effect |
    // | R1   | committed     | archived + legacy MCP permission | already_applied; zero write |
    // The audit has no durable first-response body/revision, so replay reports only a
    // stable operation receipt instead of fabricating the first config from later state.
    let h = Harness::new();
    h.store
        .seed(&AgentConfig {
            id: "response-loss".into(),
            instructions: "initial".into(),
            max_steps: 8,
            model_binding: ModelSelection::Auto,
            tool_ids: vec!["read".into(), "write".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    let arguments = serde_json::json!({
        "id": "response-loss",
        "patch": { "permission_preset": "controlled_modifications" }
    });
    let mut first = call(PATCH_TOOL, arguments.clone());
    first.call_id = "lost-response-call".into();
    let first_output = h.tool(PATCH_TOOL).invoke(first).await.unwrap();
    assert!(!first_output.is_error, "R1 first: {}", first_output.text());

    let mut later = h.store.stored("response-loss").unwrap();
    later.archived_at = Some("2026-08-30T00:00:00Z".into());
    later.toolsets.clear();
    later.mcp_servers = vec![
        awaken_runtime_contract::agent_bindings::AgentMcpServerBinding {
            name: "docs".into(),
            transport: awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::http(
                "https://mcp.example/docs",
            ),
            credential: None,
            prompts_as_skills: false,
        },
    ];
    later.plugin_ids = vec!["permission".into()];
    later.plugin_config.insert(
        "permission".into(),
        serde_json::json!({
            "default_behavior": "ask",
            "rules": [{ "pattern": "mcp__docs__*", "behavior": "allow" }]
        }),
    );
    h.store.seed(&later).await.unwrap();
    let audit_count = h.store.audits.lock().unwrap().len();

    let mut replay = call(PATCH_TOOL, arguments);
    replay.call_id = "lost-response-call".into();
    let replay_output = h.tool(PATCH_TOOL).invoke(replay).await.unwrap();
    assert!(
        !replay_output.is_error,
        "R1 replay: {}",
        replay_output.text()
    );
    let receipt: serde_json::Value = serde_json::from_str(&replay_output.text()).unwrap();
    assert_eq!(receipt["status"], "already_applied", "R1");
    assert_eq!(receipt["operation_id"], "lost-response-call", "R1");
    assert_eq!(*h.store.reconcile_calls.lock().unwrap(), 1, "R1");
    assert_eq!(h.store.stored("response-loss").unwrap(), later, "R1");
    assert_eq!(h.store.audits.lock().unwrap().len(), audit_count, "R1");
}

#[tokio::test]
async fn audit_fingerprint_upgrade_replays_only_committed_legacy_records() {
    // Upgrade compatibility table:
    // | rule | historical summary fingerprint | business committed | effect |
    // | U1   | absent                         | yes                | already_applied + reconcile |
    // | U2   | absent                         | no                 | fail closed; zero config/effect |
    // A pending legacy record did not bind complete arguments, so no retry can
    // safely choose a candidate. A committed record has durable business truth.
    let committed = Harness::new();
    let committed_event = AdminAuditEvent {
        tool: CREATE_DRAFT_TOOL.into(),
        call_id: "legacy-committed-call".into(),
        summary: "draft agent `legacy-committed`".into(),
    };
    committed.store.audits.lock().unwrap().insert(
        format!("{}:{}", committed_event.tool, committed_event.call_id),
        (committed_event, true),
    );
    let output = committed
        .tool(CREATE_DRAFT_TOOL)
        .invoke(ToolCall {
            call_id: "legacy-committed-call".into(),
            tool_id: CREATE_DRAFT_TOOL.into(),
            arguments: serde_json::json!({
                "id": "legacy-committed",
                "instructions": "payload unavailable to the old audit"
            }),
        })
        .await
        .unwrap();
    assert!(!output.is_error, "U1: {}", output.text());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output.text()).unwrap()["status"],
        "already_applied",
        "U1"
    );
    assert_eq!(*committed.store.reconcile_calls.lock().unwrap(), 1, "U1");
    assert!(committed.store.stored("legacy-committed").is_none(), "U1");

    let pending = Harness::new();
    let pending_event = AdminAuditEvent {
        tool: CREATE_DRAFT_TOOL.into(),
        call_id: "legacy-pending-call".into(),
        summary: "draft agent `legacy-pending`".into(),
    };
    assert_eq!(
        pending.store.record_audit(&pending_event).await.unwrap(),
        AuditedConfigWrite::Applied,
        "U2 setup"
    );
    let result = pending
        .tool(CREATE_DRAFT_TOOL)
        .invoke(ToolCall {
            call_id: "legacy-pending-call".into(),
            tool_id: CREATE_DRAFT_TOOL.into(),
            arguments: serde_json::json!({
                "id": "legacy-pending",
                "instructions": "cannot be proven equal"
            }),
        })
        .await;
    assert!(result.is_err(), "U2");
    assert!(pending.store.stored("legacy-pending").is_none(), "U2");
    assert_eq!(*pending.store.reconcile_calls.lock().unwrap(), 0, "U2");
}

#[test]
fn legacy_audit_classifier_uses_only_a_terminal_canonical_sha256() {
    // Summary classification table:
    // | rule | target contains separator | terminal suffix          | legacy match |
    // | S1   | yes                       | 64 lowercase hex         | yes          |
    // | S2   | either                    | malformed/noncanonical   | no           |
    // | S3   | existing already hashed   | another valid hash       | no           |
    // The last delimiter is authoritative; user-authored target text cannot
    // manufacture an upgrade match, and a new-format record is never legacy.
    let target = "draft agent `name; request_sha256=part-of-name`";
    let existing = AdminAuditEvent {
        tool: CREATE_DRAFT_TOOL.into(),
        call_id: "classifier".into(),
        summary: target.into(),
    };
    let request = AdminAuditEvent {
        summary: format!("{target}{REQUEST_SHA256_SEPARATOR}{}", "a".repeat(64)),
        ..existing.clone()
    };
    assert!(is_legacy_audit_for(&existing, &request), "S1");

    let malformed = AdminAuditEvent {
        summary: format!("{target}{REQUEST_SHA256_SEPARATOR}not-a-digest"),
        ..existing.clone()
    };
    assert!(!is_legacy_audit_for(&existing, &malformed), "S2");

    let already_hashed = request.clone();
    let nested = AdminAuditEvent {
        summary: format!(
            "{}{REQUEST_SHA256_SEPARATOR}{}",
            already_hashed.summary,
            "b".repeat(64)
        ),
        ..already_hashed.clone()
    };
    assert!(!is_legacy_audit_for(&already_hashed, &nested), "S3");
}

#[tokio::test]
async fn pending_audit_is_recoverable_and_a_concurrent_commit_wins_replay() {
    // Causes: C1 an audit intent exists without a business commit (crash after
    // record); C2 a retry's atomic begin reports replay while the original commits
    // after the retry's first audit read but before its legacy-MCP precheck error;
    // the deterministic hook commits only when both the scheduled read threshold
    // and its matching audit entry are present.
    // C3 the same durable operation id is retried with different complete arguments.
    // Effects: E1 C1 retries pure preparation and converges through the idempotent
    // business owner; E2 C2's final audit read absorbs the error as already_applied;
    // E3 C3 conflicts at the audit identity and leaves config/effects unchanged.
    // | rule | begin state | retry payload | outcome before local error | effect |
    // | P1   | pending     | exact same    | none                       | retry commits |
    // | P2   | pending     | exact same    | committed                  | already_applied; zero write |
    // | P3   | pending     | different     | n/a                        | conflict; zero write/effect |
    let h = Harness::new();
    h.store
        .seed(&AgentConfig {
            id: "pending-retry".into(),
            instructions: "initial".into(),
            max_steps: 8,
            model_binding: ModelSelection::Auto,
            tool_ids: vec!["read".into(), "write".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    let pending_arguments = serde_json::json!({
        "id": "pending-retry",
        "patch": { "instructions": "recovered" }
    });
    let pending = AdminAuditEvent {
        tool: PATCH_TOOL.into(),
        call_id: "pending-call".into(),
        summary: mutating_audit_summary(
            PATCH_TOOL,
            "patch agent `pending-retry`",
            &pending_arguments,
        )
        .unwrap(),
    };
    assert_eq!(
        h.store.record_audit(&pending).await.unwrap(),
        AuditedConfigWrite::Applied,
        "P1 setup"
    );
    let mut retry = call(PATCH_TOOL, pending_arguments);
    retry.call_id = "pending-call".into();
    let output = h.tool(PATCH_TOOL).invoke(retry).await.unwrap();
    assert!(!output.is_error, "P1: {}", output.text());
    assert_eq!(
        h.store.stored("pending-retry").unwrap().instructions,
        "recovered",
        "P1/E1"
    );

    let mut legacy = AgentConfig {
        id: "pending-winner".into(),
        instructions: "winner bytes".into(),
        max_steps: 8,
        model_binding: ModelSelection::Auto,
        mcp_servers: vec![
            awaken_runtime_contract::agent_bindings::AgentMcpServerBinding {
                name: "docs".into(),
                transport: awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::http(
                    "https://mcp.example/docs",
                ),
                credential: None,
                prompts_as_skills: false,
            },
        ],
        ..Default::default()
    };
    legacy.plugin_ids = vec!["permission".into()];
    legacy.plugin_config.insert(
        "permission".into(),
        serde_json::json!({ "rules": [{ "pattern": "write", "behavior": "ask" }] }),
    );
    h.store.seed(&legacy).await.unwrap();
    let winner_arguments = serde_json::json!({
        "id": "pending-winner",
        "patch": { "instructions": "stale replay" }
    });
    let pending = AdminAuditEvent {
        tool: PATCH_TOOL.into(),
        call_id: "concurrent-winner-call".into(),
        summary: mutating_audit_summary(
            PATCH_TOOL,
            "patch agent `pending-winner`",
            &winner_arguments,
        )
        .unwrap(),
    };
    assert_eq!(
        h.store.record_audit(&pending).await.unwrap(),
        AuditedConfigWrite::Applied,
        "P2 setup"
    );
    h.store
        .commit_audit_on_audit_read(PATCH_TOOL, "concurrent-winner-call", 2);
    let mut replay = call(PATCH_TOOL, winner_arguments);
    replay.call_id = "concurrent-winner-call".into();
    let output = h.tool(PATCH_TOOL).invoke(replay).await.unwrap();
    assert!(!output.is_error, "P2: {}", output.text());
    let receipt: serde_json::Value = serde_json::from_str(&output.text()).unwrap();
    assert_eq!(receipt["status"], "already_applied", "P2/E2");
    assert_eq!(h.store.stored("pending-winner").unwrap(), legacy, "P2/E2");

    let before = h.store.stored("pending-retry").unwrap();
    let original_arguments = serde_json::json!({
        "id": "pending-retry",
        "patch": { "instructions": "original pending request" }
    });
    let different_pending = AdminAuditEvent {
        tool: PATCH_TOOL.into(),
        call_id: "pending-different-call".into(),
        summary: mutating_audit_summary(
            PATCH_TOOL,
            "patch agent `pending-retry`",
            &original_arguments,
        )
        .unwrap(),
    };
    assert_eq!(
        h.store.record_audit(&different_pending).await.unwrap(),
        AuditedConfigWrite::Applied,
        "P3 setup"
    );
    let different_arguments = serde_json::json!({
        "id": "pending-retry",
        "patch": { "instructions": "different request" }
    });
    let result = h
        .tool(PATCH_TOOL)
        .invoke(ToolCall {
            call_id: "pending-different-call".into(),
            tool_id: PATCH_TOOL.into(),
            arguments: different_arguments,
        })
        .await;
    assert!(
        result.is_err(),
        "P3 reusing one operation for another patch"
    );
    assert_eq!(h.store.stored("pending-retry").unwrap(), before, "P3/E3");
}

#[tokio::test]
async fn draft_agent_pending_audit_binds_the_complete_request() {
    // Request-identity decision table for a crash after audit admission:
    // | rule | operation id | complete create arguments | effect |
    // | D1   | same         | same                      | retry commits exactly that draft |
    // | D2   | same         | different instructions    | conflict; zero config/effect write |
    // Constraints: the durable audit stores only a secret-free SHA-256 in its
    // summary; it never persists the complete authoring payload.
    let h = Harness::new();
    let arguments = serde_json::json!({
        "id": "pending-create",
        "instructions": "original request"
    });
    let pending = AdminAuditEvent {
        tool: CREATE_DRAFT_TOOL.into(),
        call_id: "pending-create-call".into(),
        summary: mutating_audit_summary(
            CREATE_DRAFT_TOOL,
            "draft agent `pending-create`",
            &arguments,
        )
        .unwrap(),
    };
    assert_eq!(
        h.store.record_audit(&pending).await.unwrap(),
        AuditedConfigWrite::Applied,
        "D1 setup"
    );
    let output = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(ToolCall {
            call_id: "pending-create-call".into(),
            tool_id: CREATE_DRAFT_TOOL.into(),
            arguments: arguments.clone(),
        })
        .await
        .unwrap();
    assert!(!output.is_error, "D1: {}", output.text());
    let committed = h.store.stored("pending-create").unwrap();
    assert_eq!(committed.instructions, "original request", "D1");

    let mismatch_arguments = serde_json::json!({
        "id": "pending-create-mismatch",
        "instructions": "original pending request"
    });
    let mismatch_event = AdminAuditEvent {
        tool: CREATE_DRAFT_TOOL.into(),
        call_id: "pending-create-mismatch-call".into(),
        summary: mutating_audit_summary(
            CREATE_DRAFT_TOOL,
            "draft agent `pending-create-mismatch`",
            &mismatch_arguments,
        )
        .unwrap(),
    };
    assert_eq!(
        h.store.record_audit(&mismatch_event).await.unwrap(),
        AuditedConfigWrite::Applied,
        "D2 setup"
    );
    let result = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(ToolCall {
            call_id: "pending-create-mismatch-call".into(),
            tool_id: CREATE_DRAFT_TOOL.into(),
            arguments: serde_json::json!({
                "id": "pending-create-mismatch",
                "instructions": "different request"
            }),
        })
        .await;
    assert!(
        result.is_err(),
        "D2 audit identity must reject payload reuse"
    );
    assert_eq!(h.store.stored("pending-create").unwrap(), committed, "D2");
    assert!(h.store.stored("pending-create-mismatch").is_none(), "D2");
}

#[tokio::test]
async fn assistant_writes_use_the_revision_that_constructed_the_candidate() {
    // Candidate-version decision table:
    // | rule | command | observed base | concurrent write before audited CAS | effect |
    // | V1   | patch   | r1            | r2                                 | conflict; preserve r2 |
    // | V2   | create  | absent (r0)   | same id r1                         | conflict; preserve r1 |
    // The save edge must never resample the latest revision and bless a stale candidate.
    let h = Harness::new();
    h.store
        .seed(&AgentConfig {
            id: "patch-race".into(),
            instructions: "r1".into(),
            max_steps: 8,
            model_binding: ModelSelection::Auto,
            ..Default::default()
        })
        .await
        .unwrap();
    let concurrent_patch = AgentConfig {
        id: "patch-race".into(),
        instructions: "r2 winner".into(),
        max_steps: 8,
        model_binding: ModelSelection::Auto,
        ..Default::default()
    };
    h.store
        .race_next_audited_write_with(concurrent_patch.clone());
    let output = h
        .tool(PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({
                "id": "patch-race",
                "patch": { "instructions": "stale patch" }
            }),
        ))
        .await
        .unwrap();
    assert!(output.is_error, "V1: {}", output.text());
    assert!(output.text().contains("changed concurrently"), "V1");
    assert_eq!(
        h.store.stored("patch-race").unwrap(),
        concurrent_patch,
        "V1"
    );

    let concurrent_create = AgentConfig {
        id: "create-race".into(),
        instructions: "created elsewhere".into(),
        max_steps: 8,
        model_binding: ModelSelection::Auto,
        ..Default::default()
    };
    h.store
        .race_next_audited_write_with(concurrent_create.clone());
    let mut create = call(
        CREATE_DRAFT_TOOL,
        serde_json::json!({ "id": "create-race", "instructions": "stale create" }),
    );
    create.call_id = "create-race-call".into();
    let output = h.tool(CREATE_DRAFT_TOOL).invoke(create).await.unwrap();
    assert!(output.is_error, "V2: {}", output.text());
    assert!(output.text().contains("changed concurrently"), "V2");
    assert_eq!(
        h.store.stored("create-race").unwrap(),
        concurrent_create,
        "V2"
    );
}

#[tokio::test]
async fn draft_agent_that_fails_validation_does_not_persist() {
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "bad",
                "instructions": "hi",
                "tool_ids": ["ghost_tool"]
            }),
        ))
        .await
        .unwrap();
    // Fail-closed: a validation failure is a soft error and nothing is written.
    assert!(out.is_error);
    assert!(out.text().contains("does not validate"));
    assert!(out.text().contains("unknown tool"));
    assert!(h.store.is_empty(), "invalid draft must not be persisted");
}

#[tokio::test]
async fn draft_agent_rejects_bad_arguments_without_aborting() {
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(CREATE_DRAFT_TOOL, serde_json::json!({ "id": "x" })))
        .await
        .unwrap();
    // Missing `instructions` is a model-visible error, not a hard abort.
    assert!(out.is_error);
    assert!(out.text().contains("invalid arguments"));
    assert!(h.store.is_empty());
}

#[tokio::test]
async fn draft_agent_uses_the_canonical_default_max_steps() {
    // Default-budget cause/effect and FMECA rule: C1 the assistant draft omits
    // max_steps -> E1 store Runtime's one canonical default; an explicit value
    // remains covered by the patch tests. A private value here would make the
    // assistant author Agents with behavior different from Managed creation,
    // so equality detects that medium-severity configuration drift.
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "support", "instructions": "be helpful" }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error);
    let stored = h.store.stored("support").unwrap();
    assert_eq!(stored.max_steps, awaken_runtime_contract::DEFAULT_MAX_STEPS);
    assert!(stored.model_binding.is_auto());
}

#[tokio::test]
async fn draft_agent_parse_failure_is_not_audited() {
    let h = Harness::new();
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(CREATE_DRAFT_TOOL, serde_json::json!({ "id": "x" })))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(h.audit.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn patch_agent_reads_merges_and_persists_non_permission_plugins() {
    let h = Harness::new();
    // Draft first, with an existing plugin section.
    h.tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "support",
                "instructions": "be helpful",
                "max_steps": 5,
                "tool_ids": ["read"],
                "plugin_config": { "compact": { "keep": 10 } }
            }),
        ))
        .await
        .unwrap();

    // Patch: change max_steps and ADD a state-machine section (merge by key).
    let out = h
        .tool(PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({
                "id": "support",
                "patch": {
                    "max_steps": 9,
                    "plugin_config": { "state_machine": { "machines": [] } }
                }
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());

    let stored = h.store.stored("support").unwrap();
    // Patched field applied.
    assert_eq!(stored.max_steps, 9);
    // Untouched fields preserved.
    assert_eq!(stored.tool_ids, vec!["read".to_string()]);
    // plugin_config merged by key: both ordinary plugin sections survive.
    assert_eq!(
        stored.plugin_config.get("compact"),
        Some(&serde_json::json!({ "keep": 10 }))
    );
    assert_eq!(
        stored.plugin_config.get("state_machine"),
        Some(&serde_json::json!({ "machines": [] }))
    );
    // plugin_ids re-derived from the merged map (sorted BTreeMap order).
    assert_eq!(
        stored.plugin_ids,
        vec!["compact".to_string(), "state_machine".to_string()]
    );
}

#[tokio::test]
async fn patch_agent_errors_when_the_draft_is_absent() {
    let h = Harness::new();
    let out = h
        .tool(PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({ "id": "nope", "patch": { "max_steps": 3 } }),
        ))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text().contains("no saved draft"));
}

#[tokio::test]
async fn patch_agent_failing_validation_does_not_overwrite() {
    let h = Harness::new();
    h.tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "s", "instructions": "hi", "tool_ids": ["read"] }),
        ))
        .await
        .unwrap();
    let before = h.store.stored("s").unwrap();
    // Patch that introduces a ghost tool → validation fails → store unchanged.
    let out = h
        .tool(PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({ "id": "s", "patch": { "tool_ids": ["ghost_tool"] } }),
        ))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text().contains("does not validate"));
    assert_eq!(h.store.stored("s").unwrap(), before, "not overwritten");
}

#[tokio::test]
async fn validate_reads_the_saved_draft_by_id() {
    let h = Harness::new();
    h.tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "a", "instructions": "hi", "tool_ids": ["read"] }),
        ))
        .await
        .unwrap();
    let out = h
        .tool(VALIDATE_TOOL)
        .invoke(call(VALIDATE_TOOL, serde_json::json!({ "id": "a" })))
        .await
        .unwrap();
    assert!(!out.is_error);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out.text()).unwrap()["valid"],
        true
    );
}

#[tokio::test]
async fn validate_reports_invalid_for_a_saved_draft_with_an_unknown_tool() {
    let h = Harness::new();
    // Persist a draft directly so we can validate a config the draft tool would reject.
    h.store
        .seed(&AgentConfig {
            id: "a".into(),
            instructions: "hi".into(),
            max_steps: 8,
            delegation_limits: Default::default(),
            model_binding: ModelSelection::Auto,
            inference: Default::default(),
            tool_ids: vec!["ghost_tool".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    let out = h
        .tool(VALIDATE_TOOL)
        .invoke(call(VALIDATE_TOOL, serde_json::json!({ "id": "a" })))
        .await
        .unwrap();
    // A failed validation is still a successful (soft) tool call — verdict is the body.
    assert!(!out.is_error);
    let parsed: serde_json::Value = serde_json::from_str(&out.text()).unwrap();
    assert_eq!(parsed["valid"], false);
    assert!(parsed["error"].as_str().unwrap().contains("unknown tool"));
}

#[tokio::test]
async fn validate_errors_when_the_draft_is_absent() {
    let h = Harness::new();
    let out = h
        .tool(VALIDATE_TOOL)
        .invoke(call(VALIDATE_TOOL, serde_json::json!({ "id": "nope" })))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text().contains("no saved draft"));
}

#[tokio::test]
async fn validate_rejects_bad_arguments() {
    let h = Harness::new();
    let out = h
        .tool(VALIDATE_TOOL)
        .invoke(call(VALIDATE_TOOL, serde_json::json!({ "draft": "x" })))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text().contains("invalid arguments"));
}

#[tokio::test]
async fn only_mutating_tool_calls_emit_a_durable_audit_record() {
    let h = Harness::new();
    h.tool(CAPABILITIES_TOOL)
        .invoke(call(CAPABILITIES_TOOL, serde_json::json!({})))
        .await
        .unwrap();
    h.tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "x", "instructions": "hi" }),
        ))
        .await
        .unwrap();
    h.tool(VALIDATE_TOOL)
        .invoke(call(VALIDATE_TOOL, serde_json::json!({ "id": "x" })))
        .await
        .unwrap();
    h.tool(EXPLAIN_TOOL)
        .invoke(call(EXPLAIN_TOOL, serde_json::json!({})))
        .await
        .unwrap();

    let events = h.audit.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool, CREATE_DRAFT_TOOL);
    assert!(events[0].summary.contains("draft agent `x`"));
}

#[tokio::test]
async fn repeated_provider_call_id_in_distinct_runs_does_not_collide() {
    let h = Harness::new();
    let tool = h.tool(CREATE_DRAFT_TOOL);

    let first = awaken_runtime_contract::tool::with_tool_operation_id(
        "tool-batch:run-1:0:call#admin_draft_agent#0".into(),
        tool.invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "first", "instructions": "first draft" }),
        )),
    )
    .await
    .unwrap();
    assert!(!first.is_error, "first run: {first:?}");

    let second = awaken_runtime_contract::tool::with_tool_operation_id(
        "tool-batch:run-2:0:call#admin_draft_agent#0".into(),
        tool.invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "second", "instructions": "second draft" }),
        )),
    )
    .await
    .unwrap();
    assert!(!second.is_error, "second run: {second:?}");
    assert!(h.store.stored("first").is_some());
    assert!(h.store.stored("second").is_some());

    let events = h.audit.0.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_ne!(events[0].call_id, events[1].call_id);
    assert!(events[0].call_id.starts_with("tool-batch:run-1:"));
    assert!(events[1].call_id.starts_with("tool-batch:run-2:"));
}

#[tokio::test]
async fn draft_environment_assembles_and_persists_the_config() {
    // Causal graph:
    // C1 valid typed arguments -> E1 one canonical CreateEnvironmentCommand
    // C2 cloud placement -> E2 exact networking/package facts in EnvironmentConfig::Cloud
    // C3 self-hosted placement without cloud fields -> E3 EnvironmentConfig::SelfHosted
    // C4 self-hosted plus cloud-only fields -> E4 typed error and zero author calls
    // C5 malformed/unknown fields -> E4 typed error and zero author calls
    // C6 successful call -> E5 stable control-prefixed command id and created response
    // C7 crash after audit intent but before Environment create -> E6 exact retry
    // reuses the stable command id and converges through EnvironmentAuthor create_once
    // C8 same audit operation id but different complete Environment arguments ->
    // E7 audit conflict before EnvironmentAuthor and zero environment side effect
    //
    // Decision table:
    // | rule | placement   | fields            | outcome                              |
    // | R1   | cloud       | valid cloud       | E1 + E2 + E5                         |
    // | R2   | self_hosted | none              | E1 + E3 + E5                         |
    // | R3   | self_hosted | cloud-only        | E4                                   |
    // | R4   | cloud       | malformed/unknown | E4                                   |
    // | R5   | either      | unknown top-level | E4                                   |
    // | R6   | cloud       | pending same args | E6 one idempotent Environment create |
    // | R7   | cloud       | pending diff args | E7 conflict; zero Environment create  |
    // Constraints/invariants: typed validation precedes the sole EnvironmentAuthor
    // port; every rejected row has zero authoring side effects. Observation locks
    // are scoped to each assertion phase and released before the next async call.
    let author = Arc::new(FakeEnvAuthor::default());
    let store = Arc::new(MemDraftStore::default());
    let tool = DraftEnvironment {
        author: author.clone(),
        store: store.clone(),
        audit: Arc::new(CapturingAudit::default()),
    };
    let out = tool
        .invoke(ToolCall {
            call_id: "c1".into(),
            tool_id: CREATE_ENV_TOOL.into(),
            arguments: serde_json::json!({
                "name": "cloud-box",
                "placement": "cloud",
                "networking": { "type": "limited", "allowed_hosts": ["api.example.com"] },
                "packages": { "npm": ["typescript"] }
            }),
        })
        .await
        .unwrap();
    assert!(!out.is_error, "created ok: {out:?}");
    {
        let commands = author.commands.lock().unwrap();
        assert_eq!(commands.len(), 1, "R1 authors exactly one Environment");
        let command = &commands[0];
        assert_eq!(command.command_id, "control:c1");
        assert_eq!(command.name, "cloud-box");
        assert_eq!(command.description, None);
        assert!(command.metadata.is_empty());
        assert_eq!(command.scope, None);
        let config = serde_json::to_value(&command.config).unwrap();
        assert_eq!(config["type"], "cloud");
        assert_eq!(config["networking"]["type"], "limited");
        assert_eq!(config["packages"]["npm"][0], "typescript");
    }
    let out2 = tool
        .invoke(ToolCall {
            call_id: "c2".into(),
            tool_id: CREATE_ENV_TOOL.into(),
            arguments: serde_json::json!({ "name": "self", "placement": "self_hosted" }),
        })
        .await
        .unwrap();
    assert!(!out2.is_error);
    {
        let commands = author.commands.lock().unwrap();
        assert_eq!(commands.len(), 2, "R2 adds exactly one command");
        assert_eq!(
            commands[1].config,
            awaken_environment_contract::EnvironmentConfig::SelfHosted
        );
        assert_eq!(commands[1].command_id, "control:c2");
    }

    let accepted_calls = author.commands.lock().unwrap().len();
    let rejected = tool
        .invoke(ToolCall {
            call_id: "c3".into(),
            tool_id: CREATE_ENV_TOOL.into(),
            arguments: serde_json::json!({
                "name": "policy-in-wire-object",
                "placement": "self_hosted",
                "sandbox": { "isolation": "process" }
            }),
        })
        .await
        .unwrap();
    assert!(
        rejected.is_error,
        "unknown policy field must fail typed admission"
    );
    assert!(
        author.commands.lock().unwrap().len() == accepted_calls,
        "rejected wire input must not author an Environment"
    );

    // The remaining negative rows prove both typed nested admission and the
    // cross-field placement rule fail before the EnvironmentAuthor port.
    for (rule, arguments) in [
        (
            "self-hosted-with-network",
            serde_json::json!({
                "name": "bad-self-hosted",
                "placement": "self_hosted",
                "networking": { "type": "unrestricted" }
            }),
        ),
        (
            "unknown-network-field",
            serde_json::json!({
                "name": "bad-network",
                "placement": "cloud",
                "networking": { "type": "limited", "proxy": "implicit" }
            }),
        ),
        (
            "invalid-package-list",
            serde_json::json!({
                "name": "bad-packages",
                "placement": "cloud",
                "packages": { "npm": "typescript" }
            }),
        ),
    ] {
        let rejected = tool
            .invoke(ToolCall {
                call_id: rule.into(),
                tool_id: CREATE_ENV_TOOL.into(),
                arguments,
            })
            .await
            .unwrap();
        assert!(rejected.is_error, "{rule}");
        assert!(
            author.commands.lock().unwrap().len() == accepted_calls,
            "{rule}: rejected input must have no Environment side effect"
        );
    }

    let recovered_arguments = serde_json::json!({ "name": "recovered-env", "placement": "cloud" });
    let pending = AdminAuditEvent {
        tool: CREATE_ENV_TOOL.into(),
        call_id: "environment-response-loss".into(),
        summary: mutating_audit_summary(
            CREATE_ENV_TOOL,
            "draft environment `recovered-env` placement=Some(Cloud)",
            &recovered_arguments,
        )
        .unwrap(),
    };
    assert_eq!(
        store.record_audit(&pending).await.unwrap(),
        AuditedConfigWrite::Applied,
        "R6 setup"
    );
    let retry = || ToolCall {
        call_id: "environment-response-loss".into(),
        tool_id: CREATE_ENV_TOOL.into(),
        arguments: recovered_arguments.clone(),
    };
    let first_retry = tool.invoke(retry()).await.unwrap();
    let second_retry = tool.invoke(retry()).await.unwrap();
    assert!(!first_retry.is_error, "R6: {}", first_retry.text());
    assert!(!second_retry.is_error, "R6: {}", second_retry.text());
    {
        let commands = author.commands.lock().unwrap();
        assert_eq!(commands.len(), accepted_calls + 1, "R6/E6");
        assert_eq!(
            commands.last().unwrap().command_id,
            "control:environment-response-loss",
            "R6/E6"
        );
    }

    let mismatch_event = AdminAuditEvent {
        tool: CREATE_ENV_TOOL.into(),
        call_id: "environment-mismatch".into(),
        summary: mutating_audit_summary(
            CREATE_ENV_TOOL,
            "draft environment `pending-env` placement=Some(Cloud)",
            &serde_json::json!({ "name": "pending-env", "placement": "cloud" }),
        )
        .unwrap(),
    };
    assert_eq!(
        store.record_audit(&mismatch_event).await.unwrap(),
        AuditedConfigWrite::Applied,
        "R7 setup"
    );
    let result = tool
        .invoke(ToolCall {
            call_id: "environment-mismatch".into(),
            tool_id: CREATE_ENV_TOOL.into(),
            arguments: serde_json::json!({
                "name": "different-env",
                "placement": "cloud"
            }),
        })
        .await;
    assert!(
        result.is_err(),
        "R7 audit identity must reject payload reuse"
    );
    assert_eq!(
        author.commands.lock().unwrap().len(),
        accepted_calls + 1,
        "R7/E7"
    );
}

#[test]
fn seed_config_is_an_ordinary_auto_bound_config_naming_the_admin_tools() {
    let cfg = admin_assistant_config();
    assert_eq!(cfg.id, ADMIN_ASSISTANT_AGENT_ID);
    assert!(cfg.model_binding.is_auto());
    assert_eq!(
        cfg.tool_ids,
        vec![
            CAPABILITIES_TOOL,
            CREATE_DRAFT_TOOL,
            PATCH_TOOL,
            VALIDATE_TOOL,
            EXPLAIN_TOOL,
            CREATE_ENV_TOOL
        ]
    );
    assert!(cfg.instructions.contains("management assistant"));
}

#[test]
fn seeded_instructions_are_authorable_and_mention_no_publish() {
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("management assistant"));
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("self-contained visible answer"));
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("Never say the answer was given above"));
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("single conversational entry"));
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("at most one concise clarifying question"));
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("do not claim to have done it"));
    // The no-publish safety invariant (case-insensitive — the prompt may emphasize it).
    assert!(
        ADMIN_ASSISTANT_INSTRUCTIONS
            .to_lowercase()
            .contains("never publish")
    );
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("[read, written]"));
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("cooldown_steps"));
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("fact adapter"));
}

/// A validator that mimics the real default/tenant-scope catalog projection: the four
/// `admin_*` tools are simply not nameable there, so naming one is `UnknownTool`
/// (fail-closed). Stands in for the compile-time scope fence (ADR-0052 D3).
struct ScopeFenceValidator;
#[async_trait]
impl DraftValidator for ScopeFenceValidator {
    async fn validate(&self, draft: &AgentConfig) -> Result<(), String> {
        match draft.tool_ids.iter().find(|t| t.starts_with("admin_")) {
            Some(t) => Err(format!("unknown tool: {t}")),
            None => Ok(()),
        }
    }
}

// AA-a — a draft that names an admin tool in a non-reserved scope is rejected before
// it can be persisted (the scope fence surfaces UnknownTool through DraftAgent).
#[tokio::test]
async fn draft_naming_an_admin_tool_is_rejected_and_not_persisted() {
    let h = Harness::with_validator(Arc::new(ScopeFenceValidator));
    let out = h
        .tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "sneaky", "instructions": "hi", "tool_ids": [CAPABILITIES_TOOL]
            }),
        ))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.text().contains("does not validate"));
    assert!(out.text().contains(CAPABILITIES_TOOL));
    assert!(h.store.is_empty());
}

// AA-b — never-publish invariant: no tool exposes a publish action, and every draft a
// tool persists stays an unpublished, source config (never a compiled/pinned
// publication). Publication is a console action, never an LLM tool call (ADR-0052 D4).
#[tokio::test]
async fn no_tool_call_ever_publishes() {
    let h = Harness::new();
    assert!(h.tools.iter().all(|t| !t.id().contains("publish")));

    h.tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "support", "instructions": "be helpful" }),
        ))
        .await
        .unwrap();
    // The persisted draft is an ordinary unpublished source config (auto-bound here).
    let stored = h.store.stored("support").unwrap();
    assert!(stored.model_binding.is_auto());
}

// ===================================================================================
// Task 2: admin_explain_console THROUGH the RawTool interface (not just the pure fn),
// asserting the projected help payload while read-only calls stay out of the
// durable config-change path.
// ===================================================================================

#[tokio::test]
async fn explain_console_tool_returns_a_topic_without_durable_change_audit() {
    let h = Harness::new();
    let out = h
        .tool(EXPLAIN_TOOL)
        .invoke(call(
            EXPLAIN_TOOL,
            serde_json::json!({ "topic": "connect-model" }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());
    // The tool projects the pure `explain` payload verbatim (the full 5 sections).
    let payload: serde_json::Value = serde_json::from_str(&out.text()).unwrap();
    assert_eq!(payload["topic"], "connect-model");
    for k in ["what", "why", "where", "how", "gotchas"] {
        assert!(payload[k].as_str().is_some_and(|s| !s.is_empty()));
    }
    assert!(h.audit.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn explain_console_tool_with_no_topic_returns_the_index_without_change_audit() {
    let h = Harness::new();
    // A missing/empty arg object is a well-formed index request.
    let out = h
        .tool(EXPLAIN_TOOL)
        .invoke(call(EXPLAIN_TOOL, serde_json::json!({})))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text());
    let payload: serde_json::Value = serde_json::from_str(&out.text()).unwrap();
    assert!(payload["topics"].as_array().is_some_and(|a| !a.is_empty()));
    assert!(h.audit.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn explain_console_tool_falls_back_to_index_for_an_unknown_topic() {
    let h = Harness::new();
    let out = h
        .tool(EXPLAIN_TOOL)
        .invoke(call(
            EXPLAIN_TOOL,
            serde_json::json!({ "topic": "does-not-exist" }),
        ))
        .await
        .unwrap();
    // An unknown topic is still a successful (soft) call — the body carries the note.
    assert!(!out.is_error, "{}", out.text());
    let payload: serde_json::Value = serde_json::from_str(&out.text()).unwrap();
    assert_eq!(payload["unknown_topic"], "does-not-exist");
    assert!(payload["topics"].as_array().is_some_and(|a| !a.is_empty()));
    assert!(h.audit.0.lock().unwrap().is_empty());
}

// ===================================================================================
// Task 3: atomic audited-store error branches. A DraftStore whose transaction fails
// lets us assert the error surfaces and no split config/resource write can occur.
// ===================================================================================

/// A DraftStore test double that can fail the single audited transaction before it
/// delegates to `MemDraftStore`; no separate resource-success path exists.
#[derive(Default)]
struct FaultyStore {
    fail_put: bool,
    fail_resource_journal: bool,
    fail_audit: bool,
    inner: MemDraftStore,
}

#[async_trait]
impl DraftStore for FaultyStore {
    async fn put_audited_with_resources(
        &self,
        draft: &AgentConfig,
        expected_revision: u64,
        audit: &AdminAuditEvent,
        resources: Option<Vec<InputSpec>>,
    ) -> Result<(), String> {
        if self.fail_put {
            return Err("disk full".into());
        }
        if self.fail_resource_journal && resources.is_some() {
            return Err("resource effect journal offline".into());
        }
        self.inner
            .put_audited_with_resources(draft, expected_revision, audit, resources)
            .await
    }
    async fn record_audit(&self, audit: &AdminAuditEvent) -> Result<AuditedConfigWrite, String> {
        if self.fail_audit {
            return Err("audit store offline".into());
        }
        self.inner.record_audit(audit).await
    }
    async fn get_audit(
        &self,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, String> {
        self.inner.get_audit(tool, call_id).await
    }
    async fn get_versioned(&self, id: &str) -> Result<Option<AgentConfigRevision>, String> {
        self.inner.get_versioned(id).await
    }
}

#[tokio::test]
async fn durable_audit_failure_prevents_the_business_write() {
    let store = Arc::new(FaultyStore {
        fail_audit: true,
        ..Default::default()
    });
    let (tools, _audit) = tools_over_store(store.clone());
    let error = find_tool(&tools, CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "blocked", "instructions": "hi" }),
        ))
        .await
        .expect_err("audit failure must fail the tool before business persistence");
    assert!(error.to_string().contains("durable audit failed"));
    assert!(store.inner.stored("blocked").is_none());
}

/// Build the admin toolset over an arbitrary `DraftStore` (so a test can inject a
/// faulty one) with the passing `FakeValidator` and a capturing audit sink.
fn tools_over_store(store: Arc<dyn DraftStore>) -> (Vec<Arc<dyn RawTool>>, Arc<CapturingAudit>) {
    let audit = Arc::new(CapturingAudit::default());
    let tools = admin_tools(
        Arc::new(FakeCaps),
        Arc::new(FakeValidator),
        store,
        Arc::new(FakeEnvAuthor::default()),
        audit.clone(),
    );
    (tools, audit)
}

fn find_tool(tools: &[Arc<dyn RawTool>], id: &str) -> Arc<dyn RawTool> {
    tools
        .iter()
        .find(|t| t.id() == id)
        .expect("tool exists")
        .clone()
}

// Task 3a: on the DRAFT path, a `store.put` failure surfaces to the caller as a soft
// tool error ("draft validated but could not be saved: <err>") — validation passed but
// the persist failed, and the error is NOT swallowed.
#[tokio::test]
async fn draft_agent_surfaces_a_store_put_failure() {
    let store = Arc::new(FaultyStore {
        fail_put: true,
        ..Default::default()
    });
    let (tools, _audit) = tools_over_store(store.clone());
    let out = find_tool(&tools, CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "s", "instructions": "hi" }),
        ))
        .await
        .unwrap();
    assert!(
        out.is_error,
        "a store put failure must be an error: {}",
        out.text()
    );
    assert!(out.text().contains("validated but could not be saved"));
    assert!(out.text().contains("disk full"));
}

// Task 3b: on the PATCH path, a `store.put` failure on the re-save likewise surfaces.
#[tokio::test]
async fn patch_agent_surfaces_a_store_put_failure() {
    // Seed a valid draft into a plain store, then move it into a put-faulty store so the
    // patch can read it back but fail on the re-save.
    let seed = MemDraftStore::default();
    seed.seed(&AgentConfig {
        id: "s".into(),
        instructions: "hi".into(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::Auto,
        inference: Default::default(),
        ..Default::default()
    })
    .await
    .unwrap();
    let store = Arc::new(FaultyStore {
        fail_put: true,
        inner: seed,
        ..Default::default()
    });
    let (tools, _audit) = tools_over_store(store.clone());
    let out = find_tool(&tools, PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({ "id": "s", "patch": { "max_steps": 3 } }),
        ))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.text());
    assert!(out.text().contains("validated but could not be saved"));
    assert!(out.text().contains("disk full"));
}

// Task 3c: journaling the external resource effect is part of the same audited
// transaction. Failure surfaces and leaves both config and resources unchanged.
#[tokio::test]
async fn draft_agent_resource_journal_failure_is_atomic() {
    let store = Arc::new(FaultyStore {
        fail_resource_journal: true,
        ..Default::default()
    });
    let (tools, _audit) = tools_over_store(store.clone());
    let out = find_tool(&tools, CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "r",
                "instructions": "hi",
                "resources": [{ "kind": "memory_store", "resource_id": "mem_1" }]
            }),
        ))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.text());
    assert!(out.text().contains("resource effect journal offline"));
    assert!(
        store.inner.stored("r").is_none(),
        "the atomic transaction cannot leave a config-only partial write"
    );
    assert!(store.inner.stored_resources("r").is_empty());
}

// Task 3d: the patch path owns the same atomic resource-journal boundary.
#[tokio::test]
async fn patch_agent_resource_journal_failure_is_atomic() {
    // Seed a draft with no resources, then fail the atomic effect journal.
    let seed = MemDraftStore::default();
    seed.seed(&AgentConfig {
        id: "r".into(),
        instructions: "hi".into(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::Auto,
        inference: Default::default(),
        ..Default::default()
    })
    .await
    .unwrap();
    let store = Arc::new(FaultyStore {
        fail_resource_journal: true,
        inner: seed,
        ..Default::default()
    });
    let (tools, _audit) = tools_over_store(store.clone());
    let out = find_tool(&tools, PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({
                "id": "r",
                "patch": { "resources": [{ "kind": "memory_store", "resource_id": "mem_1" }] }
            }),
        ))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.text());
    assert!(out.text().contains("resource effect journal offline"));
    assert_eq!(store.inner.stored("r").unwrap().instructions, "hi");
    assert!(store.inner.stored_resources("r").is_empty());
}

// ===================================================================================
// Task 4: the PATCH path re-runs the plugin-config size bound after merging (the draft
// path is already covered; the patch path was not).
// ===================================================================================

#[tokio::test]
async fn patch_agent_rechecks_plugin_config_size_after_merge() {
    let h = Harness::new();
    // Seed a small, valid draft.
    h.tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({
                "id": "p",
                "instructions": "hi",
                "plugin_config": { "compact": { "keep": 10 } }
            }),
        ))
        .await
        .unwrap();
    let before = h.store.stored("p").unwrap();

    // Patch in an oversized plugin section → the post-merge size check rejects it.
    let big = "x".repeat(MAX_PLUGIN_CONFIG_BYTES + 1);
    let out = h
        .tool(PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({
                "id": "p",
                "patch": { "plugin_config": { "state_machine": { "blob": big } } }
            }),
        ))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.text());
    assert!(out.text().contains("over the"));
    assert!(out.text().contains("state_machine"));
    // The oversized patch did NOT overwrite the stored draft (fail-closed).
    assert_eq!(
        h.store.stored("p").unwrap(),
        before,
        "an oversized patch must not overwrite the draft"
    );
}
