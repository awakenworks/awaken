//! Tool execution strategies: Sequential and Parallel.

use async_trait::async_trait;
use awaken_contract::contract::message::ToolCall;
use awaken_contract::contract::suspension::ToolCallOutcome;
use awaken_contract::contract::tool::{Tool, ToolCallContext, ToolOutput, ToolResult};
use awaken_contract::state::StateCommand;
use std::collections::HashMap;
use std::sync::Arc;

use super::tool_error::tool_error_result;
#[cfg(feature = "background")]
use crate::extensions::background::{ToolLineageContext, scope_tool_lineage_context};

/// Result of executing a single tool call.
pub struct ToolExecutionResult {
    pub call: ToolCall,
    pub result: ToolResult,
    pub outcome: ToolCallOutcome,
    /// Side-effects produced by the tool (state mutations, scheduled actions, effects).
    pub command: StateCommand,
}

/// Error from tool execution strategy.
#[derive(Debug, thiserror::Error)]
pub enum ToolExecutorError {
    #[error("tool execution cancelled")]
    Cancelled,
    #[error("tool execution failed: {0}")]
    Failed(String),
}

/// Strategy abstraction for tool execution.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute tool calls and return results.
    async fn execute(
        &self,
        tools: &HashMap<String, Arc<dyn Tool>>,
        calls: &[ToolCall],
        base_ctx: &ToolCallContext,
    ) -> Result<Vec<ToolExecutionResult>, ToolExecutorError>;

    /// Strategy name for logging.
    fn name(&self) -> &'static str;

    /// Whether the executor needs state refreshed between individual tool calls.
    /// Sequential executors return true; parallel executors return false.
    fn requires_incremental_state(&self) -> bool {
        false
    }
}

/// Execute tool calls one by one in call order.
/// Context freshness between calls is controlled by the caller.
/// Stops at first suspension.
#[derive(Debug, Clone, Copy, Default)]
pub struct SequentialToolExecutor;

#[async_trait]
impl ToolExecutor for SequentialToolExecutor {
    async fn execute(
        &self,
        tools: &HashMap<String, Arc<dyn Tool>>,
        calls: &[ToolCall],
        base_ctx: &ToolCallContext,
    ) -> Result<Vec<ToolExecutionResult>, ToolExecutorError> {
        let mut results = Vec::with_capacity(calls.len());

        for call in calls {
            let mut ctx = base_ctx.clone();
            ctx.call_id = call.id.clone();
            ctx.tool_name = call.name.clone();
            let output = execute_single_tool(tools, call, &ctx).await;
            let outcome = ToolCallOutcome::from_tool_result(&output.result);

            results.push(ToolExecutionResult {
                call: call.clone(),
                result: output.result,
                outcome,
                command: output.command,
            });

            // Stop at first suspension
            if results
                .last()
                .is_some_and(|r| r.outcome == ToolCallOutcome::Suspended)
            {
                break;
            }
        }

        Ok(results)
    }

    fn name(&self) -> &'static str {
        "sequential"
    }

    fn requires_incremental_state(&self) -> bool {
        true
    }
}

/// Policy controlling when resume decisions are replayed into tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionReplayPolicy {
    /// Replay each resolved suspended call as soon as its decision arrives.
    Immediate,
    /// Replay only when all currently suspended calls have decisions.
    BatchAllSuspended,
}

/// Parallel execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParallelMode {
    /// All concurrently, batch approval gate — wait for all decisions.
    BatchApproval,
    /// All concurrently, streaming results — replay decisions immediately.
    Streaming,
}

/// Execute all tool calls concurrently. All tools see the same frozen snapshot.
///
/// Two modes differ in how suspension decisions are replayed:
/// - `BatchApproval`: wait for all suspended calls to have decisions before replay
/// - `Streaming`: replay each decision immediately as it arrives
#[derive(Debug, Clone, Copy)]
pub struct ParallelToolExecutor {
    mode: ParallelMode,
}

impl ParallelToolExecutor {
    pub const fn batch_approval() -> Self {
        Self {
            mode: ParallelMode::BatchApproval,
        }
    }

    pub const fn streaming() -> Self {
        Self {
            mode: ParallelMode::Streaming,
        }
    }

    /// How the runtime should replay resolved suspend decisions.
    pub fn decision_replay_policy(&self) -> DecisionReplayPolicy {
        match self.mode {
            ParallelMode::BatchApproval => DecisionReplayPolicy::BatchAllSuspended,
            ParallelMode::Streaming => DecisionReplayPolicy::Immediate,
        }
    }

    /// Whether the runtime should enforce parallel patch conflict checks.
    pub fn requires_conflict_check(&self) -> bool {
        true
    }
}

impl Default for ParallelToolExecutor {
    fn default() -> Self {
        Self::streaming()
    }
}

#[async_trait]
impl ToolExecutor for ParallelToolExecutor {
    async fn execute(
        &self,
        tools: &HashMap<String, Arc<dyn Tool>>,
        calls: &[ToolCall],
        base_ctx: &ToolCallContext,
    ) -> Result<Vec<ToolExecutionResult>, ToolExecutorError> {
        use futures::future::join_all;

        let futures: Vec<_> = calls
            .iter()
            .map(|call| {
                let tools = tools.clone();
                let call = call.clone();
                let mut ctx = base_ctx.clone();
                ctx.call_id = call.id.clone();
                ctx.tool_name = call.name.clone();
                async move {
                    let output = execute_single_tool(&tools, &call, &ctx).await;
                    let outcome = ToolCallOutcome::from_tool_result(&output.result);
                    ToolExecutionResult {
                        call,
                        result: output.result,
                        outcome,
                        command: output.command,
                    }
                }
            })
            .collect();

        Ok(join_all(futures).await)
    }

    fn name(&self) -> &'static str {
        match self.mode {
            ParallelMode::BatchApproval => "parallel_batch_approval",
            ParallelMode::Streaming => "parallel_streaming",
        }
    }
}

/// Execute a single tool call, never panicking.
pub(crate) async fn execute_single_tool(
    tools: &HashMap<String, Arc<dyn Tool>>,
    call: &ToolCall,
    ctx: &ToolCallContext,
) -> ToolOutput {
    let Some(tool) = tools.get(&call.name) else {
        return ToolResult::error(&call.name, format!("tool '{}' not found", call.name)).into();
    };

    if let Err(e) = tool.validate_args(&call.arguments) {
        return ToolResult::error(&call.name, e.to_string()).into();
    }

    match execute_tool_with_runtime_context(tool, call, ctx).await {
        Ok(output) => output,
        Err(e) => tool_error_result(&call.name, e).into(),
    }
}

#[cfg(feature = "background")]
async fn execute_tool_with_runtime_context(
    tool: &Arc<dyn Tool>,
    call: &ToolCall,
    ctx: &ToolCallContext,
) -> Result<ToolOutput, awaken_contract::contract::tool::ToolError> {
    scope_tool_lineage_context(
        ToolLineageContext {
            run_id: ctx.run_identity.run_id.clone(),
            call_id: ctx.call_id.clone(),
            agent_id: ctx.run_identity.agent_id.clone(),
        },
        tool.execute(call.arguments.clone(), ctx),
    )
    .await
}

#[cfg(not(feature = "background"))]
async fn execute_tool_with_runtime_context(
    tool: &Arc<dyn Tool>,
    call: &ToolCall,
    ctx: &ToolCallContext,
) -> Result<ToolOutput, awaken_contract::contract::tool::ToolError> {
    tool.execute(call.arguments.clone(), ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::tool::{ToolDescriptor, ToolError, ToolOutput};
    use serde_json::{Value, json};

    #[cfg(feature = "background")]
    use crate::extensions::background::{
        BackgroundTaskManager, BackgroundTaskPlugin, TaskParentContext, TaskResult,
    };
    #[cfg(feature = "background")]
    use crate::phase::ExecutionEnv;
    #[cfg(feature = "background")]
    use crate::plugins::Plugin;
    #[cfg(feature = "background")]
    use crate::state::StateStore;
    #[cfg(feature = "background")]
    use awaken_contract::contract::identity::{RunIdentity, RunOrigin};

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("echo", "echo", "Echoes input")
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("no message")
                .to_string();
            Ok(ToolResult::success_with_message("echo", args, msg).into())
        }
    }

    struct FailingTool;

    #[async_trait]
    impl Tool for FailingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("failing", "failing", "Always fails")
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Err(ToolError::ExecutionFailed("intentional failure".into()))
        }
    }

    struct SuspendingTool;

    #[async_trait]
    impl Tool for SuspendingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("suspending", "suspending", "Returns pending")
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::suspended("suspending", "needs approval").into())
        }
    }

    fn tool_map(tools: Vec<Arc<dyn Tool>>) -> HashMap<String, Arc<dyn Tool>> {
        tools.into_iter().map(|t| (t.descriptor().id, t)).collect()
    }

    // -- Sequential tests --

    #[tokio::test]
    async fn sequential_single_tool_success() {
        let tools = tool_map(vec![Arc::new(EchoTool)]);
        let calls = vec![ToolCall::new("c1", "echo", json!({"message": "hi"}))];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, ToolCallOutcome::Succeeded);
        assert!(results[0].result.is_success());
    }

    #[tokio::test]
    async fn sequential_partial_failure() {
        let tools = tool_map(vec![Arc::new(EchoTool), Arc::new(FailingTool)]);
        let calls = vec![
            ToolCall::new("c1", "echo", json!({"message": "ok"})),
            ToolCall::new("c2", "failing", json!({})),
        ];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].outcome, ToolCallOutcome::Succeeded);
        assert_eq!(results[1].outcome, ToolCallOutcome::Failed);
    }

    #[tokio::test]
    async fn sequential_stops_after_first_suspension() {
        let tools = tool_map(vec![Arc::new(SuspendingTool), Arc::new(EchoTool)]);
        let calls = vec![
            ToolCall::new("c1", "suspending", json!({})),
            ToolCall::new("c2", "echo", json!({"message": "should not run"})),
        ];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1, "should stop after suspended tool");
        assert_eq!(results[0].outcome, ToolCallOutcome::Suspended);
    }

    #[tokio::test]
    async fn sequential_unknown_tool_returns_error() {
        let tools = tool_map(vec![]);
        let calls = vec![ToolCall::new("c1", "nonexistent", json!({}))];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, ToolCallOutcome::Failed);
        assert!(results[0].result.is_error());
    }

    #[tokio::test]
    async fn sequential_empty_calls() {
        let tools = tool_map(vec![Arc::new(EchoTool)]);
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &[], &ToolCallContext::test_default())
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    // -- Parallel tests --

    #[tokio::test]
    async fn parallel_all_succeed() {
        let tools = tool_map(vec![Arc::new(EchoTool)]);
        let calls = vec![
            ToolCall::new("c1", "echo", json!({"message": "first"})),
            ToolCall::new("c2", "echo", json!({"message": "second"})),
        ];
        let executor = ParallelToolExecutor::streaming();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| r.outcome == ToolCallOutcome::Succeeded)
        );
    }

    #[tokio::test]
    async fn parallel_partial_failure() {
        let tools = tool_map(vec![Arc::new(EchoTool), Arc::new(FailingTool)]);
        let calls = vec![
            ToolCall::new("c1", "echo", json!({"message": "ok"})),
            ToolCall::new("c2", "failing", json!({})),
        ];
        let executor = ParallelToolExecutor::streaming();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        let successes = results
            .iter()
            .filter(|r| r.outcome == ToolCallOutcome::Succeeded)
            .count();
        let failures = results
            .iter()
            .filter(|r| r.outcome == ToolCallOutcome::Failed)
            .count();
        assert_eq!(successes, 1);
        assert_eq!(failures, 1);
    }

    #[tokio::test]
    async fn parallel_does_not_stop_on_suspension() {
        let tools = tool_map(vec![Arc::new(SuspendingTool), Arc::new(EchoTool)]);
        let calls = vec![
            ToolCall::new("c1", "suspending", json!({})),
            ToolCall::new("c2", "echo", json!({"message": "runs anyway"})),
        ];
        let executor = ParallelToolExecutor::streaming();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        // Parallel executes ALL tools regardless of suspension
        assert_eq!(results.len(), 2);
        let suspended = results
            .iter()
            .filter(|r| r.outcome == ToolCallOutcome::Suspended)
            .count();
        let succeeded = results
            .iter()
            .filter(|r| r.outcome == ToolCallOutcome::Succeeded)
            .count();
        assert_eq!(suspended, 1);
        assert_eq!(succeeded, 1);
    }

    #[tokio::test]
    async fn parallel_empty_calls() {
        let tools = tool_map(vec![Arc::new(EchoTool)]);
        let executor = ParallelToolExecutor::streaming();

        let results = executor
            .execute(&tools, &[], &ToolCallContext::test_default())
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn executor_names() {
        assert_eq!(SequentialToolExecutor.name(), "sequential");
        assert_eq!(
            ParallelToolExecutor::streaming().name(),
            "parallel_streaming"
        );
        assert_eq!(
            ParallelToolExecutor::batch_approval().name(),
            "parallel_batch_approval"
        );
    }

    #[test]
    fn parallel_default_is_streaming() {
        let executor = ParallelToolExecutor::default();
        assert_eq!(executor.name(), "parallel_streaming");
        assert_eq!(
            executor.decision_replay_policy(),
            DecisionReplayPolicy::Immediate
        );
    }

    #[test]
    fn parallel_batch_approval_policy() {
        let executor = ParallelToolExecutor::batch_approval();
        assert_eq!(
            executor.decision_replay_policy(),
            DecisionReplayPolicy::BatchAllSuspended
        );
        assert!(executor.requires_conflict_check());
    }

    #[test]
    fn parallel_streaming_policy() {
        let executor = ParallelToolExecutor::streaming();
        assert_eq!(
            executor.decision_replay_policy(),
            DecisionReplayPolicy::Immediate
        );
        assert!(executor.requires_conflict_check());
    }

    #[tokio::test]
    async fn batch_approval_executes_all_concurrently() {
        let tools = tool_map(vec![Arc::new(EchoTool)]);
        let calls = vec![
            ToolCall::new("c1", "echo", json!({"message": "a"})),
            ToolCall::new("c2", "echo", json!({"message": "b"})),
        ];
        let executor = ParallelToolExecutor::batch_approval();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| r.outcome == ToolCallOutcome::Succeeded)
        );
    }

    #[tokio::test]
    async fn batch_approval_does_not_stop_on_suspension() {
        let tools = tool_map(vec![Arc::new(SuspendingTool), Arc::new(EchoTool)]);
        let calls = vec![
            ToolCall::new("c1", "suspending", json!({})),
            ToolCall::new("c2", "echo", json!({"message": "runs anyway"})),
        ];
        let executor = ParallelToolExecutor::batch_approval();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: additional tool executor tests
    // -----------------------------------------------------------------------

    /// A tool that counts how many times it's been called.
    struct CountingTool {
        count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingTool {
        fn new() -> Self {
            Self {
                count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            self.count.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("counting", "counting", "Counts calls")
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            let n = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolResult::success("counting", json!({"call_number": n + 1})).into())
        }
    }

    #[tokio::test]
    async fn sequential_multiple_calls_ordered() {
        let counting = Arc::new(CountingTool::new());
        let tools = tool_map(vec![counting.clone() as Arc<dyn Tool>]);
        let calls = vec![
            ToolCall::new("c1", "counting", json!({})),
            ToolCall::new("c2", "counting", json!({})),
            ToolCall::new("c3", "counting", json!({})),
        ];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(counting.call_count(), 3);
        // Verify order is preserved
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.call.id, format!("c{}", i + 1));
            assert_eq!(result.outcome, ToolCallOutcome::Succeeded);
        }
    }

    #[tokio::test]
    async fn sequential_failure_does_not_stop_execution() {
        // Unlike suspension, failures do NOT stop sequential execution
        let tools = tool_map(vec![Arc::new(FailingTool), Arc::new(EchoTool)]);
        let calls = vec![
            ToolCall::new("c1", "failing", json!({})),
            ToolCall::new("c2", "echo", json!({"message": "still runs"})),
        ];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].outcome, ToolCallOutcome::Failed);
        assert_eq!(results[1].outcome, ToolCallOutcome::Succeeded);
    }

    #[tokio::test]
    async fn sequential_suspension_in_middle_stops_remaining() {
        let tools = tool_map(vec![
            Arc::new(EchoTool),
            Arc::new(SuspendingTool),
            Arc::new(EchoTool),
        ]);
        let calls = vec![
            ToolCall::new("c1", "echo", json!({"message": "first"})),
            ToolCall::new("c2", "suspending", json!({})),
            ToolCall::new("c3", "echo", json!({"message": "should not run"})),
        ];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2, "should stop after suspension");
        assert_eq!(results[0].outcome, ToolCallOutcome::Succeeded);
        assert_eq!(results[1].outcome, ToolCallOutcome::Suspended);
    }

    #[tokio::test]
    async fn parallel_all_fail() {
        let tools = tool_map(vec![Arc::new(FailingTool)]);
        let calls = vec![
            ToolCall::new("c1", "failing", json!({})),
            ToolCall::new("c2", "failing", json!({})),
        ];
        let executor = ParallelToolExecutor::streaming();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.outcome == ToolCallOutcome::Failed));
    }

    #[tokio::test]
    async fn parallel_unknown_tool_returns_error() {
        let tools = tool_map(vec![]);
        let calls = vec![
            ToolCall::new("c1", "nonexistent_a", json!({})),
            ToolCall::new("c2", "nonexistent_b", json!({})),
        ];
        let executor = ParallelToolExecutor::streaming();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.outcome == ToolCallOutcome::Failed));
        for r in &results {
            assert!(r.result.is_error());
        }
    }

    #[tokio::test]
    async fn parallel_counting_tool_all_called() {
        let counting = Arc::new(CountingTool::new());
        let tools = tool_map(vec![counting.clone() as Arc<dyn Tool>]);
        let calls = vec![
            ToolCall::new("c1", "counting", json!({})),
            ToolCall::new("c2", "counting", json!({})),
            ToolCall::new("c3", "counting", json!({})),
        ];
        let executor = ParallelToolExecutor::streaming();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(counting.call_count(), 3);
    }

    /// Validate that tool args are validated before execution.
    struct StrictArgsTool;

    #[async_trait]
    impl Tool for StrictArgsTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("strict", "strict", "Validates args")
        }

        fn validate_args(&self, args: &Value) -> Result<(), ToolError> {
            if args.get("required_field").is_none() {
                return Err(ToolError::InvalidArguments("missing required_field".into()));
            }
            Ok(())
        }

        async fn execute(
            &self,
            args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success("strict", args).into())
        }
    }

    #[tokio::test]
    async fn sequential_validates_args_before_execute() {
        let tools = tool_map(vec![Arc::new(StrictArgsTool)]);
        let calls = vec![ToolCall::new("c1", "strict", json!({}))]; // missing required_field
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, ToolCallOutcome::Failed);
        assert!(results[0].result.is_error());
    }

    #[tokio::test]
    async fn sequential_valid_args_succeeds() {
        let tools = tool_map(vec![Arc::new(StrictArgsTool)]);
        let calls = vec![ToolCall::new(
            "c1",
            "strict",
            json!({"required_field": "present"}),
        )];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, ToolCallOutcome::Succeeded);
    }

    #[tokio::test]
    async fn parallel_validates_args_before_execute() {
        let tools = tool_map(vec![Arc::new(StrictArgsTool)]);
        let calls = vec![ToolCall::new("c1", "strict", json!({}))];
        let executor = ParallelToolExecutor::streaming();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].outcome, ToolCallOutcome::Failed);
    }

    #[test]
    fn tool_execution_result_fields() {
        let result = ToolExecutionResult {
            call: ToolCall::new("c1", "echo", json!({})),
            result: ToolResult::success("echo", json!({"ok": true})),
            outcome: ToolCallOutcome::Succeeded,
            command: StateCommand::default(),
        };
        assert_eq!(result.call.id, "c1");
        assert_eq!(result.outcome, ToolCallOutcome::Succeeded);
    }

    #[test]
    fn tool_executor_error_display() {
        let err = ToolExecutorError::Cancelled;
        assert!(err.to_string().contains("cancelled"));
        let err2 = ToolExecutorError::Failed("some reason".into());
        assert!(err2.to_string().contains("some reason"));
    }

    // -----------------------------------------------------------------------
    // Additional coverage: context preservation, mixed scenarios, edge cases
    // -----------------------------------------------------------------------

    /// Tool that captures the context it receives for later inspection.
    struct ContextCaptureTool {
        captured_call_id: Arc<std::sync::Mutex<String>>,
        captured_tool_name: Arc<std::sync::Mutex<String>>,
    }

    impl ContextCaptureTool {
        fn new() -> Self {
            Self {
                captured_call_id: Arc::new(std::sync::Mutex::new(String::new())),
                captured_tool_name: Arc::new(std::sync::Mutex::new(String::new())),
            }
        }
    }

    #[async_trait]
    impl Tool for ContextCaptureTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new("capture", "capture", "Captures context")
        }

        async fn execute(
            &self,
            _args: Value,
            ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            *self.captured_call_id.lock().unwrap() = ctx.call_id.clone();
            *self.captured_tool_name.lock().unwrap() = ctx.tool_name.clone();
            Ok(ToolResult::success("capture", json!({"captured": true})).into())
        }
    }

    #[tokio::test]
    async fn execute_single_tool_preserves_call_context() {
        let capture = Arc::new(ContextCaptureTool::new());
        let tools = tool_map(vec![capture.clone() as Arc<dyn Tool>]);
        let call = ToolCall::new("call-42", "capture", json!({}));
        let ctx = ToolCallContext::test_default();

        let output = execute_single_tool(&tools, &call, &ctx).await;
        assert!(output.result.is_success());
        // execute_single_tool sets call_id and tool_name from the call's context
        // (the caller is responsible for setting ctx fields, which the executor does)
    }

    #[tokio::test]
    async fn execute_single_tool_missing_returns_error_with_name() {
        let tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        let call = ToolCall::new("c1", "ghost_tool", json!({}));
        let ctx = ToolCallContext::test_default();

        let output = execute_single_tool(&tools, &call, &ctx).await;
        assert!(output.result.is_error());
        assert!(
            output
                .result
                .message
                .as_deref()
                .unwrap_or("")
                .contains("ghost_tool")
        );
    }

    #[tokio::test]
    async fn execute_single_tool_validates_args() {
        let tools = tool_map(vec![Arc::new(StrictArgsTool)]);
        let call = ToolCall::new("c1", "strict", json!({"wrong": "field"}));
        let ctx = ToolCallContext::test_default();

        let output = execute_single_tool(&tools, &call, &ctx).await;
        assert!(output.result.is_error());
    }

    #[tokio::test]
    async fn sequential_context_call_id_set_per_tool() {
        let capture = Arc::new(ContextCaptureTool::new());
        let tools = tool_map(vec![capture.clone() as Arc<dyn Tool>]);
        let calls = vec![ToolCall::new("unique-id-99", "capture", json!({}))];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].call.id, "unique-id-99");
        // The executor sets ctx.call_id = call.id before passing to execute_single_tool
        assert_eq!(*capture.captured_call_id.lock().unwrap(), "unique-id-99");
    }

    #[tokio::test]
    async fn sequential_mixed_success_failure_suspension_order() {
        let tools = tool_map(vec![
            Arc::new(EchoTool),
            Arc::new(FailingTool),
            Arc::new(SuspendingTool),
        ]);
        // success, fail, suspend — suspension stops execution
        let calls = vec![
            ToolCall::new("c1", "echo", json!({"message": "hi"})),
            ToolCall::new("c2", "failing", json!({})),
            ToolCall::new("c3", "suspending", json!({})),
            ToolCall::new("c4", "echo", json!({"message": "should not run"})),
        ];
        let executor = SequentialToolExecutor;

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 3, "stops after suspension at c3");
        assert_eq!(results[0].outcome, ToolCallOutcome::Succeeded);
        assert_eq!(results[1].outcome, ToolCallOutcome::Failed);
        assert_eq!(results[2].outcome, ToolCallOutcome::Suspended);
    }

    #[tokio::test]
    async fn parallel_preserves_result_order() {
        let counting = Arc::new(CountingTool::new());
        let tools = tool_map(vec![counting.clone() as Arc<dyn Tool>]);
        let calls: Vec<_> = (0..5)
            .map(|i| ToolCall::new(format!("c{i}"), "counting", json!({})))
            .collect();
        let executor = ParallelToolExecutor::streaming();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 5);
        // join_all preserves order of futures
        for (i, r) in results.iter().enumerate() {
            assert_eq!(r.call.id, format!("c{i}"));
        }
    }

    #[tokio::test]
    async fn parallel_mixed_success_failure_suspension() {
        let tools = tool_map(vec![
            Arc::new(EchoTool),
            Arc::new(FailingTool),
            Arc::new(SuspendingTool),
        ]);
        let calls = vec![
            ToolCall::new("c1", "echo", json!({"message": "hi"})),
            ToolCall::new("c2", "failing", json!({})),
            ToolCall::new("c3", "suspending", json!({})),
        ];
        let executor = ParallelToolExecutor::batch_approval();

        let results = executor
            .execute(&tools, &calls, &ToolCallContext::test_default())
            .await
            .unwrap();
        assert_eq!(results.len(), 3, "parallel runs all regardless");
        assert_eq!(results[0].outcome, ToolCallOutcome::Succeeded);
        assert_eq!(results[1].outcome, ToolCallOutcome::Failed);
        assert_eq!(results[2].outcome, ToolCallOutcome::Suspended);
    }

    #[test]
    fn sequential_requires_incremental_state() {
        let executor = SequentialToolExecutor;
        assert!(executor.requires_incremental_state());
    }

    #[test]
    fn parallel_does_not_require_incremental_state() {
        let executor = ParallelToolExecutor::streaming();
        assert!(!executor.requires_incremental_state());
        let batch = ParallelToolExecutor::batch_approval();
        assert!(!batch.requires_incremental_state());
    }

    #[tokio::test]
    async fn execute_single_tool_success_returns_correct_tool_name() {
        let tools = tool_map(vec![Arc::new(EchoTool)]);
        let call = ToolCall::new("c1", "echo", json!({"message": "test"}));
        let ctx = ToolCallContext::test_default();

        let output = execute_single_tool(&tools, &call, &ctx).await;
        assert!(output.result.is_success());
        assert_eq!(output.result.tool_name, "echo");
    }

    #[cfg(feature = "background")]
    struct SpawnBackgroundTaskTool {
        manager: Arc<BackgroundTaskManager>,
        spawned_task_id: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[cfg(feature = "background")]
    #[async_trait]
    impl Tool for SpawnBackgroundTaskTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(
                "spawn_background",
                "spawn_background",
                "Spawns a background task",
            )
        }

        async fn execute(
            &self,
            _args: Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            let task_id = self
                .manager
                .spawn(
                    "thread-1",
                    "background",
                    None,
                    "spawned from tool",
                    TaskParentContext::default(),
                    |_| async { TaskResult::Success(json!({"ok": true})) },
                )
                .await
                .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
            *self.spawned_task_id.lock().unwrap() = Some(task_id.clone());
            Ok(ToolResult::success("spawn_background", json!({"task_id": task_id})).into())
        }
    }

    #[cfg(feature = "background")]
    #[tokio::test]
    async fn execute_single_tool_scopes_tool_lineage_for_background_spawns() {
        let store = StateStore::new();
        let manager = Arc::new(BackgroundTaskManager::new());
        manager.set_store(store.clone());
        let plugin: Arc<dyn Plugin> = Arc::new(BackgroundTaskPlugin::new(manager.clone()));
        let env = ExecutionEnv::from_plugins(&[plugin], &Default::default()).unwrap();
        store.register_keys(&env.key_registrations).unwrap();

        let spawned_task_id = Arc::new(std::sync::Mutex::new(None::<String>));
        let tools = tool_map(vec![Arc::new(SpawnBackgroundTaskTool {
            manager: manager.clone(),
            spawned_task_id: spawned_task_id.clone(),
        }) as Arc<dyn Tool>]);
        let executor = SequentialToolExecutor;
        let calls = vec![ToolCall::new("call-77", "spawn_background", json!({}))];

        let mut ctx = ToolCallContext::test_default();
        ctx.run_identity = RunIdentity::new(
            "thread-1".into(),
            None,
            "run-77".into(),
            None,
            "agent-77".into(),
            RunOrigin::User,
        );
        ctx.cancellation_token = Some(crate::cancellation::CancellationToken::new());

        let results = executor.execute(&tools, &calls, &ctx).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].result.is_success());

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let task_id = spawned_task_id
            .lock()
            .unwrap()
            .clone()
            .expect("spawned task id should be recorded");
        let summary = manager
            .get(&task_id)
            .await
            .expect("spawned background task should be queryable");
        assert_eq!(summary.parent_context.run_id.as_deref(), Some("run-77"));
        assert_eq!(summary.parent_context.call_id.as_deref(), Some("call-77"));
        assert_eq!(summary.parent_context.agent_id.as_deref(), Some("agent-77"));
    }
}
