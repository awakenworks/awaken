//! Tool execution strategies: Sequential and Parallel.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::message::ToolCall;
use awaken_contract::contract::suspension::ToolCallOutcome;
use awaken_contract::contract::tool::{Tool, ToolCallContext, ToolResult};

/// Result of executing a single tool call.
#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub call: ToolCall,
    pub result: ToolResult,
    pub outcome: ToolCallOutcome,
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
}

/// Execute tool calls one by one. Each tool sees the results of previous tools.
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
            let result = execute_single_tool(tools, call, &ctx).await;
            let outcome = ToolCallOutcome::from_tool_result(&result);

            results.push(ToolExecutionResult {
                call: call.clone(),
                result,
                outcome,
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
                async move {
                    let result = execute_single_tool(&tools, &call, &ctx).await;
                    let outcome = ToolCallOutcome::from_tool_result(&result);
                    ToolExecutionResult {
                        call,
                        result,
                        outcome,
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
) -> ToolResult {
    let Some(tool) = tools.get(&call.name) else {
        return ToolResult::error(&call.name, format!("tool '{}' not found", call.name));
    };

    if let Err(e) = tool.validate_args(&call.arguments) {
        return ToolResult::error(&call.name, e.to_string());
    }

    match tool.execute(call.arguments.clone(), ctx).await {
        Ok(result) => result,
        Err(e) => ToolResult::error(&call.name, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::tool::{ToolDescriptor, ToolError};
    use serde_json::{Value, json};

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
        ) -> Result<ToolResult, ToolError> {
            let msg = args
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("no message")
                .to_string();
            Ok(ToolResult::success_with_message("echo", args, msg))
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
        ) -> Result<ToolResult, ToolError> {
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
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::suspended("suspending", "needs approval"))
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
        ) -> Result<ToolResult, ToolError> {
            let n = self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolResult::success(
                "counting",
                json!({"call_number": n + 1}),
            ))
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
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success("strict", args))
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
    fn tool_execution_result_clone_and_debug() {
        // Ensure types are debuggable
        let result = ToolExecutionResult {
            call: ToolCall::new("c1", "echo", json!({})),
            result: ToolResult::success("echo", json!({"ok": true})),
            outcome: ToolCallOutcome::Succeeded,
        };
        let cloned = result.clone();
        assert_eq!(cloned.call.id, "c1");
        assert_eq!(cloned.outcome, ToolCallOutcome::Succeeded);
        let _ = format!("{:?}", result);
    }

    #[test]
    fn tool_executor_error_display() {
        let err = ToolExecutorError::Cancelled;
        assert!(err.to_string().contains("cancelled"));
        let err2 = ToolExecutorError::Failed("some reason".into());
        assert!(err2.to_string().contains("some reason"));
    }
}
