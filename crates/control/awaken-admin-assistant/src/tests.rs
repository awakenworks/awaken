//! Unit tests for the four management tools. The `CapabilityReader` /
//! `DraftValidator` ports are exercised with in-crate test doubles; the real
//! adapters (over the shared catalog and `ConfigService`) are wired and tested in
//! the host.

use super::*;

fn call(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        tool_id: id.into(),
        arguments: args,
    }
}

struct FakeCaps;
impl CapabilityReader for FakeCaps {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            agents: vec!["assistant".into()],
            models: vec!["m-1".into(), "m-2".into()],
            providers: vec!["openai".into()],
            tools: vec!["read".into(), "glob".into()],
            plugins: vec![PluginInfo {
                id: "state_machine".into(),
                schema_keys: vec!["state_machine".into()],
            }],
            skills: vec!["greet".into()],
            mcp_servers: vec![],
        }
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

/// Captures audit records so a test can assert every privileged call is recorded.
#[derive(Default)]
struct CapturingAudit(std::sync::Mutex<Vec<AdminAuditEvent>>);
impl AuditSink for CapturingAudit {
    fn record(&self, event: AdminAuditEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn tools() -> Vec<Arc<dyn RawTool>> {
    admin_tools(
        Arc::new(FakeCaps),
        Arc::new(FakeValidator),
        Arc::new(CapturingAudit::default()),
    )
}

fn tool(id: &str) -> Arc<dyn RawTool> {
    tools()
        .into_iter()
        .find(|t| t.id() == id)
        .expect("tool exists")
}

#[test]
fn descriptors_are_the_four_admin_tools_and_carry_no_publish_tool() {
    let ids: Vec<String> = admin_tool_descriptors()
        .iter()
        .map(|d| d.id.clone())
        .collect();
    assert_eq!(
        ids,
        vec![
            CAPABILITIES_TOOL,
            CREATE_DRAFT_TOOL,
            SET_PLUGIN_TOOL,
            VALIDATE_TOOL
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
    // The executable set matches the advertised descriptor set one-to-one.
    let exec_ids: Vec<String> = tools().iter().map(|t| t.id().to_string()).collect();
    assert_eq!(exec_ids, ids);
}

#[tokio::test]
async fn capabilities_returns_the_redacted_org_shared_view() {
    let out = tool(CAPABILITIES_TOOL)
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
async fn create_draft_auto_binds_the_model_and_never_publishes() {
    let out = tool(CREATE_DRAFT_TOOL)
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
    assert!(!out.is_error);
    let draft: AgentConfig = serde_json::from_str(&out.content).unwrap();
    assert_eq!(draft.id, "support");
    assert_eq!(draft.max_steps, 5);
    assert_eq!(draft.tool_ids, vec!["read".to_string()]);
    // The draft is auto-bound (D5) — the operator never hand-picks a model.
    assert!(draft.model_binding.is_auto());
    // A draft is data only: the wire carries {"mode":"auto"} for the binding.
    assert!(out.content.contains("\"mode\":\"auto\""));
}

#[tokio::test]
async fn create_draft_rejects_bad_arguments_without_aborting() {
    let out = tool(CREATE_DRAFT_TOOL)
        .invoke(call(CREATE_DRAFT_TOOL, serde_json::json!({ "id": "x" })))
        .await
        .unwrap();
    // Missing `instructions` is a model-visible error, not a hard abort.
    assert!(out.is_error);
    assert!(out.content.contains("invalid arguments"));
}

#[tokio::test]
async fn set_plugin_config_attaches_and_validates() {
    let draft = serde_json::json!({
        "id": "support",
        "instructions": "be helpful",
        "max_steps": 8,
        "model_binding": { "mode": "auto" },
        "tool_ids": []
    });
    let out = tool(SET_PLUGIN_TOOL)
        .invoke(call(
            SET_PLUGIN_TOOL,
            serde_json::json!({
                "draft": draft,
                "plugin_id": "state_machine",
                "config": { "machines": [] }
            }),
        ))
        .await
        .unwrap();
    assert!(
        !out.is_error,
        "attaching a valid section succeeds: {}",
        out.content
    );
    let updated: AgentConfig = serde_json::from_str(&out.content).unwrap();
    assert_eq!(updated.plugin_ids, vec!["state_machine".to_string()]);
    assert_eq!(
        updated.plugin_config.get("state_machine"),
        Some(&serde_json::json!({ "machines": [] }))
    );
}

#[tokio::test]
async fn set_plugin_config_size_bounds_the_section() {
    let draft = serde_json::json!({
        "id": "support", "instructions": "be helpful", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": []
    });
    let big = "x".repeat(MAX_PLUGIN_CONFIG_BYTES + 1);
    let out = tool(SET_PLUGIN_TOOL)
        .invoke(call(
            SET_PLUGIN_TOOL,
            serde_json::json!({
                "draft": draft,
                "plugin_id": "state_machine",
                "config": { "blob": big }
            }),
        ))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("over the"));
}

#[tokio::test]
async fn validate_agent_reports_valid_and_invalid() {
    let ok_draft = serde_json::json!({
        "id": "a", "instructions": "hi", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": ["read"]
    });
    let ok = tool(VALIDATE_TOOL)
        .invoke(call(
            VALIDATE_TOOL,
            serde_json::json!({ "draft": ok_draft }),
        ))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ok.content).unwrap()["valid"],
        true
    );

    let bad_draft = serde_json::json!({
        "id": "a", "instructions": "hi", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": ["ghost_tool"]
    });
    let bad = tool(VALIDATE_TOOL)
        .invoke(call(
            VALIDATE_TOOL,
            serde_json::json!({ "draft": bad_draft }),
        ))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&bad.content).unwrap();
    assert_eq!(parsed["valid"], false);
    assert!(parsed["error"].as_str().unwrap().contains("unknown tool"));
}

#[tokio::test]
async fn every_tool_call_emits_an_audit_record() {
    let audit = Arc::new(CapturingAudit::default());
    let toolset = admin_tools(Arc::new(FakeCaps), Arc::new(FakeValidator), audit.clone());
    let get = |id: &str| toolset.iter().find(|t| t.id() == id).unwrap().clone();

    get(CAPABILITIES_TOOL)
        .invoke(call(CAPABILITIES_TOOL, serde_json::json!({})))
        .await
        .unwrap();
    get(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "x", "instructions": "hi" }),
        ))
        .await
        .unwrap();

    let events = audit.0.lock().unwrap();
    // Every privileged call is recorded (ADR-0052 D6), tagged with its tool id.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].tool, CAPABILITIES_TOOL);
    assert_eq!(events[1].tool, CREATE_DRAFT_TOOL);
    assert!(events[1].summary.contains("draft agent `x`"));
}

#[test]
fn seed_config_is_an_ordinary_auto_bound_config_naming_the_four_tools() {
    let cfg = admin_assistant_config();
    assert_eq!(cfg.id, ADMIN_ASSISTANT_AGENT_ID);
    // Auto-bound (D5), names exactly the four admin tools (D3), no plugins, no
    // sandbox concept — an ordinary AgentConfig (D1/D4).
    assert!(cfg.model_binding.is_auto());
    assert_eq!(
        cfg.tool_ids,
        vec![
            CAPABILITIES_TOOL,
            CREATE_DRAFT_TOOL,
            SET_PLUGIN_TOOL,
            VALIDATE_TOOL
        ]
    );
    assert!(cfg.instructions.contains("management assistant"));
}

#[test]
fn seeded_instructions_are_authorable_and_mention_no_publish() {
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("management assistant"));
    assert!(ADMIN_ASSISTANT_INSTRUCTIONS.contains("never publish"));
}

// ---- CEG spec 08: additional cases ------------------------------------------------

/// Build the admin toolset over a specific audit sink so a test can inspect what was
/// recorded, and fetch one tool by id.
fn tools_with_audit(audit: Arc<CapturingAudit>) -> Vec<Arc<dyn RawTool>> {
    admin_tools(Arc::new(FakeCaps), Arc::new(FakeValidator), audit)
}

fn pick(set: &[Arc<dyn RawTool>], id: &str) -> Arc<dyn RawTool> {
    set.iter()
        .find(|t| t.id() == id)
        .expect("tool exists")
        .clone()
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

// PL1 — SetPluginConfig: invalid JSON args → soft error, and NOT audited (parse-fail
// short-circuits before the audit seam).
#[tokio::test]
async fn set_plugin_config_bad_arguments_error_without_audit() {
    let audit = Arc::new(CapturingAudit::default());
    let set = tools_with_audit(audit.clone());
    let out = pick(&set, SET_PLUGIN_TOOL)
        // Missing `plugin_id`/`config`, and `draft` the wrong shape.
        .invoke(call(SET_PLUGIN_TOOL, serde_json::json!({ "draft": 7 })))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("invalid arguments"));
    // Parse failure is a soft ToolOutput::error that short-circuits before audit.
    assert!(audit.0.lock().unwrap().is_empty());
}

// PL2 — SetPluginConfig: section over 64 KiB → error, and IS audited (audit precedes
// the size check). Error path covered by `set_plugin_config_size_bounds_the_section`;
// this asserts the audit-ordering half.
#[tokio::test]
async fn set_plugin_config_over_limit_is_audited() {
    let audit = Arc::new(CapturingAudit::default());
    let set = tools_with_audit(audit.clone());
    let draft = serde_json::json!({
        "id": "support", "instructions": "be helpful", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": []
    });
    let big = "x".repeat(MAX_PLUGIN_CONFIG_BYTES + 1);
    let out = pick(&set, SET_PLUGIN_TOOL)
        .invoke(call(
            SET_PLUGIN_TOOL,
            serde_json::json!({
                "draft": draft,
                "plugin_id": "state_machine",
                "config": { "blob": big }
            }),
        ))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("over the"));
    // A well-formed-but-oversized call is a privileged call: it is audited.
    let events = audit.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tool, SET_PLUGIN_TOOL);
}

// PL3 — SetPluginConfig: attaching leaves a draft that does not validate → error.
#[tokio::test]
async fn set_plugin_config_reports_validation_failure_after_attach() {
    let draft = serde_json::json!({
        "id": "support", "instructions": "be helpful", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": ["ghost_tool"]
    });
    let out = tool(SET_PLUGIN_TOOL)
        .invoke(call(
            SET_PLUGIN_TOOL,
            serde_json::json!({
                "draft": draft,
                "plugin_id": "state_machine",
                "config": { "machines": [] }
            }),
        ))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("does not validate"));
    assert!(out.content.contains("unknown tool"));
}

// existing: PL4 legit attach → emit_draft — `set_plugin_config_attaches_and_validates`.

// CreateAgentDraft (a) — parse failure is a soft error AND is not audited.
// (existing `create_draft_rejects_bad_arguments_without_aborting` covers the soft error;
//  this adds the not-audited invariant.)
#[tokio::test]
async fn create_draft_parse_failure_is_not_audited() {
    let audit = Arc::new(CapturingAudit::default());
    let set = tools_with_audit(audit.clone());
    let out = pick(&set, CREATE_DRAFT_TOOL)
        .invoke(call(CREATE_DRAFT_TOOL, serde_json::json!({ "id": "x" })))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(audit.0.lock().unwrap().is_empty());
}

// CreateAgentDraft (c) — missing max_steps defaults to 8.
#[tokio::test]
async fn create_draft_defaults_max_steps_to_eight() {
    let out = tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "support", "instructions": "be helpful" }),
        ))
        .await
        .unwrap();
    assert!(!out.is_error);
    let draft: AgentConfig = serde_json::from_str(&out.content).unwrap();
    assert_eq!(draft.max_steps, 8);
    assert!(draft.model_binding.is_auto());
}

// ValidateAgent (a) — parse failure → soft error.
#[tokio::test]
async fn validate_agent_rejects_bad_arguments() {
    let out = tool(VALIDATE_TOOL)
        .invoke(call(
            VALIDATE_TOOL,
            serde_json::json!({ "draft": "not-a-config" }),
        ))
        .await
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("invalid arguments"));
}

// ValidateAgent (b)/(c) — both a passing and a failing validation return
// ToolOutput::ok (soft): the {valid:bool} verdict is data, not a tool error.
// (existing `validate_agent_reports_valid_and_invalid` asserts the verdict bodies;
//  this asserts the is_error==false half for both outcomes.)
#[tokio::test]
async fn validate_agent_is_soft_ok_for_both_verdicts() {
    let ok_draft = serde_json::json!({
        "id": "a", "instructions": "hi", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": ["read"]
    });
    let ok = tool(VALIDATE_TOOL)
        .invoke(call(
            VALIDATE_TOOL,
            serde_json::json!({ "draft": ok_draft }),
        ))
        .await
        .unwrap();
    assert!(!ok.is_error);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&ok.content).unwrap()["valid"],
        true
    );

    let bad_draft = serde_json::json!({
        "id": "a", "instructions": "hi", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": ["ghost_tool"]
    });
    let bad = tool(VALIDATE_TOOL)
        .invoke(call(
            VALIDATE_TOOL,
            serde_json::json!({ "draft": bad_draft }),
        ))
        .await
        .unwrap();
    // A failed validation is still a successful (soft) tool call — verdict is the body.
    assert!(!bad.is_error);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&bad.content).unwrap()["valid"],
        false
    );
}

// AA-a — a draft that names an admin tool in a non-reserved scope is correctly
// rejected by validation (the scope fence surfaces UnknownTool through ValidateAgent).
#[tokio::test]
async fn validate_agent_rejects_draft_naming_an_admin_tool() {
    let set = admin_tools(
        Arc::new(FakeCaps),
        Arc::new(ScopeFenceValidator),
        Arc::new(CapturingAudit::default()),
    );
    let draft = serde_json::json!({
        "id": "sneaky", "instructions": "hi", "max_steps": 8,
        "model_binding": { "mode": "auto" }, "tool_ids": [CAPABILITIES_TOOL]
    });
    let out = pick(&set, VALIDATE_TOOL)
        .invoke(call(VALIDATE_TOOL, serde_json::json!({ "draft": draft })))
        .await
        .unwrap();
    // Soft ok, but the verdict is a rejection naming the offending tool.
    assert!(!out.is_error);
    let parsed: serde_json::Value = serde_json::from_str(&out.content).unwrap();
    assert_eq!(parsed["valid"], false);
    let msg = parsed["error"].as_str().unwrap();
    assert!(msg.contains("unknown tool"));
    assert!(msg.contains(CAPABILITIES_TOOL));
}

// AA-b — never-publish invariant: no tool exposes a publish action, and every draft a
// tool emits stays an unpublished, auto-bound source config (never a compiled/pinned
// publication). Publication is a console action, never an LLM tool call (ADR-0052 D4).
#[tokio::test]
async fn no_tool_call_ever_publishes() {
    // 1) The toolset carries no publish tool.
    assert!(tools().iter().all(|t| !t.id().contains("publish")));

    // 2) Draft-emitting tools return an unpublished, auto-bound draft (not a pinned /
    //    resolved publication).
    let created = tool(CREATE_DRAFT_TOOL)
        .invoke(call(
            CREATE_DRAFT_TOOL,
            serde_json::json!({ "id": "support", "instructions": "be helpful" }),
        ))
        .await
        .unwrap();
    let created_draft: AgentConfig = serde_json::from_str(&created.content).unwrap();
    assert!(created_draft.model_binding.is_auto());

    let amended = tool(SET_PLUGIN_TOOL)
        .invoke(call(
            SET_PLUGIN_TOOL,
            serde_json::json!({
                "draft": {
                    "id": "support", "instructions": "be helpful", "max_steps": 8,
                    "model_binding": { "mode": "auto" }, "tool_ids": []
                },
                "plugin_id": "state_machine",
                "config": { "machines": [] }
            }),
        ))
        .await
        .unwrap();
    let amended_draft: AgentConfig = serde_json::from_str(&amended.content).unwrap();
    // Still Auto after amendment: the tool never resolves/pins/publishes the draft.
    assert!(amended_draft.model_binding.is_auto());
}
