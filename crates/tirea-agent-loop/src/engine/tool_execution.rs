//! Tool execution utilities.

use crate::contracts::reduce_state_actions;
use crate::contracts::runtime::plugin::agent::AgentBehavior;
use crate::contracts::runtime::tool_call::ToolCallContext;
use crate::contracts::runtime::tool_call::{Tool, ToolExecutionEffect, ToolResult};
pub use crate::contracts::runtime::ToolExecution;
use crate::contracts::thread::ToolCall;
use futures::future::join_all;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tirea_contract::RunConfig;
use tirea_state::{apply_patch, DocCell, Patch, TrackedPatch};

const DIRECT_STATE_WRITE_DENIED_ERROR_CODE: &str = "tool_context_state_write_not_allowed";
const PLUGIN_ACTIONS_UNSUPPORTED_ERROR_CODE: &str = "tool_plugin_actions_not_supported";

pub(crate) fn merge_context_patch_into_effect(
    call: &ToolCall,
    _effect: &mut ToolExecutionEffect,
    context_patch: TrackedPatch,
) -> Result<(), ToolResult> {
    if context_patch.patch().is_empty() {
        return Ok(());
    }

    // No compatibility mode: tool-side direct state writes are always rejected.
    Err(ToolResult::error_with_code(
        &call.name,
        DIRECT_STATE_WRITE_DENIED_ERROR_CODE,
        "direct ToolCallContext state writes are disabled; emit ToolExecutionEffect actions instead",
    ))
}

/// Execute a single tool call.
///
/// This function:
/// 1. Creates a Context from the state snapshot
/// 2. Executes the tool
/// 3. Extracts any state changes as a TrackedPatch
///
/// # Arguments
///
/// * `tool` - The tool to execute (or None if not found)
/// * `call` - The tool call with id, name, and arguments
/// * `state` - The current state snapshot (read-only)
pub async fn execute_single_tool(
    tool: Option<&dyn Tool>,
    call: &ToolCall,
    state: &Value,
) -> ToolExecution {
    execute_single_tool_with_scope_and_behavior(tool, call, state, None, None).await
}

/// Execute a single tool call with an optional scope context.
pub async fn execute_single_tool_with_scope(
    tool: Option<&dyn Tool>,
    call: &ToolCall,
    state: &Value,
    scope: Option<&RunConfig>,
) -> ToolExecution {
    execute_single_tool_with_scope_and_behavior(tool, call, state, scope, None).await
}

/// Execute a single tool call with optional scope and behavior router.
pub async fn execute_single_tool_with_scope_and_behavior(
    tool: Option<&dyn Tool>,
    call: &ToolCall,
    state: &Value,
    scope: Option<&RunConfig>,
    behavior: Option<&dyn AgentBehavior>,
) -> ToolExecution {
    let Some(tool) = tool else {
        return ToolExecution {
            call: call.clone(),
            result: ToolResult::error(&call.name, format!("Tool '{}' not found", call.name)),
            patch: None,
        };
    };

    // Create context for this tool call
    let doc = DocCell::new(state.clone());
    let ops = Mutex::new(Vec::new());
    let default_scope = RunConfig::default();
    let scope = scope.unwrap_or(&default_scope);
    let pending_messages = Mutex::new(Vec::new());
    let ctx = ToolCallContext::new(
        &doc,
        &ops,
        &call.id,
        format!("tool:{}", call.name),
        scope,
        &pending_messages,
        tirea_contract::runtime::activity::NoOpActivityManager::arc(),
    );

    // Validate arguments against the tool's JSON Schema
    if let Err(e) = tool.validate_args(&call.arguments) {
        return ToolExecution {
            call: call.clone(),
            result: ToolResult::error(&call.name, e.to_string()),
            patch: None,
        };
    }

    // Execute the tool
    let mut effect = match tool.execute_effect(call.arguments.clone(), &ctx).await {
        Ok(effect) => effect,
        Err(e) => ToolExecutionEffect::from(ToolResult::error(&call.name, e.to_string())),
    };

    let context_patch = ctx.take_patch();
    if let Err(result) = merge_context_patch_into_effect(call, &mut effect, context_patch) {
        return ToolExecution {
            call: call.clone(),
            result,
            patch: None,
        };
    }
    let (result, state_actions, plugin_actions, _user_messages) = effect.into_parts();

    let action_patches =
        match reduce_state_actions(state_actions, state, &format!("tool:{}", call.name)) {
            Ok(patches) => patches,
            Err(err) => {
                return ToolExecution {
                    call: call.clone(),
                    result: ToolResult::error(
                        &call.name,
                        format!("tool state action reduce failed: {err}"),
                    ),
                    patch: None,
                };
            }
        };

    let mut merged_patch = Patch::new();
    for tracked in action_patches {
        merged_patch.extend(tracked.patch().clone());
    }

    if !plugin_actions.is_empty() {
        let Some(behavior) = behavior else {
            return ToolExecution {
                call: call.clone(),
                result: ToolResult::error_with_code(
                    &call.name,
                    PLUGIN_ACTIONS_UNSUPPORTED_ERROR_CODE,
                    "tool returned plugin actions but this execution path has no plugin behavior router",
                ),
                patch: None,
            };
        };

        let plugin_patches = match behavior.reduce_plugin_actions(plugin_actions, state) {
            Ok(patches) => patches,
            Err(err) => {
                return ToolExecution {
                    call: call.clone(),
                    result: ToolResult::error(
                        &call.name,
                        format!("tool plugin action reduce failed: {err}"),
                    ),
                    patch: None,
                };
            }
        };
        for tracked in plugin_patches {
            merged_patch.extend(tracked.patch().clone());
        }
    }

    let patch = if merged_patch.is_empty() {
        None
    } else {
        Some(TrackedPatch::new(merged_patch).with_source(format!("tool:{}", call.name)))
    };

    ToolExecution {
        call: call.clone(),
        result,
        patch,
    }
}

/// Execute tool calls in parallel using the same state snapshot for every call.
pub async fn execute_tools_parallel(
    tools: &HashMap<String, Arc<dyn Tool>>,
    calls: &[ToolCall],
    state: &Value,
) -> Vec<ToolExecution> {
    let tasks = calls.iter().map(|call| {
        let tool = tools.get(&call.name).cloned();
        let state = state.clone();
        async move { execute_single_tool(tool.as_deref(), call, &state).await }
    });
    join_all(tasks).await
}

/// Execute tool calls sequentially, applying each resulting patch before the next call.
pub async fn execute_tools_sequential(
    tools: &HashMap<String, Arc<dyn Tool>>,
    calls: &[ToolCall],
    state: &Value,
) -> (Value, Vec<ToolExecution>) {
    let mut state = state.clone();
    let mut executions = Vec::with_capacity(calls.len());

    for call in calls {
        let exec = execute_single_tool(tools.get(&call.name).map(Arc::as_ref), call, &state).await;
        if let Some(patch) = exec.patch.as_ref() {
            if let Ok(next) = apply_patch(&state, patch.patch()) {
                state = next;
            }
        }
        executions.push(exec);
    }

    (state, executions)
}

/// Collect patches from executions.
pub fn collect_patches(executions: &[ToolExecution]) -> Vec<TrackedPatch> {
    executions.iter().filter_map(|e| e.patch.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::plugin::phase::state_spec::StateSpec;
    use crate::contracts::runtime::plugin::phase::AnyStateAction;
    use crate::contracts::runtime::tool_call::{ToolDescriptor, ToolError};
    use crate::contracts::runtime::{InferenceError, InferenceErrorState};
    use crate::contracts::ToolCallContext;
    use async_trait::async_trait;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use tirea_state::{PatchSink, Path as TPath, State, TireaResult};

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "Echo", "Echo the input")
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("echo", args))
        }
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    struct EffectCounterState {
        value: i64,
    }

    struct EffectCounterRef;

    impl State for EffectCounterState {
        type Ref<'a> = EffectCounterRef;
        const PATH: &'static str = "counter";

        fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
            EffectCounterRef
        }

        fn from_value(value: &Value) -> TireaResult<Self> {
            if value.is_null() {
                return Ok(Self::default());
            }
            serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
        }

        fn to_value(&self) -> TireaResult<Value> {
            serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
        }
    }

    impl StateSpec for EffectCounterState {
        type Action = i64;

        fn reduce(&mut self, action: Self::Action) {
            self.value += action;
        }
    }

    struct EffectTool;

    #[async_trait]
    impl Tool for EffectTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("effect", "Effect", "Tool returning state actions")
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("effect", json!({})))
        }

        async fn execute_effect(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<crate::contracts::runtime::ToolExecutionEffect, ToolError> {
            Ok(
                crate::contracts::runtime::ToolExecutionEffect::new(ToolResult::success(
                    "effect",
                    json!({}),
                ))
                .with_state_action(AnyStateAction::new::<EffectCounterState>(2)),
            )
        }
    }

    struct DirectWriteEffectTool;

    #[async_trait]
    impl Tool for DirectWriteEffectTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                "direct_write_effect",
                "DirectWriteEffect",
                "writes state directly in execute_effect",
            )
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                "direct_write_effect",
                json!({"ok": true}),
            ))
        }

        async fn execute_effect(
            &self,
            _args: Value,
            ctx: &ToolCallContext<'_>,
        ) -> Result<crate::contracts::runtime::ToolExecutionEffect, ToolError> {
            let err = ctx.state_of::<InferenceErrorState>();
            err.set_error(Some(InferenceError {
                error_type: "direct_write".to_string(),
                message: "written via execute_effect context".to_string(),
            }))
            .expect("failed to set inference error");
            Ok(crate::contracts::runtime::ToolExecutionEffect::new(
                ToolResult::success("direct_write_effect", json!({"ok": true})),
            ))
        }
    }

    #[tokio::test]
    async fn test_execute_single_tool_not_found() {
        let call = ToolCall::new("call_1", "nonexistent", json!({}));
        let state = json!({});

        let exec = execute_single_tool(None, &call, &state).await;

        assert!(exec.result.is_error());
        assert!(exec.patch.is_none());
    }

    #[tokio::test]
    async fn test_execute_single_tool_success() {
        let tool = EchoTool;
        let call = ToolCall::new("call_1", "echo", json!({"msg": "hello"}));
        let state = json!({});

        let exec = execute_single_tool(Some(&tool), &call, &state).await;

        assert!(exec.result.is_success());
        assert_eq!(exec.result.data["msg"], "hello");
    }

    #[tokio::test]
    async fn test_execute_single_tool_applies_state_actions_from_effect() {
        let tool = EffectTool;
        let call = ToolCall::new("call_1", "effect", json!({}));
        let state = json!({"counter": {"value": 1}});

        let exec = execute_single_tool(Some(&tool), &call, &state).await;
        let patch = exec.patch.expect("patch should be emitted");
        let next = apply_patch(&state, patch.patch()).expect("patch should apply");

        assert_eq!(next["counter"]["value"], 3);
    }

    #[tokio::test]
    async fn test_execute_single_tool_rejects_direct_context_writes_in_strict_mode() {
        let tool = DirectWriteEffectTool;
        let call = ToolCall::new("call_1", "direct_write_effect", json!({}));
        let state = json!({});
        let scope = RunConfig::default();

        let exec = execute_single_tool_with_scope(Some(&tool), &call, &state, Some(&scope)).await;
        assert!(exec.result.is_error());
        assert_eq!(
            exec.result.data["error"]["code"],
            json!("tool_context_state_write_not_allowed")
        );
        assert!(exec.patch.is_none());
    }

    #[tokio::test]
    async fn test_collect_patches() {
        use tirea_state::{path, Op, Patch};

        let executions = vec![
            ToolExecution {
                call: ToolCall::new("1", "a", json!({})),
                result: ToolResult::success("a", json!({})),
                patch: Some(TrackedPatch::new(
                    Patch::new().with_op(Op::set(path!("a"), json!(1))),
                )),
            },
            ToolExecution {
                call: ToolCall::new("2", "b", json!({})),
                result: ToolResult::success("b", json!({})),
                patch: None,
            },
            ToolExecution {
                call: ToolCall::new("3", "c", json!({})),
                result: ToolResult::success("c", json!({})),
                patch: Some(TrackedPatch::new(
                    Patch::new().with_op(Op::set(path!("c"), json!(3))),
                )),
            },
        ];

        let patches = collect_patches(&executions);
        assert_eq!(patches.len(), 2);
    }

    #[tokio::test]
    async fn test_tool_execution_error() {
        struct FailingTool;

        #[async_trait]
        impl Tool for FailingTool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("failing", "Failing", "Always fails")
            }

            async fn execute(
                &self,
                _args: Value,
                _ctx: &ToolCallContext<'_>,
            ) -> Result<ToolResult, ToolError> {
                Err(ToolError::ExecutionFailed(
                    "Intentional failure".to_string(),
                ))
            }
        }

        let tool = FailingTool;
        let call = ToolCall::new("call_1", "failing", json!({}));
        let state = json!({});

        let exec = execute_single_tool(Some(&tool), &call, &state).await;

        assert!(exec.result.is_error());
        assert!(exec
            .result
            .message
            .as_ref()
            .unwrap()
            .contains("Intentional failure"));
    }

    #[tokio::test]
    async fn test_execute_single_tool_with_scope_reads() {
        /// Tool that reads user_id from scope and returns it.
        struct ScopeReaderTool;

        #[async_trait]
        impl Tool for ScopeReaderTool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("scope_reader", "ScopeReader", "Reads scope values")
            }

            async fn execute(
                &self,
                _args: Value,
                ctx: &ToolCallContext<'_>,
            ) -> Result<ToolResult, ToolError> {
                let user_id = ctx
                    .config_value("user_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                Ok(ToolResult::success(
                    "scope_reader",
                    json!({"user_id": user_id}),
                ))
            }
        }

        let mut scope = RunConfig::new();
        scope.set("user_id", "u-42").unwrap();

        let tool = ScopeReaderTool;
        let call = ToolCall::new("call_1", "scope_reader", json!({}));
        let state = json!({});

        let exec = execute_single_tool_with_scope(Some(&tool), &call, &state, Some(&scope)).await;

        assert!(exec.result.is_success());
        assert_eq!(exec.result.data["user_id"], "u-42");
    }

    #[tokio::test]
    async fn test_execute_single_tool_with_scope_none() {
        /// Tool that checks scope_ref is None.
        struct ScopeCheckerTool;

        #[async_trait]
        impl Tool for ScopeCheckerTool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("scope_checker", "ScopeChecker", "Checks scope presence")
            }

            async fn execute(
                &self,
                _args: Value,
                ctx: &ToolCallContext<'_>,
            ) -> Result<ToolResult, ToolError> {
                // ToolCallContext always provides a scope reference (never None).
                // We verify scope access works by probing for a known key.
                let has_user_id = ctx.config_value("user_id").is_some();
                Ok(ToolResult::success(
                    "scope_checker",
                    json!({"has_scope": true, "has_user_id": has_user_id}),
                ))
            }
        }

        let tool = ScopeCheckerTool;
        let call = ToolCall::new("call_1", "scope_checker", json!({}));
        let state = json!({});

        // Without scope — ToolCallContext still provides a (default-empty) scope
        let exec = execute_single_tool_with_scope(Some(&tool), &call, &state, None).await;
        assert_eq!(exec.result.data["has_scope"], true);
        assert_eq!(exec.result.data["has_user_id"], false);

        // With scope (empty)
        let scope = RunConfig::new();
        let exec = execute_single_tool_with_scope(Some(&tool), &call, &state, Some(&scope)).await;
        assert_eq!(exec.result.data["has_scope"], true);
        assert_eq!(exec.result.data["has_user_id"], false);
    }

    #[tokio::test]
    async fn test_execute_with_scope_sensitive_key() {
        /// Tool that reads a sensitive key from scope.
        struct SensitiveReaderTool;

        #[async_trait]
        impl Tool for SensitiveReaderTool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("sensitive", "Sensitive", "Reads sensitive key")
            }

            async fn execute(
                &self,
                _args: Value,
                ctx: &ToolCallContext<'_>,
            ) -> Result<ToolResult, ToolError> {
                let scope = ctx.run_config();
                let token = scope.value("token").and_then(|v| v.as_str()).unwrap();
                let is_sensitive = scope.is_sensitive("token");
                Ok(ToolResult::success(
                    "sensitive",
                    json!({"token_len": token.len(), "is_sensitive": is_sensitive}),
                ))
            }
        }

        let mut scope = RunConfig::new();
        scope.set_sensitive("token", "super-secret-token").unwrap();

        let tool = SensitiveReaderTool;
        let call = ToolCall::new("call_1", "sensitive", json!({}));
        let state = json!({});

        let exec = execute_single_tool_with_scope(Some(&tool), &call, &state, Some(&scope)).await;

        assert!(exec.result.is_success());
        assert_eq!(exec.result.data["token_len"], 18);
        assert_eq!(exec.result.data["is_sensitive"], true);
    }

    // =========================================================================
    // validate_args integration: strict schema blocks invalid args at exec path
    // =========================================================================

    /// Tool with a strict schema — execute should never be reached on invalid args.
    struct StrictSchemaTool {
        executed: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Tool for StrictSchemaTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("strict", "Strict", "Requires a string 'name'").with_parameters(
                json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" }
                    },
                    "required": ["name"]
                }),
            )
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            self.executed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolResult::success("strict", args))
        }
    }

    #[tokio::test]
    async fn test_validate_args_blocks_invalid_before_execute() {
        let tool = StrictSchemaTool {
            executed: std::sync::atomic::AtomicBool::new(false),
        };
        // Missing required "name" field
        let call = ToolCall::new("call_1", "strict", json!({}));
        let state = json!({});

        let exec = execute_single_tool(Some(&tool), &call, &state).await;

        assert!(exec.result.is_error());
        assert!(
            exec.result.message.as_ref().unwrap().contains("name"),
            "error should mention the missing field"
        );
        assert!(
            !tool.executed.load(std::sync::atomic::Ordering::SeqCst),
            "execute() must NOT be called when validate_args fails"
        );
    }

    #[tokio::test]
    async fn test_validate_args_passes_valid_to_execute() {
        let tool = StrictSchemaTool {
            executed: std::sync::atomic::AtomicBool::new(false),
        };
        let call = ToolCall::new("call_1", "strict", json!({"name": "Alice"}));
        let state = json!({});

        let exec = execute_single_tool(Some(&tool), &call, &state).await;

        assert!(exec.result.is_success());
        assert!(
            tool.executed.load(std::sync::atomic::Ordering::SeqCst),
            "execute() should be called for valid args"
        );
    }

    #[tokio::test]
    async fn test_validate_args_wrong_type_blocks_execute() {
        let tool = StrictSchemaTool {
            executed: std::sync::atomic::AtomicBool::new(false),
        };
        // "name" should be string, not integer
        let call = ToolCall::new("call_1", "strict", json!({"name": 42}));
        let state = json!({});

        let exec = execute_single_tool(Some(&tool), &call, &state).await;

        assert!(exec.result.is_error());
        assert!(
            !tool.executed.load(std::sync::atomic::Ordering::SeqCst),
            "execute() must NOT be called when validate_args fails"
        );
    }
}
