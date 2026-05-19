#![allow(missing_docs)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use awaken::contract::config_store::ConfigStore;
use awaken::contract::content::ContentBlock;
use awaken::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken::contract::inference::{StopReason, StreamResult};
use awaken::contract::lifecycle::TerminationReason;
use awaken::contract::message::{Message, ToolCall};
use awaken::ext_skills::{
    ActiveSkillInstructionsPlugin, ConfigSkillRegistry, SkillDiscoveryPlugin, SkillRegistry,
};
use awaken::server::services::config_runtime::{ConfigRuntimeError, ConfigRuntimeManager};
use awaken::{AgentRuntime, AgentRuntimeBuilder, Plugin, RunRequest, SkillSpec, SkillSpecSink};
use awaken_contract::{
    AgentSpec, BuiltinSeedSet, BuiltinSpec, ConfigRecord, ModelBindingSpec, ProviderSpec,
    RecordMeta,
};

struct ScriptedLlm {
    responses: Mutex<Vec<StreamResult>>,
    captured_requests: Mutex<Vec<InferenceRequest>>,
}

impl ScriptedLlm {
    fn new(responses: Vec<StreamResult>) -> Self {
        Self {
            responses: Mutex::new(responses),
            captured_requests: Mutex::new(Vec::new()),
        }
    }

    fn captured_requests(&self) -> Vec<InferenceRequest> {
        self.captured_requests
            .lock()
            .expect("request log lock")
            .clone()
    }
}

#[async_trait]
impl LlmExecutor for ScriptedLlm {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.captured_requests
            .lock()
            .expect("request log lock")
            .push(request);
        let mut responses = self.responses.lock().expect("response lock");
        Ok(responses.remove(0))
    }

    fn name(&self) -> &str {
        "scripted"
    }
}

struct ScriptedProviderFactory {
    executor: Arc<dyn LlmExecutor>,
}

impl awaken::server::services::config_runtime::ProviderExecutorFactory for ScriptedProviderFactory {
    fn build(&self, spec: &ProviderSpec) -> Result<Arc<dyn LlmExecutor>, ConfigRuntimeError> {
        if spec.adapter == "scripted" {
            return Ok(self.executor.clone());
        }
        Err(ConfigRuntimeError::UnsupportedProviderAdapter(
            spec.adapter.clone(),
        ))
    }
}

fn tool_step(calls: Vec<ToolCall>) -> StreamResult {
    StreamResult {
        content: vec![],
        tool_calls: calls,
        usage: None,
        stop_reason: Some(StopReason::ToolUse),
        has_incomplete_tool_calls: false,
    }
}

fn text_step(text: &str) -> StreamResult {
    StreamResult {
        content: vec![ContentBlock::text(text)],
        tool_calls: vec![],
        usage: None,
        stop_reason: Some(StopReason::EndTurn),
        has_incomplete_tool_calls: false,
    }
}

struct DialogHarness {
    config_store: Arc<dyn ConfigStore>,
    managed_skills: Arc<ConfigSkillRegistry>,
    scripted_llm: Arc<ScriptedLlm>,
    runtime: Arc<AgentRuntime>,
    manager: ConfigRuntimeManager,
}

async fn build_dialog_harness(responses: Vec<StreamResult>) -> DialogHarness {
    let store = Arc::new(awaken::stores::InMemoryStore::new());
    let config_store = store.clone() as Arc<dyn ConfigStore>;
    let managed_skills = Arc::new(ConfigSkillRegistry::new());
    let scripted_llm = Arc::new(ScriptedLlm::new(responses));

    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .with_thread_run_store(store.clone())
            .with_plugin(
                "skills-discovery",
                Arc::new(SkillDiscoveryPlugin::new(managed_skills.clone())) as Arc<dyn Plugin>,
            )
            .with_plugin(
                "skills-active-instructions",
                Arc::new(ActiveSkillInstructionsPlugin::new(managed_skills.clone()))
                    as Arc<dyn Plugin>,
            )
            .build()
            .expect("build runtime"),
    );

    let manager = ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
        .expect("config runtime manager")
        .with_provider_factory(Arc::new(ScriptedProviderFactory {
            executor: scripted_llm.clone(),
        }))
        .with_skill_spec_sink(managed_skills.clone() as Arc<dyn SkillSpecSink>);

    manager
        .apply_seed(&BuiltinSeedSet {
            binary_version: "test".into(),
            specs: vec![
                BuiltinSpec::provider(ProviderSpec {
                    id: "scripted".into(),
                    adapter: "scripted".into(),
                    ..Default::default()
                }),
                BuiltinSpec::model(ModelBindingSpec {
                    id: "dialog-model".into(),
                    provider_id: "scripted".into(),
                    upstream_model: "dialog-model".into(),
                }),
                BuiltinSpec::agent(AgentSpec {
                    id: "assistant".into(),
                    model_id: "dialog-model".into(),
                    system_prompt: "Use skills when relevant.".into(),
                    max_rounds: 4,
                    plugin_ids: vec![
                        "skills-discovery".into(),
                        "skills-active-instructions".into(),
                    ],
                    ..Default::default()
                }),
            ],
        })
        .await
        .expect("apply core seed");

    DialogHarness {
        config_store,
        managed_skills,
        scripted_llm,
        runtime,
        manager,
    }
}

async fn upsert_db_skill(
    harness: &DialogHarness,
    description: &str,
    instructions: &str,
    when_to_use: &str,
) {
    let skill_record = ConfigRecord {
        spec: SkillSpec {
            id: "db-management".into(),
            name: "Database Management".into(),
            description: description.into(),
            instructions_md: instructions.into(),
            when_to_use: Some(when_to_use.into()),
            ..Default::default()
        },
        meta: RecordMeta::new_user(),
    };
    harness
        .config_store
        .put(
            "skills",
            "db-management",
            &skill_record.to_value().expect("serialize skill record"),
        )
        .await
        .expect("upsert skill config");
    harness
        .manager
        .apply()
        .await
        .expect("publish managed config");
}

async fn delete_db_skill(harness: &DialogHarness) {
    harness
        .config_store
        .delete("skills", "db-management")
        .await
        .expect("delete skill config");
    harness
        .manager
        .apply()
        .await
        .expect("publish managed config");
}

async fn run_dialog(
    harness: &DialogHarness,
    thread_id: &str,
    prompt: &str,
) -> awaken::loop_runner::AgentRunResult {
    harness
        .runtime
        .run_to_completion(
            RunRequest::new(thread_id, vec![Message::user(prompt)]).with_agent_id("assistant"),
        )
        .await
        .expect("dialog run should succeed")
}

fn request_messages_contain(request: &InferenceRequest, needle: &str) -> bool {
    request
        .messages
        .iter()
        .any(|message| message.text().contains(needle))
}

#[tokio::test]
async fn config_managed_skill_created_in_config_store_is_used_in_dialog() {
    let harness = build_dialog_harness(vec![
        tool_step(vec![ToolCall::new(
            "activate-db",
            "skill",
            json!({"skill": "db-management"}),
        )]),
        text_step("done with database skill"),
    ])
    .await;

    upsert_db_skill(
        &harness,
        "Helps inspect relational database schema",
        "Always inspect schema before writing SQL.",
        "When the user asks about database work",
    )
    .await;
    assert!(
        harness.managed_skills.get("db-management").is_some(),
        "ConfigRuntimeManager should publish the config-store skill into the live registry"
    );

    let result = run_dialog(
        &harness,
        "thread-config-skill-dialog",
        "Help me inspect a database",
    )
    .await;

    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.response, "done with database skill");

    let requests = harness.scripted_llm.captured_requests();
    assert_eq!(
        requests.len(),
        2,
        "skill activation should cause a second inference turn"
    );

    let first = &requests[0];
    assert!(
        first.tools.iter().any(|tool| tool.id == "skill"),
        "skill plugin should register the skill activation tool"
    );
    assert!(
        request_messages_contain(first, "<available_skills>")
            && request_messages_contain(first, "db-management")
            && request_messages_contain(first, "Database Management")
            && request_messages_contain(first, "When the user asks about database work"),
        "first inference should include the DB-managed skill catalog: {:?}",
        first.messages
    );

    let second = &requests[1];
    assert!(
        request_messages_contain(second, "<active_skill_instructions>")
            && request_messages_contain(second, "skill=\"db-management\"")
            && request_messages_contain(second, "Always inspect schema before writing SQL."),
        "second inference should include active instructions from the DB-managed skill: {:?}",
        second.messages
    );
}

#[tokio::test]
async fn config_managed_skill_update_is_used_by_next_dialog() {
    let harness = build_dialog_harness(vec![
        tool_step(vec![ToolCall::new(
            "activate-db-v1",
            "skill",
            json!({"skill": "db-management"}),
        )]),
        text_step("v1 done"),
        tool_step(vec![ToolCall::new(
            "activate-db-v2",
            "skill",
            json!({"skill": "db-management"}),
        )]),
        text_step("v2 done"),
    ])
    .await;

    upsert_db_skill(
        &harness,
        "Original database helper",
        "Use version one database guidance.",
        "When v1 database help is needed",
    )
    .await;
    let v1 = run_dialog(&harness, "thread-config-skill-update-v1", "Use v1 skill").await;
    assert_eq!(v1.response, "v1 done");

    let requests = harness.scripted_llm.captured_requests();
    assert_eq!(requests.len(), 2);
    assert!(request_messages_contain(
        &requests[0],
        "Original database helper"
    ));
    assert!(request_messages_contain(
        &requests[1],
        "Use version one database guidance."
    ));

    upsert_db_skill(
        &harness,
        "Updated database specialist",
        "Use version two database guidance.",
        "When v2 database help is needed",
    )
    .await;
    let v2 = run_dialog(&harness, "thread-config-skill-update-v2", "Use v2 skill").await;
    assert_eq!(v2.response, "v2 done");

    let requests = harness.scripted_llm.captured_requests();
    assert_eq!(requests.len(), 4);
    assert!(
        !request_messages_contain(&requests[2], "<active_skill_instructions>"),
        "a fresh run must not inherit active skills from the previous run: {:?}",
        requests[2].messages
    );
    assert!(request_messages_contain(
        &requests[2],
        "Updated database specialist"
    ));
    assert!(request_messages_contain(
        &requests[2],
        "When v2 database help is needed"
    ));
    assert!(request_messages_contain(
        &requests[3],
        "Use version two database guidance."
    ));
    assert!(
        !request_messages_contain(&requests[3], "Use version one database guidance."),
        "active instructions must come from the latest DB-managed skill spec"
    );
}

#[tokio::test]
async fn config_managed_skill_delete_removes_it_from_next_dialog() {
    let harness = build_dialog_harness(vec![text_step("no skill catalog")]).await;

    upsert_db_skill(
        &harness,
        "Temporary database helper",
        "Temporary database guidance.",
        "When temporary database help is needed",
    )
    .await;
    assert!(harness.managed_skills.get("db-management").is_some());

    delete_db_skill(&harness).await;
    assert!(
        harness.managed_skills.get("db-management").is_none(),
        "deleted DB-managed skill should be removed from the live registry"
    );

    let result = run_dialog(
        &harness,
        "thread-config-skill-delete",
        "Answer without any configured skills",
    )
    .await;
    assert_eq!(result.termination, TerminationReason::NaturalEnd);
    assert_eq!(result.response, "no skill catalog");

    let requests = harness.scripted_llm.captured_requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].tools.iter().any(|tool| tool.id == "skill"),
        "skill activation tool remains registered by the plugin"
    );
    assert!(
        !request_messages_contain(&requests[0], "<available_skills>")
            && !request_messages_contain(&requests[0], "db-management")
            && !request_messages_contain(&requests[0], "Temporary database guidance."),
        "deleted DB-managed skill must not appear in the next dialog prompt: {:?}",
        requests[0].messages
    );
}
