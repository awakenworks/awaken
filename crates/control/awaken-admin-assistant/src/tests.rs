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
            runtimes: vec![
                serde_json::json!({ "id": "acp:claude", "label": "Claude Code", "kind": "acp", "cli": "claude" }),
            ],
            sandbox: Some(
                serde_json::json!({ "config_schema": { "type": "object" }, "presets": [] }),
            ),
        }
    }
}

/// Records the last environment authored, so a test can assert the tool persisted it.
#[derive(Default)]
struct FakeEnvAuthor {
    last: std::sync::Mutex<Option<(String, serde_json::Value)>>,
}
#[async_trait]
impl EnvironmentAuthor for FakeEnvAuthor {
    async fn create(&self, name: &str, config: serde_json::Value) -> Result<String, String> {
        *self.last.lock().unwrap() = Some((name.to_string(), config));
        Ok("env_test_0".to_string())
    }
}

/// Rejects any draft that names a tool id containing "ghost" (stands in for the real
/// compile-time `UnknownTool` fence).
struct FakeValidator;
impl DraftValidator for FakeValidator {
    fn validate(&self, draft: &AgentConfig) -> Result<(), String> {
        if draft.tool_ids.iter().any(|t| t.contains("ghost")) {
            Err("references unknown tool".into())
        } else {
            Ok(())
        }
    }
}

/// An in-memory stand-in for the host's `ConfigServiceDraftStore`: `put` overwrites by
/// id, `get` reads back. Lets a test assert exactly what was persisted (unpublished).
/// The `resources` map stands in for the SEPARATE data-plane resource store so a test
/// can assert a binding was authored alongside the config.
#[derive(Default)]
struct MemDraftStore {
    configs: Mutex<HashMap<String, AgentConfig>>,
    resources: Mutex<HashMap<String, Vec<ResourceSpec>>>,
    audits: Mutex<HashMap<String, (AdminAuditEvent, bool)>>,
}

#[async_trait]
impl DraftStore for MemDraftStore {
    async fn put(&self, draft: &AgentConfig) -> Result<(), String> {
        self.configs
            .lock()
            .unwrap()
            .insert(draft.id.clone(), draft.clone());
        Ok(())
    }
    async fn put_audited(
        &self,
        draft: &AgentConfig,
        audit: &AdminAuditEvent,
    ) -> Result<(), String> {
        let audit_key = format!("{}:{}", audit.tool, audit.call_id);
        if self
            .audits
            .lock()
            .unwrap()
            .get(&audit_key)
            .is_some_and(|(_, committed)| *committed)
        {
            return Ok(());
        }
        self.put(draft).await?;
        self.audits
            .lock()
            .unwrap()
            .insert(audit_key, (audit.clone(), true));
        Ok(())
    }
    async fn record_audit(&self, audit: &AdminAuditEvent) -> Result<(), String> {
        let audit_key = format!("{}:{}", audit.tool, audit.call_id);
        let mut audits = self.audits.lock().unwrap();
        match audits.get(&audit_key) {
            Some((existing, _)) if existing != audit => Err("conflicting audit id".into()),
            Some(_) => Ok(()),
            None => {
                audits.insert(audit_key, (audit.clone(), false));
                Ok(())
            }
        }
    }
    async fn get(&self, id: &str) -> Result<Option<AgentConfig>, String> {
        Ok(self.configs.lock().unwrap().get(id).cloned())
    }
    async fn put_resources(
        &self,
        agent_id: &str,
        resources: Vec<ResourceSpec>,
    ) -> Result<(), String> {
        self.resources
            .lock()
            .unwrap()
            .insert(agent_id.to_string(), resources);
        Ok(())
    }
    async fn get_resources(&self, agent_id: &str) -> Result<Vec<ResourceSpec>, String> {
        Ok(self
            .resources
            .lock()
            .unwrap()
            .get(agent_id)
            .cloned()
            .unwrap_or_default())
    }
}

impl MemDraftStore {
    fn stored(&self, id: &str) -> Option<AgentConfig> {
        self.configs.lock().unwrap().get(id).cloned()
    }
    fn is_empty(&self) -> bool {
        self.configs.lock().unwrap().is_empty()
    }
    fn stored_resources(&self, id: &str) -> Vec<ResourceSpec> {
        self.resources
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
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
            .all(|d| d.content_hash.starts_with("admin:"))
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
    let caps: PlatformCapabilities = serde_json::from_str(&out.content).unwrap();
    assert_eq!(caps.models, vec!["m-1", "m-2"]);
    assert_eq!(caps.plugins[0].id, "state_machine");
    // No secret-bearing field exists on the view — it is redacted by construction.
    assert!(!out.content.contains("api_key") && !out.content.contains("credential"));
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
    assert!(!out.is_error, "{}", out.content);
    let returned = parse_saved(&out.content);
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
                    { "target": "read", "alias": "peek", "description": "look", "defer": true }
                ]
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    // The persisted config carries the override (previously impossible to set at all).
    let stored = h.store.stored("authoring").expect("persisted");
    assert_eq!(stored.tool_overrides.len(), 1);
    let ov = &stored.tool_overrides[0];
    assert_eq!(ov.target, "read");
    assert_eq!(ov.alias.as_deref(), Some("peek"));
    assert!(ov.defer);
    // And the returned envelope reflects it too.
    let returned = parse_saved(&out.content);
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
    assert!(!out.is_error, "{}", out.content);
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
                "mcp_servers": [{ "id": "github" }],
                "skills": [{ "id": "greet" }],
                "multiagent": { "workers": ["a", "b"] },
                "metadata": { "team": "platform", "tier": "gold" }
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    let stored = h.store.stored("full").unwrap();
    assert_eq!(
        stored.mcp_servers,
        vec![serde_json::json!({ "id": "github" })]
    );
    assert_eq!(stored.skills, vec![serde_json::json!({ "id": "greet" })]);
    assert_eq!(
        stored.multiagent,
        Some(serde_json::json!({ "workers": ["a", "b"] }))
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
                        { "kind": "github_repository", "resource_id": "repo_1", "access": "read_only" }
                    ]
                }
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    let bound = h.store.stored_resources("r");
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].kind, "github_repository");
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
    assert!(!out.is_error, "{}", out.content);
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
    assert!(!out.is_error, "{}", out.content);
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
    assert!(out.content.contains("over the"));
    assert!(
        h.store.stored("toobig").is_none(),
        "oversized not persisted"
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
    assert!(out.content.contains("does not validate"));
    assert!(out.content.contains("unknown tool"));
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
    assert!(out.content.contains("invalid arguments"));
    assert!(h.store.is_empty());
}

#[tokio::test]
async fn draft_agent_defaults_max_steps_to_eight() {
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
    assert_eq!(stored.max_steps, 8);
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
async fn patch_agent_reads_merges_and_persists() {
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

    // Patch: change max_steps and ADD a permission section (merge by key).
    let out = h
        .tool(PATCH_TOOL)
        .invoke(call(
            PATCH_TOOL,
            serde_json::json!({
                "id": "support",
                "patch": {
                    "max_steps": 9,
                    "plugin_config": { "permission": { "allow": ["read"] } }
                }
            }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);

    let stored = h.store.stored("support").unwrap();
    // Patched field applied.
    assert_eq!(stored.max_steps, 9);
    // Untouched fields preserved.
    assert_eq!(stored.tool_ids, vec!["read".to_string()]);
    // plugin_config merged by key: BOTH the old compact and the new permission survive.
    assert_eq!(
        stored.plugin_config.get("compact"),
        Some(&serde_json::json!({ "keep": 10 }))
    );
    assert_eq!(
        stored.plugin_config.get("permission"),
        Some(&serde_json::json!({ "allow": ["read"] }))
    );
    // plugin_ids re-derived from the merged map (sorted BTreeMap order).
    assert_eq!(
        stored.plugin_ids,
        vec!["compact".to_string(), "permission".to_string()]
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
    assert!(out.content.contains("no saved draft"));
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
    assert!(out.content.contains("does not validate"));
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
        serde_json::from_str::<serde_json::Value>(&out.content).unwrap()["valid"],
        true
    );
}

#[tokio::test]
async fn validate_reports_invalid_for_a_saved_draft_with_an_unknown_tool() {
    let h = Harness::new();
    // Persist a draft directly so we can validate a config the draft tool would reject.
    h.store
        .put(&AgentConfig {
            id: "a".into(),
            instructions: "hi".into(),
            max_steps: 8,
            delegation_limits: Default::default(),
            model_binding: ModelSelection::Auto,
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
    let parsed: serde_json::Value = serde_json::from_str(&out.content).unwrap();
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
    assert!(out.content.contains("no saved draft"));
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
    assert!(out.content.contains("invalid arguments"));
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
    let author = Arc::new(FakeEnvAuthor::default());
    let tool = DraftEnvironment {
        author: author.clone(),
        store: Arc::new(MemDraftStore::default()),
        audit: Arc::new(CapturingAudit::default()),
    };
    let out = tool
        .invoke(ToolCall {
            call_id: "c1".into(),
            tool_id: CREATE_ENV_TOOL.into(),
            arguments: serde_json::json!({
                "name": "claude-box",
                "runtime": "acp:claude",
                "placement": "self_hosted",
                "sandbox": { "isolation": "namespace", "network": { "mode": "none" } }
            }),
        })
        .await
        .unwrap();
    assert!(!out.is_error, "created ok: {out:?}");
    let (name, config) = author.last.lock().unwrap().clone().expect("authored");
    assert_eq!(name, "claude-box");
    assert_eq!(config["type"], "self_hosted");
    assert_eq!(config["runtime"], "acp:claude");
    assert_eq!(config["sandbox"]["network"]["mode"], "none");
    // The native `awaken` runtime is the default → omitted from the config.
    let out2 = tool
        .invoke(ToolCall {
            call_id: "c2".into(),
            tool_id: CREATE_ENV_TOOL.into(),
            arguments: serde_json::json!({ "name": "native", "runtime": "awaken" }),
        })
        .await
        .unwrap();
    assert!(!out2.is_error);
    let (_, config2) = author.last.lock().unwrap().clone().unwrap();
    assert!(
        config2.get("runtime").is_none(),
        "native runtime is omitted"
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
impl DraftValidator for ScopeFenceValidator {
    fn validate(&self, draft: &AgentConfig) -> Result<(), String> {
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
    assert!(out.content.contains("does not validate"));
    assert!(out.content.contains(CAPABILITIES_TOOL));
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
    assert!(!out.is_error, "{}", out.content);
    // The tool projects the pure `explain` payload verbatim (the full 5 sections).
    let payload: serde_json::Value = serde_json::from_str(&out.content).unwrap();
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
    assert!(!out.is_error, "{}", out.content);
    let payload: serde_json::Value = serde_json::from_str(&out.content).unwrap();
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
    assert!(!out.is_error, "{}", out.content);
    let payload: serde_json::Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(payload["unknown_topic"], "does-not-exist");
    assert!(payload["topics"].as_array().is_some_and(|a| !a.is_empty()));
    assert!(h.audit.0.lock().unwrap().is_empty());
}

// ===================================================================================
// Task 3: store.put / put_resources error branches. A DraftStore whose write ops fail
// lets us assert the error SURFACES to the caller (it is not swallowed / fail-open).
// ===================================================================================

/// A DraftStore test double that can be told to fail `put` and/or `put_resources`,
/// delegating everything else to an in-memory `MemDraftStore`. Lets a test drive the
/// store-write error branches in `validate_persist_emit` / `persist_resources_after`.
#[derive(Default)]
struct FaultyStore {
    fail_put: bool,
    fail_put_resources: bool,
    fail_audit: bool,
    inner: MemDraftStore,
}

#[async_trait]
impl DraftStore for FaultyStore {
    async fn put(&self, draft: &AgentConfig) -> Result<(), String> {
        if self.fail_put {
            return Err("disk full".into());
        }
        self.inner.put(draft).await
    }
    async fn put_audited(
        &self,
        draft: &AgentConfig,
        audit: &AdminAuditEvent,
    ) -> Result<(), String> {
        if self.fail_put {
            return Err("disk full".into());
        }
        self.inner.put_audited(draft, audit).await
    }
    async fn record_audit(&self, audit: &AdminAuditEvent) -> Result<(), String> {
        if self.fail_audit {
            return Err("audit store offline".into());
        }
        self.inner.record_audit(audit).await
    }
    async fn get(&self, id: &str) -> Result<Option<AgentConfig>, String> {
        self.inner.get(id).await
    }
    async fn put_resources(
        &self,
        agent_id: &str,
        resources: Vec<ResourceSpec>,
    ) -> Result<(), String> {
        if self.fail_put_resources {
            return Err("resource store offline".into());
        }
        self.inner.put_resources(agent_id, resources).await
    }
    async fn get_resources(&self, agent_id: &str) -> Result<Vec<ResourceSpec>, String> {
        self.inner.get_resources(agent_id).await
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
        out.content
    );
    assert!(out.content.contains("validated but could not be saved"));
    assert!(out.content.contains("disk full"));
}

// Task 3b: on the PATCH path, a `store.put` failure on the re-save likewise surfaces.
#[tokio::test]
async fn patch_agent_surfaces_a_store_put_failure() {
    // Seed a valid draft into a plain store, then move it into a put-faulty store so the
    // patch can read it back but fail on the re-save.
    let seed = MemDraftStore::default();
    seed.put(&AgentConfig {
        id: "s".into(),
        instructions: "hi".into(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::Auto,
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
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("validated but could not be saved"));
    assert!(out.content.contains("disk full"));
}

// Task 3c: `persist_resources_after` — the config saves, but binding the SEPARATE
// data-plane resources fails. The error surfaces ("draft saved but its resources could
// not be bound: <err>"); it is not swallowed. NOTE (characterization, not a bug): the
// config IS left persisted while the resource bind failed — a partial write the caller
// is told about via the error body.
#[tokio::test]
async fn draft_agent_surfaces_a_resource_bind_failure_after_saving_the_config() {
    let store = Arc::new(FaultyStore {
        fail_put_resources: true,
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
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("resources could not be bound"));
    assert!(out.content.contains("resource store offline"));
    // Characterization: the CONFIG was still saved (partial write) even though the
    // resource bind failed — the config store shows the draft.
    assert!(
        store.inner.stored("r").is_some(),
        "the config is persisted even though the resource bind failed"
    );
    assert!(store.inner.stored_resources("r").is_empty());
}

// Task 3d: patch path — a present `resources` array triggers `put_resources`; its
// failure surfaces to the caller the same way.
#[tokio::test]
async fn patch_agent_surfaces_a_resource_bind_failure() {
    // Seed a draft with no resources, then arm the put_resources fault.
    let seed = MemDraftStore::default();
    seed.put(&AgentConfig {
        id: "r".into(),
        instructions: "hi".into(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::Auto,
        ..Default::default()
    })
    .await
    .unwrap();
    let store = Arc::new(FaultyStore {
        fail_put_resources: true,
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
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("resources could not be bound"));
    assert!(out.content.contains("resource store offline"));
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
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("over the"));
    assert!(out.content.contains("state_machine"));
    // The oversized patch did NOT overwrite the stored draft (fail-closed).
    assert_eq!(
        h.store.stored("p").unwrap(),
        before,
        "an oversized patch must not overwrite the draft"
    );
}
