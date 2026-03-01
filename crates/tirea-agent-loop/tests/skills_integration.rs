#![allow(missing_docs)]

use async_trait::async_trait;
use serde_json::json;
use std::fs;
use std::io::Write;
use std::sync::Arc;
use tempfile::TempDir;
use tirea_agent_loop::contracts::runtime::behavior::AgentBehavior;
use tirea_agent_loop::contracts::runtime::tool_call::{Tool, ToolResult};
use tirea_agent_loop::contracts::thread::Thread;
use tirea_agent_loop::contracts::thread::{Message, ToolCall};
use tirea_agent_loop::engine::tool_execution::execute_single_tool_with_scope_and_behavior;
use tirea_extension_permission::{PermissionPlugin, SCOPE_ALLOWED_SKILLS_KEY};
use tirea_extension_skills::{
    FsSkill, InMemorySkillRegistry, LoadSkillResourceTool, Skill, SkillActivateTool, SkillRegistry,
    SkillRuntimePlugin, SkillScriptTool,
};

struct TestToolBehavior {
    permission: PermissionPlugin,
    skills_runtime: SkillRuntimePlugin,
}

impl TestToolBehavior {
    fn new() -> Self {
        Self {
            permission: PermissionPlugin,
            skills_runtime: SkillRuntimePlugin::new(),
        }
    }
}

#[async_trait]
impl AgentBehavior for TestToolBehavior {
    fn id(&self) -> &str {
        "skills_integration_test_router"
    }

    fn behavior_ids(&self) -> Vec<&str> {
        let mut ids = self.permission.behavior_ids();
        ids.extend(self.skills_runtime.behavior_ids());
        ids
    }
}

fn make_skill_tree() -> (TempDir, Arc<dyn SkillRegistry>) {
    let td = TempDir::new().unwrap();
    let skills_root = td.path().join("skills");
    let docx_root = skills_root.join("docx");
    fs::create_dir_all(docx_root.join("references")).unwrap();
    fs::create_dir_all(docx_root.join("assets")).unwrap();
    fs::create_dir_all(docx_root.join("scripts")).unwrap();

    fs::write(
        docx_root.join("references").join("DOCX-JS.md"),
        "Use docx-js for new documents.",
    )
    .unwrap();

    fs::write(docx_root.join("assets").join("logo.txt"), "asset-payload").unwrap();

    let mut f = fs::File::create(docx_root.join("SKILL.md")).unwrap();
    f.write_all(
        b"---\nname: docx\ndescription: DOCX processing guidance\nallowed-tools: read_file\n---\n# DOCX Processing\n\n## Creating documents\n\nUse docx-js for new documents. See [DOCX-JS.md](references/DOCX-JS.md).\n",
    )
    .unwrap();

    fs::write(
        docx_root.join("scripts").join("hello.sh"),
        r#"#!/usr/bin/env bash
echo "hello"
"#,
    )
    .unwrap();

    let result = FsSkill::discover(skills_root).unwrap();
    let skills: Vec<Arc<dyn Skill>> = result
        .skills
        .into_iter()
        .map(|s| Arc::new(s) as Arc<dyn Skill>)
        .collect();
    let registry: Arc<dyn SkillRegistry> = Arc::new(InMemorySkillRegistry::from_skills(skills));
    (td, registry)
}

async fn apply_tool(thread: Thread, tool: &dyn Tool, call: ToolCall) -> (Thread, ToolResult) {
    let state = thread.rebuild_state().unwrap();
    let behavior = TestToolBehavior::new();
    let exec = execute_single_tool_with_scope_and_behavior(
        Some(tool),
        &call,
        &state,
        None,
        Some(&behavior),
    )
    .await;
    let thread = if let Some(patch) = exec.patch.clone() {
        thread.with_patch(patch)
    } else {
        thread
    };
    (thread, exec.result)
}

async fn apply_tool_with_scope(
    thread: Thread,
    tool: &dyn Tool,
    call: ToolCall,
    scope: &tirea_contract::RunConfig,
) -> (Thread, ToolResult) {
    let state = thread.rebuild_state().unwrap();
    let behavior = TestToolBehavior::new();
    let exec = execute_single_tool_with_scope_and_behavior(
        Some(tool),
        &call,
        &state,
        Some(scope),
        Some(&behavior),
    )
    .await;
    let thread = if let Some(patch) = exec.patch.clone() {
        thread.with_patch(patch)
    } else {
        thread
    };
    (thread, exec.result)
}

fn assert_error_code(result: &ToolResult, expected_code: &str) {
    assert!(result.is_error(), "expected an error result");
    assert_eq!(result.data["error"]["code"], expected_code);
}

fn assert_invalid_arguments_error(result: &ToolResult) {
    assert!(result.is_error(), "expected an error result");
    if result.data["error"]["code"] == "invalid_arguments" {
        return;
    }
    let message = result
        .message
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        message.contains("invalid arguments"),
        "expected invalid_arguments code or validation error message, got: {:?}",
        result
    );
}

#[tokio::test]
async fn test_skill_activation_delivers_instructions_via_user_messages_on_effect() {
    let (_td, skills) = make_skill_tree();
    let activate = SkillActivateTool::new(skills);

    let state = json!({});
    let doc = tirea_state::DocCell::new(state);
    let ops = std::sync::Mutex::new(Vec::new());
    let pending_messages = std::sync::Mutex::new(Vec::new());
    let scope = tirea_contract::RunConfig::default();
    let ctx = tirea_contract::runtime::tool_call::ToolCallContext::new(
        &doc,
        &ops,
        "call_1",
        "tool:skill",
        &scope,
        &pending_messages,
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
    );

    let effect = activate
        .execute_effect(json!({"skill": "docx"}), &ctx)
        .await
        .expect("execute_effect should succeed");
    let (result, _state_actions, user_messages) = effect.into_parts();
    assert!(result.is_success(), "result={result:?}");
    assert_eq!(result.message.as_deref(), Some("Launching skill: docx"));
    assert_eq!(user_messages.len(), 1);
    assert!(user_messages[0].contains("Use docx-js for new documents"));
}

#[tokio::test]
async fn test_skill_activation_respects_scope_skill_policy() {
    let (_td, skills) = make_skill_tree();
    let activate = SkillActivateTool::new(skills);
    let thread = Thread::with_initial_state("s", json!({}));
    let mut scope = tirea_contract::RunConfig::new();
    scope
        .set(SCOPE_ALLOWED_SKILLS_KEY, vec!["other-skill"])
        .unwrap();

    let (_thread, result) = apply_tool_with_scope(
        thread,
        &activate,
        ToolCall::new("call_1", "skill", json!({"skill": "docx"})),
        &scope,
    )
    .await;
    assert_error_code(&result, "forbidden_skill");
}

#[tokio::test]
async fn test_load_skill_resource_respects_scope_skill_policy() {
    let (_td, skills) = make_skill_tree();
    let load = LoadSkillResourceTool::new(skills);
    let thread = Thread::with_initial_state("s", json!({}));
    let mut scope = tirea_contract::RunConfig::new();
    scope
        .set(SCOPE_ALLOWED_SKILLS_KEY, vec!["other-skill"])
        .unwrap();

    let (_thread, result) = apply_tool_with_scope(
        thread,
        &load,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "references/DOCX-JS.md"}),
        ),
        &scope,
    )
    .await;
    assert_error_code(&result, "forbidden_skill");
}

#[tokio::test]
async fn test_load_reference_returns_content_in_tool_result() {
    let (_td, skills) = make_skill_tree();
    let load_ref = LoadSkillResourceTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));

    let (_thread, result) = apply_tool(
        thread,
        &load_ref,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "references/DOCX-JS.md"}),
        ),
    )
    .await;
    assert!(result.is_success(), "result={result:?}");
    assert_eq!(result.data["loaded"], true);
    assert_eq!(result.data["kind"], "reference");
    assert_eq!(result.data["path"], "references/DOCX-JS.md");
}

#[tokio::test]
async fn test_script_result_is_persisted_in_state() {
    let (_td, skills) = make_skill_tree();
    let activate = SkillActivateTool::new(skills.clone());
    let run_script = SkillScriptTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({})).with_message(Message::user("hi"));

    let (thread, _) = apply_tool(
        thread,
        &activate,
        ToolCall::new("call_1", "skill", json!({"skill": "docx"})),
    )
    .await;

    let (_thread, result) = apply_tool(
        thread,
        &run_script,
        ToolCall::new(
            "call_2",
            "skill_script",
            json!({"skill": "docx", "script": "scripts/hello.sh"}),
        ),
    )
    .await;
    assert!(result.is_success(), "result={result:?}");
    assert_eq!(result.data["ok"], true);
    assert_eq!(result.data["script"], "scripts/hello.sh");
}

#[tokio::test]
async fn test_load_asset_returns_metadata_in_tool_result() {
    let (_td, skills) = make_skill_tree();
    let load_asset = LoadSkillResourceTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));

    let (_thread, result) = apply_tool(
        thread,
        &load_asset,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "assets/logo.txt"}),
        ),
    )
    .await;
    assert!(result.is_success(), "result={result:?}");
    assert_eq!(result.data["encoding"], "base64");
    assert_eq!(result.data["kind"], "asset");
    assert_eq!(result.data["path"], "assets/logo.txt");
}

#[tokio::test]
async fn test_load_reference_rejects_escape() {
    let (_td, skills) = make_skill_tree();
    let load_ref = LoadSkillResourceTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &load_ref,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "../secrets.txt"}),
        ),
    )
    .await;

    assert_error_code(&result, "invalid_path");
}

#[tokio::test]
async fn test_load_resource_requires_supported_prefix() {
    let (_td, skills) = make_skill_tree();
    let load_asset = LoadSkillResourceTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &load_asset,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "resource/DOCX-JS.md"}),
        ),
    )
    .await;

    assert_error_code(&result, "unsupported_path");
}

#[tokio::test]
async fn test_load_resource_kind_mismatch_is_error() {
    let (_td, skills) = make_skill_tree();
    let load_resource = LoadSkillResourceTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &load_resource,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "assets/logo.txt", "kind": "reference"}),
        ),
    )
    .await;

    assert_error_code(&result, "invalid_arguments");
}

#[tokio::test]
async fn test_load_resource_explicit_kind_asset_works() {
    let (_td, skills) = make_skill_tree();
    let load_resource = LoadSkillResourceTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &load_resource,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "assets/logo.txt", "kind": "asset"}),
        ),
    )
    .await;

    assert!(result.is_success());
    assert_eq!(result.data["kind"], "asset");
}

#[tokio::test]
async fn test_skill_activation_requires_exact_skill_name() {
    let (_td, skills) = make_skill_tree();
    let activate = SkillActivateTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &activate,
        ToolCall::new("call_1", "skill", json!({"skill": "DOCX"})),
    )
    .await;

    assert_error_code(&result, "unknown_skill");
}

#[tokio::test]
async fn test_skill_activation_unknown_skill_errors() {
    let (_td, skills) = make_skill_tree();
    let activate = SkillActivateTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &activate,
        ToolCall::new("call_1", "skill", json!({"skill": "nope"})),
    )
    .await;

    assert_error_code(&result, "unknown_skill");
}

#[tokio::test]
async fn test_skill_activation_missing_skill_argument_is_error() {
    let (_td, skills) = make_skill_tree();
    let activate = SkillActivateTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &activate,
        ToolCall::new("call_1", "skill", json!({})),
    )
    .await;

    assert_invalid_arguments_error(&result);
}

#[tokio::test]
async fn test_skill_activation_applies_allowed_tools_to_permission_state() {
    let (_td, skills) = make_skill_tree();
    let activate = SkillActivateTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (thread, result) = apply_tool(
        thread,
        &activate,
        ToolCall::new("call_1", "skill", json!({"skill": "docx"})),
    )
    .await;
    assert!(result.is_success());

    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["permissions"]["tools"]["read_file"], "allow");
}

#[tokio::test]
async fn test_skill_activation_user_messages_contain_skill_instructions() {
    let (_td, skills) = make_skill_tree();
    let activate = SkillActivateTool::new(skills);

    let state = json!({});
    let doc = tirea_state::DocCell::new(state);
    let ops = std::sync::Mutex::new(Vec::new());
    let pending_messages = std::sync::Mutex::new(Vec::new());
    let scope = tirea_contract::RunConfig::default();
    let ctx = tirea_contract::runtime::tool_call::ToolCallContext::new(
        &doc,
        &ops,
        "call_1",
        "tool:skill",
        &scope,
        &pending_messages,
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
    );

    let effect = activate
        .execute_effect(json!({"skill": "docx"}), &ctx)
        .await
        .expect("execute_effect should succeed");
    let (result, _state_actions, user_messages) = effect.into_parts();
    assert!(result.is_success());
    assert_eq!(user_messages.len(), 1);
    assert!(
        user_messages[0].contains("# DOCX Processing"),
        "expected SKILL.md heading in user message, got: {}",
        &user_messages[0][..user_messages[0].len().min(200)]
    );
}

#[tokio::test]
async fn test_skill_activation_requires_skill_md_to_exist_at_activation_time() {
    let (td, skills) = make_skill_tree();
    // Ensure discovery has produced the meta and cached SKILL.md content.
    assert_eq!(skills.len(), 1);
    fs::remove_file(td.path().join("skills").join("docx").join("SKILL.md")).unwrap();

    let activate = SkillActivateTool::new(skills);
    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &activate,
        ToolCall::new("call_1", "skill", json!({"skill": "docx"})),
    )
    .await;

    assert_error_code(&result, "io_error");
}

#[tokio::test]
async fn test_load_reference_requires_references_prefix() {
    let (_td, skills) = make_skill_tree();
    let load_ref = LoadSkillResourceTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &load_ref,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "SKILL.md"}),
        ),
    )
    .await;

    assert_error_code(&result, "unsupported_path");
}

#[tokio::test]
async fn test_load_reference_missing_arguments_are_errors() {
    let (_td, skills) = make_skill_tree();
    let load_ref = LoadSkillResourceTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));

    let (_thread, r1) = apply_tool(
        thread.clone(),
        &load_ref,
        ToolCall::new("call_1", "load_skill_resource", json!({"skill": "docx"})),
    )
    .await;
    assert_invalid_arguments_error(&r1);

    let (_thread, r2) = apply_tool(
        thread,
        &load_ref,
        ToolCall::new(
            "call_2",
            "load_skill_resource",
            json!({"path": "references/DOCX-JS.md"}),
        ),
    )
    .await;
    assert_invalid_arguments_error(&r2);
}

#[tokio::test]
async fn test_load_reference_invalid_utf8_is_error() {
    let (td, skills) = make_skill_tree();
    let load_ref = LoadSkillResourceTool::new(skills);

    let refs_dir = td.path().join("skills").join("docx").join("references");
    fs::write(refs_dir.join("BAD.bin"), vec![0xff, 0xfe, 0xfd]).unwrap();

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &load_ref,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "references/BAD.bin"}),
        ),
    )
    .await;

    assert_error_code(&result, "io_error");
}

#[tokio::test]
async fn test_load_reference_missing_file_is_error() {
    let (_td, skills) = make_skill_tree();
    let load_ref = LoadSkillResourceTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &load_ref,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "references/DOES_NOT_EXIST.md"}),
        ),
    )
    .await;

    assert_error_code(&result, "io_error");
}

#[cfg(unix)]
#[tokio::test]
async fn test_load_reference_symlink_escape_is_error() {
    use std::os::unix::fs as unix_fs;

    let (td, skills) = make_skill_tree();
    let load_ref = LoadSkillResourceTool::new(skills);

    let outside = td.path().join("outside.md");
    fs::write(&outside, "outside").unwrap();

    let refs_dir = td.path().join("skills").join("docx").join("references");
    unix_fs::symlink(&outside, refs_dir.join("ESCAPE.md")).unwrap();

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &load_ref,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "references/ESCAPE.md"}),
        ),
    )
    .await;

    assert_error_code(&result, "path_escapes_root");
}

#[tokio::test]
async fn test_script_requires_scripts_prefix() {
    let (_td, skills) = make_skill_tree();
    let run_script = SkillScriptTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &run_script,
        ToolCall::new(
            "call_1",
            "skill_script",
            json!({"skill": "docx", "script": "references/DOCX-JS.md"}),
        ),
    )
    .await;

    assert_error_code(&result, "unsupported_path");
}

#[tokio::test]
async fn test_script_missing_arguments_are_errors() {
    let (_td, skills) = make_skill_tree();
    let run_script = SkillScriptTool::new(skills);

    let thread = Thread::with_initial_state("s", json!({}));

    let (_thread, r1) = apply_tool(
        thread.clone(),
        &run_script,
        ToolCall::new("call_1", "skill_script", json!({"skill": "docx"})),
    )
    .await;
    assert_invalid_arguments_error(&r1);

    let (_thread, r2) = apply_tool(
        thread,
        &run_script,
        ToolCall::new(
            "call_2",
            "skill_script",
            json!({"script": "scripts/hello.sh"}),
        ),
    )
    .await;
    assert_invalid_arguments_error(&r2);
}

#[tokio::test]
async fn test_script_args_are_passed_through() {
    let (td, skills) = make_skill_tree();
    let run_script = SkillScriptTool::new(skills);

    let scripts_dir = td.path().join("skills").join("docx").join("scripts");
    fs::write(
        scripts_dir.join("echo_args.sh"),
        r#"#!/usr/bin/env bash
printf "%s" "$*"
"#,
    )
    .unwrap();

    let thread = Thread::with_initial_state("s", json!({}));

    let (_thread, result) = apply_tool(
        thread,
        &run_script,
        ToolCall::new(
            "call_1",
            "skill_script",
            json!({"skill": "docx", "script": "scripts/echo_args.sh", "args": ["a", "b"]}),
        ),
    )
    .await;
    assert!(result.is_success());
    assert_eq!(result.data["ok"], true);
}

#[tokio::test]
async fn test_script_nonzero_exit_sets_ok_false() {
    let (td, skills) = make_skill_tree();
    let run_script = SkillScriptTool::new(skills);

    let scripts_dir = td.path().join("skills").join("docx").join("scripts");
    fs::write(
        scripts_dir.join("fail.sh"),
        r#"#!/usr/bin/env bash
echo "nope"
exit 2
"#,
    )
    .unwrap();

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &run_script,
        ToolCall::new(
            "call_1",
            "skill_script",
            json!({"skill": "docx", "script": "scripts/fail.sh"}),
        ),
    )
    .await;

    assert!(result.is_success());
    assert_eq!(result.data["ok"], false);
    assert_eq!(result.data["exit_code"], 2);
}

#[tokio::test]
async fn test_script_unsupported_runtime_is_error() {
    let (td, skills) = make_skill_tree();
    // Create a script with an unsupported extension.
    let scripts_dir = td.path().join("skills").join("docx").join("scripts");
    fs::write(scripts_dir.join("bad.rb"), "puts 'hi'\n").unwrap();

    let run_script = SkillScriptTool::new(skills);
    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &run_script,
        ToolCall::new(
            "call_1",
            "skill_script",
            json!({"skill": "docx", "script": "scripts/bad.rb"}),
        ),
    )
    .await;

    assert_error_code(&result, "unsupported_runtime");
}

#[tokio::test]
async fn test_script_rejects_excessive_argument_count() {
    let (_td, skills) = make_skill_tree();
    let run_script = SkillScriptTool::new(skills);

    let mut args = Vec::new();
    for i in 0..70 {
        args.push(format!("arg-{i}"));
    }

    let thread = Thread::with_initial_state("s", json!({}));
    let (_thread, result) = apply_tool(
        thread,
        &run_script,
        ToolCall::new(
            "call_1",
            "skill_script",
            json!({"skill": "docx", "script": "scripts/hello.sh", "args": args}),
        ),
    )
    .await;

    assert_error_code(&result, "invalid_arguments");
}

#[tokio::test]
async fn test_skill_activation_does_not_widen_scoped_allowed_tools() {
    let td = TempDir::new().unwrap();
    let skills_root = td.path().join("skills");
    let root = skills_root.join("scoped");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("SKILL.md"),
        r#"---
name: scoped
description: scoped allowed tools
allowed-tools: 'read_file Bash(command: "git status")'
---
Body
"#,
    )
    .unwrap();
    let result = FsSkill::discover(skills_root).unwrap();
    let skills: Vec<Arc<dyn Skill>> = result
        .skills
        .into_iter()
        .map(|s| Arc::new(s) as Arc<dyn Skill>)
        .collect();
    let registry: Arc<dyn SkillRegistry> = Arc::new(InMemorySkillRegistry::from_skills(skills));
    let activate = SkillActivateTool::new(registry);

    let thread = Thread::with_initial_state("s", json!({}));
    let (thread, result) = apply_tool(
        thread,
        &activate,
        ToolCall::new("call_1", "skill", json!({"skill": "scoped"})),
    )
    .await;

    assert!(result.is_success());
    assert_eq!(result.data["skill_id"], "scoped");

    // Verify bare tool ids are applied but scoped ones are not widened.
    let state = thread.rebuild_state().unwrap();
    assert_eq!(state["permissions"]["tools"]["read_file"], "allow");
    assert!(
        state["permissions"]["tools"].get("Bash").is_none(),
        "scoped Bash permission must not be widened to plain Bash"
    );
}

#[tokio::test]
async fn test_reference_truncation_flag_in_tool_result() {
    let (td, skills) = make_skill_tree();
    let load_ref = LoadSkillResourceTool::new(skills);

    // Create a big reference file (>256KiB).
    let big = "a".repeat(257 * 1024);
    let refs_dir = td.path().join("skills").join("docx").join("references");
    fs::write(refs_dir.join("BIG.md"), big).unwrap();

    let thread = Thread::with_initial_state("s", json!({}));

    let (_thread, result) = apply_tool(
        thread,
        &load_ref,
        ToolCall::new(
            "call_1",
            "load_skill_resource",
            json!({"skill": "docx", "path": "references/BIG.md"}),
        ),
    )
    .await;
    assert!(result.is_success());
    assert_eq!(result.data["path"], "references/BIG.md");
    assert_eq!(result.data["truncated"], true);
}

#[tokio::test]
async fn test_script_stdout_truncation_flag_in_tool_result() {
    let (td, skills) = make_skill_tree();
    let run_script = SkillScriptTool::new(skills);

    // Print >32KiB to stdout.
    let scripts_dir = td.path().join("skills").join("docx").join("scripts");
    fs::write(
        scripts_dir.join("big.sh"),
        r#"#!/usr/bin/env bash
head -c 40000 /dev/zero | tr '\0' 'a'
"#,
    )
    .unwrap();

    let thread = Thread::with_initial_state("s", json!({}));

    let (_thread, result) = apply_tool(
        thread,
        &run_script,
        ToolCall::new(
            "call_1",
            "skill_script",
            json!({"skill": "docx", "script": "scripts/big.sh"}),
        ),
    )
    .await;
    assert!(result.is_success());
    assert_eq!(result.data["script"], "scripts/big.sh");
    assert_eq!(result.data["stdout_truncated"], true);
}
