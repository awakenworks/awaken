//! The bare-`Runtime` assembly recipe for memory + skills together — no host
//! layer. This is the reference an embedder follows: everything wired here is a
//! public export of `awaken-ext-memory` / `awaken-ext-skills`, so the host's
//! private wiring never needs to be copied. It exercises the full combined
//! surface in one run: bounded recall injected before inference (G13,
//! request-only), skill discovery/conditional surfacing/inline and fork
//! activation, `write_memory` persisting to the store directory, and the two
//! extensions coexisting on one runtime.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use awaken_ext_builtin_tools::erase;
use awaken_ext_memory::{
    MEMORY_PLUGIN_ID, MemoryDir, MemoryPlugin, RecallBounds, WriteMemoryTool,
    write_memory_descriptor,
};
use awaken_ext_skills::{
    InMemorySkillRegistry, ListSkillsTool, PathActivations, RecordingGate, SKILL_LIST_TOOL_ID,
    SKILL_TOOL_ID, SkillContext, SkillSpec, SkillTool, list_skills_tool_descriptor,
    skill_tool_descriptor,
};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::subagent_runner::{
    SubagentError, SubagentReply, SubagentRequest, SubagentRunner,
};
use awaken_runtime_examples::prelude::*;

/// A deterministic model that drives the whole combined surface: discover →
/// touch a rust file → discover again (the conditional skill surfaces) →
/// activate inline → activate a fork skill → save a memory → end. It records
/// every request so the test can assert what the model actually saw (recall is
/// request-only context and never committed, G13).
#[derive(Default)]
struct RecipeLlm {
    calls: AtomicUsize,
    seen: Mutex<Vec<ChatRequest>>,
}

fn tool_call(step: usize, tool_id: &str, arguments: serde_json::Value) -> AssistantOutput {
    AssistantOutput::from_tool_calls(vec![ToolCall {
        call_id: format!("call-{step}"),
        tool_id: tool_id.to_string(),
        arguments,
    }])
}

#[async_trait]
impl LlmExecutor for RecipeLlm {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.seen.lock().unwrap().push(request);
        let step = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = match step {
            0 => tool_call(step, SKILL_LIST_TOOL_ID, serde_json::json!({})),
            1 => tool_call(
                step,
                "echo",
                serde_json::json!({ "text": "hi", "path": "src/app/main.rs" }),
            ),
            2 => tool_call(step, SKILL_LIST_TOOL_ID, serde_json::json!({})),
            3 => tool_call(
                step,
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "commit", "args": "-m fix" }),
            ),
            4 => tool_call(
                step,
                SKILL_TOOL_ID,
                serde_json::json!({ "skill": "review", "args": "PR-7" }),
            ),
            5 => tool_call(
                step,
                "write_memory",
                serde_json::json!({ "name": "pref", "content": "the user likes tea" }),
            ),
            _ => AssistantOutput::text("All done.".to_string()),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// The embedder's own fork substrate — the one piece the recipe cannot supply,
/// because how a sub-agent is spawned belongs to the composition root.
struct EchoFork;

#[async_trait]
impl SubagentRunner for EchoFork {
    async fn run(&self, request: SubagentRequest) -> Result<SubagentReply, SubagentError> {
        let prompt = request
            .seed
            .first()
            .map(|m| m.text_content())
            .unwrap_or_default();
        Ok(SubagentReply {
            text: Some(format!("forked[{}]: {prompt}", request.agent_id)),
        })
    }
}

fn skill_registry() -> std::sync::Arc<InMemorySkillRegistry> {
    std::sync::Arc::new(InMemorySkillRegistry::from_specs([
        SkillSpec::new(
            "commit",
            "Commit",
            "Make a git commit",
            "Use single-line commit messages.",
        ),
        SkillSpec::new("rusty", "Rusty", "For rust files", "rust guidance")
            .with_paths(vec!["src/**/*.rs".into()]),
        SkillSpec::new(
            "review",
            "Review",
            "Review a PR",
            "review $ARGUMENTS carefully",
        )
        .with_context(SkillContext::Fork),
    ]))
}

fn allow_all_gate() -> std::sync::Arc<PermissionGate> {
    std::sync::Arc::new(PermissionGate::new(std::sync::Arc::new(
        RulePermissionPolicy::new(PermissionRuleset {
            default_behavior: ToolPermissionBehavior::Allow,
            mode: Mode::Default,
            rules: Vec::new(),
        }),
    )))
}

fn memory_dir(label: &str) -> MemoryDir {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    MemoryDir::new(std::env::temp_dir().join(format!("awaken-combo-{label}-{stamp}")))
}

fn combo_config(with_memory_plugin: bool) -> RunnableConfig {
    let mut builder = RunnableConfig::builder("combo")
        .model(ModelBinding::new("demo", "stub", "stub"))
        .tool(ToolDescriptor::pinned(
            "demo",
            "echo",
            "Echo",
            serde_json::json!({"type": "object"}),
        ))
        .tool(list_skills_tool_descriptor())
        .tool(skill_tool_descriptor())
        .tool(write_memory_descriptor())
        .max_steps(12);
    if with_memory_plugin {
        builder = builder.plugins([MEMORY_PLUGIN_ID.to_string()]);
    }
    builder.build()
}

/// Everything the model saw across one request, flattened to text.
fn request_text(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn bare_runtime_assembles_memory_and_skills_from_public_parts() {
    let memory = memory_dir("full");
    memory
        .write("tea", "the user drinks oolong")
        .expect("seed memory");

    let registry = skill_registry();
    let activations = PathActivations::new();
    let llm = std::sync::Arc::new(RecipeLlm::default());

    let runtime = Runtime::new()
        .with_llm(llm.clone())
        .with_tool(std::sync::Arc::new(EchoTool))
        .with_tool(std::sync::Arc::new(
            ListSkillsTool::new(registry.clone()).with_path_activations(activations.clone()),
        ))
        .with_tool(std::sync::Arc::new(
            SkillTool::new(registry.clone())
                .with_session_id("sess-1")
                .with_fork_runner(std::sync::Arc::new(EchoFork)),
        ))
        .with_tool(erase(WriteMemoryTool::new(memory.clone())))
        .with_gate(std::sync::Arc::new(RecordingGate::new(
            allow_all_gate(),
            activations.clone(),
        )))
        .with_plugin(std::sync::Arc::new(MemoryPlugin::new(
            memory.clone(),
            RecallBounds::default(),
        )));

    let commit = std::sync::Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .run(&combo_config(true), "Set things up.", ctx)
        .await
        .expect("run");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    // ① Recall reached the model as request-only context (G13): the seeded
    // memory is in what the model saw, but never in the committed thread.
    let seen = llm.seen.lock().unwrap();
    assert!(
        request_text(&seen[0]).contains("the user drinks oolong"),
        "recall injected before the first inference"
    );
    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .all(|m| !m.text_content().contains("drinks oolong")),
        "recall context is never committed"
    );

    // ② Conditional surfacing: the first catalog hides the `paths` skill; after
    // the gate observed `src/app/main.rs`, the second catalog surfaces it.
    let catalogs: Vec<String> = committed
        .messages
        .iter()
        .map(|m| m.text_content())
        .filter(|t| t.contains("\"hint\""))
        .collect();
    assert_eq!(catalogs.len(), 2, "two list_skills results committed");
    assert!(catalogs[0].contains("commit") && !catalogs[0].contains("rusty"));
    assert!(catalogs[1].contains("rusty"), "surfaced after path touch");
    assert!(
        activations
            .touched()
            .contains(&"src/app/main.rs".to_string()),
        "the recording gate observed the touched path"
    );

    // ③ Inline activation returns the body; fork activation ran through the
    // embedder's runner.
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.text_content().contains("Skill: Commit")
                && m.text_content().contains("single-line commit messages")),
        "inline skill activated"
    );
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.text_content() == "forked[review]: review PR-7 carefully"),
        "fork skill ran through the SubAgentRunner"
    );

    // ④ write_memory persisted into the same store recall reads from.
    let entries = memory.entries();
    assert!(
        entries
            .iter()
            .any(|e| e.content.contains("the user likes tea")),
        "write_memory landed in the store"
    );
}

/// G30: an installed plugin is inert for a run whose config does not select its
/// id — same assembly, no `plugins(["memory"])`, no recall.
#[tokio::test]
async fn memory_plugin_is_inert_without_its_plugin_id() {
    let memory = memory_dir("inert");
    memory
        .write("tea", "the user drinks oolong")
        .expect("seed memory");

    let llm = std::sync::Arc::new(RecipeLlm::default());
    let runtime = Runtime::new()
        .with_llm(llm.clone())
        .with_tool(std::sync::Arc::new(EchoTool))
        .with_tool(std::sync::Arc::new(ListSkillsTool::new(skill_registry())))
        .with_tool(std::sync::Arc::new(SkillTool::new(skill_registry())))
        .with_tool(erase(WriteMemoryTool::new(memory.clone())))
        .with_gate(allow_all_gate())
        .with_plugin(std::sync::Arc::new(MemoryPlugin::new(
            memory.clone(),
            RecallBounds::default(),
        )));

    let ctx =
        RuntimeRunContext::new().with_commit(std::sync::Arc::new(MemoryCommitCoordinator::new()));
    let state = runtime
        .run(&combo_config(false), "Set things up.", ctx)
        .await
        .expect("run");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));

    let seen = llm.seen.lock().unwrap();
    assert!(
        seen.iter()
            .all(|req| !request_text(req).contains("drinks oolong")),
        "an unselected plugin contributes nothing (G30)"
    );
}
