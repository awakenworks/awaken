//! Integration tests for skill tool → state management pipeline.
//!
//! These tests verify the end-to-end flow: tool execution → state mutation
//! → hook reads state → context injection. They deliberately expose gaps
//! where tools should but do not write state.

use std::sync::Arc;

use awaken_contract::contract::tool::{Tool, ToolCallContext, ToolStatus};
use awaken_contract::state::{MergeStrategy, StateKey};
use awaken_ext_skills::registry::InMemorySkillRegistry;
use awaken_ext_skills::state::{SkillState, SkillStateUpdate, SkillStateValue};
use awaken_ext_skills::tools::{LoadSkillResourceTool, SkillActivateTool, SkillScriptTool};
use awaken_ext_skills::{
    EmbeddedSkill, EmbeddedSkillData, SKILL_ACTIVATE_TOOL_ID, SKILL_LOAD_RESOURCE_TOOL_ID,
    SKILL_SCRIPT_TOOL_ID,
};
use serde_json::{Value, json};

// ── Test skill definitions ────────────────────────────────────────

const SKILL_BASIC_MD: &str = "\
---
name: basic-skill
description: A basic test skill with no allowed tools
---
# Basic Skill

Instructions for basic skill.
";

const SKILL_WITH_TOOLS_MD: &str = "\
---
name: tool-skill
description: A skill that declares allowed tools
allowed-tools: Bash Read Edit
---
# Tool Skill

Instructions for tool-skill with elevated permissions.
";

const SKILL_WITH_PATTERN_MD: &str = "\
---
name: pattern-skill
description: A skill that declares pattern-based allowed tools
allowed-tools: mcp__* Bash(npm *)
---
# Pattern Skill

Instructions for pattern-skill.
";

const SKILL_WITH_REFS_MD: &str = "\
---
name: ref-skill
description: A skill with references
---
# Ref Skill

Instructions for ref-skill.
";

fn make_registry_with_skills(skill_data: &[EmbeddedSkillData]) -> Arc<InMemorySkillRegistry> {
    let skills = EmbeddedSkill::from_static_slice(skill_data).unwrap();
    Arc::new(InMemorySkillRegistry::from_skills(skills))
}

fn default_ctx() -> ToolCallContext {
    ToolCallContext::test_default()
}

// ═══════════════════════════════════════════════════════════════════
// SkillActivateTool tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn activate_tool_returns_success_for_known_skill() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "basic-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    assert_eq!(output.result.data["activated"], true);
    assert_eq!(output.result.data["skill_id"], "basic-skill");
}

#[tokio::test]
async fn activate_tool_returns_error_for_unknown_skill() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "nonexistent"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
    assert!(output.result.data.to_string().contains("unknown_skill"));
}

#[tokio::test]
async fn activate_tool_returns_error_for_missing_skill_arg() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool.execute(json!({}), &default_ctx()).await.unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
    assert!(output.result.data.to_string().contains("missing"));
}

#[tokio::test]
async fn activate_tool_returns_error_for_empty_skill_arg() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "  "}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
}

#[tokio::test]
async fn activate_tool_trims_skill_id_whitespace() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "  basic-skill  "}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    assert_eq!(output.result.data["skill_id"], "basic-skill");
}

#[tokio::test]
async fn activate_tool_passes_arguments_to_skill() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    // Both "args" and "arguments" forms should work
    let output = tool
        .execute(
            json!({"skill": "basic-skill", "args": "some arg"}),
            &default_ctx(),
        )
        .await
        .unwrap();
    assert_eq!(output.result.status, ToolStatus::Success);

    let result2 = tool
        .execute(
            json!({"skill": "basic-skill", "arguments": {"key": "val"}}),
            &default_ctx(),
        )
        .await
        .unwrap();
    assert_eq!(result2.result.status, ToolStatus::Success);
}

// ── CRITICAL: State management gap tests ──────────────────────────

/// This test demonstrates that SkillActivateTool does NOT write to SkillState.
/// After activation, the state should contain the activated skill ID, but it doesn't.
///
/// EXPECTED BEHAVIOR: After tool.execute(), the tool result should carry
/// a state mutation that adds the skill_id to SkillState.active.
///
/// ACTUAL BEHAVIOR: SkillState is never written. This test documents the bug.
#[tokio::test]
async fn activate_tool_result_should_contain_skill_id_for_state_update() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_TOOLS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "tool-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);

    // The tool result MUST contain the skill_id so that an AfterToolExecute
    // hook (or the tool itself) can write SkillState.
    assert_eq!(output.result.data["skill_id"], "tool-skill");

    // FIXED: The tool now writes SkillState + permission overrides via command.
    assert!(
        !output.command.is_empty(),
        "command should contain SkillState activation + permission overrides"
    );
}

/// Verify that SkillStateUpdate::Activate is NEVER called in production.
/// This test simulates what SHOULD happen but currently doesn't.
#[tokio::test]
async fn activate_tool_should_update_skill_state_but_doesnt() {
    // This test manually does what the tool SHOULD do after execution.
    let mut state = SkillStateValue::default();
    assert!(state.active.is_empty());

    // Simulate what SkillActivateTool returns:
    let data = json!({"activated": true, "skill_id": "tool-skill"});

    // Extract skill_id from result (this is what an AfterToolExecute hook would do)
    let skill_id = data["skill_id"].as_str().unwrap();

    // Apply the state update manually (what the hook should do)
    SkillState::apply(&mut state, SkillStateUpdate::Activate(skill_id.into()));
    assert!(state.active.contains("tool-skill"));

    // NOTE: In production, nobody does this. The tool returns the result,
    // no AfterToolExecute hook exists to read it, and SkillState stays empty.
}

/// Verify that allowed_tools from skill metadata are parsed but never applied.
#[tokio::test]
async fn activate_tool_parses_allowed_tools_but_discards_them() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_TOOLS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "tool-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);

    // FIXED: allowed_tools are now written as permission overrides in the command.
    // The command carries SkillState::Activate + PermissionOverridesKey updates.
    assert!(
        !output.command.is_empty(),
        "command should carry SkillState + permission overrides for Bash, Read, Edit"
    );
}

/// Verify that pattern-based allowed_tools are also parsed but lost.
#[tokio::test]
async fn activate_tool_handles_pattern_allowed_tools() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_PATTERN_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "pattern-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    // FIXED: Patterns are now written as permission rule overrides in the command.
    assert!(!output.command.is_empty());
}

/// SkillState key properties should be correct for the intended use.
#[test]
fn skill_state_key_properties_are_correct() {
    assert_eq!(SkillState::KEY, "skills");
    assert_eq!(SkillState::MERGE, MergeStrategy::Commutative);
    // Commutative merge is essential: parallel tool calls activating
    // different skills must merge without conflict.
}

/// Concurrent activations should merge via commutative union.
#[test]
fn skill_state_concurrent_activations_merge_correctly() {
    let mut state1 = SkillStateValue::default();
    SkillState::apply(&mut state1, SkillStateUpdate::Activate("skill-a".into()));

    let mut state2 = SkillStateValue::default();
    SkillState::apply(&mut state2, SkillStateUpdate::Activate("skill-b".into()));

    // Simulate commutative merge (both orderings should produce same result)
    let mut merged_ab = state1.clone();
    for id in &state2.active {
        SkillState::apply(&mut merged_ab, SkillStateUpdate::Activate(id.clone()));
    }

    let mut merged_ba = state2.clone();
    for id in &state1.active {
        SkillState::apply(&mut merged_ba, SkillStateUpdate::Activate(id.clone()));
    }

    assert_eq!(merged_ab.active, merged_ba.active);
    assert_eq!(merged_ab.active.len(), 2);
}

// ═══════════════════════════════════════════════════════════════════
// LoadSkillResourceTool tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn load_resource_returns_reference_content() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_REFS_MD,
        references: &[("references/guide.md", "# Guide Content\n\nHelpful guide.")],
        assets: &[],
    }]);
    let tool = LoadSkillResourceTool::new(registry);

    let output = tool
        .execute(
            json!({"skill": "ref-skill", "path": "references/guide.md"}),
            &default_ctx(),
        )
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    assert_eq!(output.result.data["loaded"], true);
    assert_eq!(output.result.data["skill_id"], "ref-skill");
    assert_eq!(output.result.data["kind"], "reference");
    assert_eq!(
        output.result.data["content"],
        "# Guide Content\n\nHelpful guide."
    );
}

#[tokio::test]
async fn load_resource_infers_kind_from_path_prefix() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_REFS_MD,
        references: &[("references/doc.md", "content")],
        assets: &[],
    }]);
    let tool = LoadSkillResourceTool::new(registry);

    // Kind inferred from "references/" prefix
    let output = tool
        .execute(
            json!({"skill": "ref-skill", "path": "references/doc.md"}),
            &default_ctx(),
        )
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    assert_eq!(output.result.data["kind"], "reference");
}

#[tokio::test]
async fn load_resource_errors_for_unknown_skill() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_REFS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = LoadSkillResourceTool::new(registry);

    let output = tool
        .execute(
            json!({"skill": "nonexistent", "path": "references/x.md"}),
            &default_ctx(),
        )
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
    assert!(output.result.data.to_string().contains("unknown_skill"));
}

#[tokio::test]
async fn load_resource_errors_for_missing_path() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_REFS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = LoadSkillResourceTool::new(registry);

    let output = tool
        .execute(json!({"skill": "ref-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
}

#[tokio::test]
async fn load_resource_errors_for_missing_reference() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_REFS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = LoadSkillResourceTool::new(registry);

    let output = tool
        .execute(
            json!({"skill": "ref-skill", "path": "references/missing.md"}),
            &default_ctx(),
        )
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
}

#[tokio::test]
async fn load_resource_errors_for_missing_skill_arg() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_REFS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = LoadSkillResourceTool::new(registry);

    let output = tool
        .execute(json!({"path": "references/guide.md"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
}

// ═══════════════════════════════════════════════════════════════════
// SkillScriptTool tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn script_tool_errors_for_unknown_skill() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillScriptTool::new(registry);

    let output = tool
        .execute(
            json!({"skill": "nonexistent", "script": "scripts/run.sh"}),
            &default_ctx(),
        )
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
    assert!(output.result.data.to_string().contains("unknown_skill"));
}

#[tokio::test]
async fn script_tool_errors_for_embedded_skill() {
    // Embedded skills don't support script execution
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillScriptTool::new(registry);

    let output = tool
        .execute(
            json!({"skill": "basic-skill", "script": "scripts/run.sh"}),
            &default_ctx(),
        )
        .await
        .unwrap();

    // Embedded skills return an error for script execution
    assert_eq!(output.result.status, ToolStatus::Error);
}

#[tokio::test]
async fn script_tool_errors_for_missing_skill_arg() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillScriptTool::new(registry);

    let output = tool
        .execute(json!({"script": "scripts/run.sh"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
}

#[tokio::test]
async fn script_tool_errors_for_missing_script_arg() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillScriptTool::new(registry);

    let output = tool
        .execute(json!({"skill": "basic-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
}

// ═══════════════════════════════════════════════════════════════════
// Tool descriptor tests
// ═══════════════════════════════════════════════════════════════════

#[test]
fn activate_tool_descriptor_has_correct_id() {
    let registry = make_registry_with_skills(&[]);
    let tool = SkillActivateTool::new(registry);
    let desc = tool.descriptor();
    assert_eq!(desc.id, SKILL_ACTIVATE_TOOL_ID);
}

#[test]
fn load_resource_tool_descriptor_has_correct_id() {
    let registry = make_registry_with_skills(&[]);
    let tool = LoadSkillResourceTool::new(registry);
    let desc = tool.descriptor();
    assert_eq!(desc.id, SKILL_LOAD_RESOURCE_TOOL_ID);
}

#[test]
fn script_tool_descriptor_has_correct_id() {
    let registry = make_registry_with_skills(&[]);
    let tool = SkillScriptTool::new(registry);
    let desc = tool.descriptor();
    assert_eq!(desc.id, SKILL_SCRIPT_TOOL_ID);
}

#[test]
fn activate_tool_descriptor_requires_skill_parameter() {
    let registry = make_registry_with_skills(&[]);
    let tool = SkillActivateTool::new(registry);
    let desc = tool.descriptor();
    let required = desc.parameters["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "skill"));
}

#[test]
fn load_resource_descriptor_requires_skill_and_path() {
    let registry = make_registry_with_skills(&[]);
    let tool = LoadSkillResourceTool::new(registry);
    let desc = tool.descriptor();
    let required = desc.parameters["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "skill"));
    assert!(required.iter().any(|v| v == "path"));
}

// ═══════════════════════════════════════════════════════════════════
// Validation tests
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn activate_tool_handles_null_args_gracefully() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool.execute(Value::Null, &default_ctx()).await.unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
}

#[tokio::test]
async fn activate_tool_handles_numeric_skill_arg() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": 123}), &default_ctx())
        .await
        .unwrap();

    // numeric skill arg should be rejected (not a string)
    assert_eq!(output.result.status, ToolStatus::Error);
}

#[tokio::test]
async fn load_resource_rejects_path_traversal() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_REFS_MD,
        references: &[("references/guide.md", "content")],
        assets: &[],
    }]);
    let tool = LoadSkillResourceTool::new(registry);

    let output = tool
        .execute(
            json!({"skill": "ref-skill", "path": "../../../etc/passwd"}),
            &default_ctx(),
        )
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
}

#[tokio::test]
async fn load_resource_rejects_absolute_path() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_REFS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = LoadSkillResourceTool::new(registry);

    let output = tool
        .execute(
            json!({"skill": "ref-skill", "path": "/etc/passwd"}),
            &default_ctx(),
        )
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Error);
}

// ═══════════════════════════════════════════════════════════════════
// Multiple skill activation sequence
// ═══════════════════════════════════════════════════════════════════

#[tokio::test]
async fn activate_multiple_skills_sequentially() {
    let registry = make_registry_with_skills(&[
        EmbeddedSkillData {
            skill_md: SKILL_BASIC_MD,
            references: &[],
            assets: &[],
        },
        EmbeddedSkillData {
            skill_md: SKILL_WITH_TOOLS_MD,
            references: &[],
            assets: &[],
        },
    ]);
    let tool = SkillActivateTool::new(registry);

    let r1 = tool
        .execute(json!({"skill": "basic-skill"}), &default_ctx())
        .await
        .unwrap();
    assert_eq!(r1.result.status, ToolStatus::Success);
    assert_eq!(r1.result.data["skill_id"], "basic-skill");

    let r2 = tool
        .execute(json!({"skill": "tool-skill"}), &default_ctx())
        .await
        .unwrap();
    assert_eq!(r2.result.status, ToolStatus::Success);
    assert_eq!(r2.result.data["skill_id"], "tool-skill");
}

// ═══════════════════════════════════════════════════════════════════
// SkillActivateTool StateCommand output tests
// ═══════════════════════════════════════════════════════════════════

/// Verify that SkillActivateTool command writes SkillState activation.
#[tokio::test]
async fn activate_tool_command_writes_skill_state() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "basic-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    // Command should contain at least the SkillState activation mutation
    assert!(
        !output.command.is_empty(),
        "command should write SkillState::Activate"
    );
    // touched_keys should include "skills" (SkillState::KEY)
    assert!(
        output.command.touched_keys.contains(&"skills".to_string()),
        "command should touch SkillState key"
    );
}

/// Verify that skill with allowed_tools produces permission override mutations.
#[tokio::test]
async fn activate_tool_command_grants_permission_overrides() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_TOOLS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "tool-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    // Command should touch both SkillState and PermissionOverridesKey
    assert!(output.command.touched_keys.contains(&"skills".to_string()));
    assert!(
        output
            .command
            .touched_keys
            .contains(&"permission_overrides".to_string()),
        "command should write permission_overrides for allowed_tools [Bash, Read, Edit]"
    );
    // 1 SkillState mutation + 3 permission overrides = at least 4 ops
    assert!(
        output.command.op_len() >= 4,
        "expected at least 4 mutations (1 skill + 3 tool overrides), got {}",
        output.command.op_len()
    );
}

/// Verify that pattern-based allowed_tools produce permission rule overrides.
#[tokio::test]
async fn activate_tool_command_grants_pattern_permission_overrides() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_PATTERN_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "pattern-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    assert!(
        output
            .command
            .touched_keys
            .contains(&"permission_overrides".to_string()),
        "command should contain pattern permission overrides"
    );
}

/// Skill without allowed_tools should only produce SkillState mutation (no permission overrides).
#[tokio::test]
async fn activate_tool_command_no_permission_overrides_when_no_allowed_tools() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "basic-skill"}), &default_ctx())
        .await
        .unwrap();

    // Only SkillState should be touched, not permission_overrides
    assert!(output.command.touched_keys.contains(&"skills".to_string()));
    assert!(
        !output
            .command
            .touched_keys
            .contains(&"permission_overrides".to_string()),
        "should not write permission_overrides when skill has no allowed_tools"
    );
    assert_eq!(output.command.op_len(), 1, "only SkillState mutation");
}

/// Verify that command applies correctly to a real StateStore.
#[tokio::test]
async fn activate_tool_command_applies_to_state_store() {
    use awaken_contract::state::{Snapshot, StateMap};
    use awaken_ext_permission::evaluate_tool_permission;
    use awaken_ext_permission::rules::ToolPermissionBehavior;
    use awaken_ext_permission::state::{PermissionOverridesKey, permission_rules_from_state};
    use std::sync::Arc;

    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_TOOLS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "tool-skill"}), &default_ctx())
        .await
        .unwrap();

    // Apply the command to a snapshot
    let mut snapshot = Snapshot::new(0, Arc::new(StateMap::default()));
    for op in output.command.patch.ops {
        op.apply(&mut snapshot);
    }

    // Verify SkillState was updated
    let skill_state = snapshot.get::<SkillState>().unwrap();
    assert!(skill_state.active.contains("tool-skill"));

    // Verify permission overrides were written
    let overrides = snapshot.get::<PermissionOverridesKey>().unwrap();
    let ruleset = permission_rules_from_state(None, Some(overrides));

    let eval_bash = evaluate_tool_permission(&ruleset, "Bash", &json!({}));
    assert_eq!(
        eval_bash.behavior,
        ToolPermissionBehavior::Allow,
        "Bash should be allowed"
    );

    let eval_read = evaluate_tool_permission(&ruleset, "Read", &json!({}));
    assert_eq!(
        eval_read.behavior,
        ToolPermissionBehavior::Allow,
        "Read should be allowed"
    );

    let eval_edit = evaluate_tool_permission(&ruleset, "Edit", &json!({}));
    assert_eq!(
        eval_edit.behavior,
        ToolPermissionBehavior::Allow,
        "Edit should be allowed"
    );
}

/// Verify that permission overrides are run-scoped (from PermissionOverridesKey semantics).
#[tokio::test]
async fn activate_tool_permission_overrides_use_skill_source() {
    use awaken_contract::state::{Snapshot, StateMap};
    use awaken_ext_permission::PermissionRuleSource;
    use awaken_ext_permission::state::PermissionOverridesKey;
    use std::sync::Arc;

    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_TOOLS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let output = tool
        .execute(json!({"skill": "tool-skill"}), &default_ctx())
        .await
        .unwrap();

    let mut snapshot = Snapshot::new(0, Arc::new(StateMap::default()));
    for op in output.command.patch.ops {
        op.apply(&mut snapshot);
    }

    let overrides = snapshot.get::<PermissionOverridesKey>().unwrap();
    // All rules should have Skill source
    for rule in overrides.rules.values() {
        assert_eq!(rule.source, PermissionRuleSource::Skill);
    }
}

/// Activating the same skill twice should succeed (idempotent).
#[tokio::test]
async fn activate_same_skill_twice_is_idempotent() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_BASIC_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    let r1 = tool
        .execute(json!({"skill": "basic-skill"}), &default_ctx())
        .await
        .unwrap();
    let r2 = tool
        .execute(json!({"skill": "basic-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(r1.result.status, ToolStatus::Success);
    assert_eq!(r2.result.status, ToolStatus::Success);
}

// ---------------------------------------------------------------------------
// Deferred tool promotion tests
// ---------------------------------------------------------------------------

/// Without deferred-tools plugin active (no DeferralState in snapshot),
/// skill activation should NOT attempt to write DeferralState.
#[tokio::test]
async fn activate_tool_command_no_promote_without_deferred_tools_plugin() {
    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_TOOLS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    // default_ctx() has no DeferralState seeded — simulates no deferred-tools plugin
    let output = tool
        .execute(json!({"skill": "tool-skill"}), &default_ctx())
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    // Should NOT touch deferred_tools.state since plugin is not active
    assert!(
        !output
            .command
            .touched_keys
            .contains(&"deferred_tools.state".to_string()),
        "should not write deferred_tools.state when deferred-tools plugin is not active"
    );
}

/// With DeferralState seeded in the snapshot (deferred-tools plugin active),
/// skill activation should promote allowed_tools.
#[tokio::test]
async fn activate_tool_command_promotes_deferred_tools_when_plugin_active() {
    use awaken_contract::state::{Snapshot, StateMap};
    use awaken_ext_deferred_tools::config::ToolLoadMode;
    use awaken_ext_deferred_tools::state::{DeferralState, DeferralStateValue};
    use std::sync::Arc;

    let registry = make_registry_with_skills(&[EmbeddedSkillData {
        skill_md: SKILL_WITH_TOOLS_MD,
        references: &[],
        assets: &[],
    }]);
    let tool = SkillActivateTool::new(registry);

    // Create a context with DeferralState seeded (simulates deferred-tools plugin active)
    let mut state_map = StateMap::default();
    state_map.insert::<DeferralState>(DeferralStateValue::default());
    let snapshot = Snapshot::new(0, Arc::new(state_map));
    let ctx = ToolCallContext {
        snapshot,
        ..ToolCallContext::test_default()
    };

    let output = tool
        .execute(json!({"skill": "tool-skill"}), &ctx)
        .await
        .unwrap();

    assert_eq!(output.result.status, ToolStatus::Success);
    assert!(
        output
            .command
            .touched_keys
            .contains(&"deferred_tools.state".to_string()),
        "command should write deferred_tools.state to promote allowed_tools"
    );

    // Apply and verify
    let mut snapshot = Snapshot::new(0, Arc::new(StateMap::default()));
    for op in output.command.patch.ops {
        op.apply(&mut snapshot);
    }

    let deferral = snapshot.get::<DeferralState>().unwrap();
    assert_eq!(
        deferral.modes.get("Bash").copied(),
        Some(ToolLoadMode::Eager),
        "Bash should be promoted to Eager"
    );
    assert_eq!(
        deferral.modes.get("Read").copied(),
        Some(ToolLoadMode::Eager),
        "Read should be promoted to Eager"
    );
    assert_eq!(
        deferral.modes.get("Edit").copied(),
        Some(ToolLoadMode::Eager),
        "Edit should be promoted to Eager"
    );
}
