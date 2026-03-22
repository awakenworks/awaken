//! Stop condition policy system and built-in policies.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use async_trait::async_trait;

use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::runtime::{PhaseContext, PhaseHook};
use crate::state::StateCommand;
use awaken_contract::StateError;
use awaken_contract::contract::lifecycle::{StopConditionSpec, TerminationReason};
use awaken_contract::model::Phase;

use super::state::{RunLifecycle, RunLifecycleUpdate};

// ---------------------------------------------------------------------------
// StopPolicy trait and StopDecision
// ---------------------------------------------------------------------------

/// Decision returned by a stop policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDecision {
    /// No stop condition triggered.
    Continue,
    /// Stop the run with a code and detail message.
    Stop { code: String, detail: String },
}

/// Statistics available to stop policies for evaluation.
#[derive(Debug, Clone)]
pub struct StopPolicyStats {
    pub step_count: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub elapsed_ms: u64,
    pub consecutive_errors: u32,
    pub last_tool_names: Vec<String>,
    pub last_response_text: String,
}

/// A stateless stop condition evaluator.
///
/// Reads stats from the context and returns a decision.
/// Implementations must NOT be async — evaluation is pure computation on stats.
pub trait StopPolicy: Send + Sync + 'static {
    /// Unique identifier for this policy.
    fn id(&self) -> &str;

    /// Evaluate whether the run should stop based on current stats.
    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision;
}

// ---------------------------------------------------------------------------
// Built-in policies
// ---------------------------------------------------------------------------

/// Stop when step count reaches or exceeds `max`.
pub struct MaxRoundsPolicy {
    pub max: usize,
}

impl MaxRoundsPolicy {
    pub fn new(max: usize) -> Self {
        Self { max }
    }
}

impl StopPolicy for MaxRoundsPolicy {
    fn id(&self) -> &str {
        "max_rounds"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.max == 0 {
            return StopDecision::Continue;
        }
        if stats.step_count as usize > self.max {
            StopDecision::Stop {
                code: "max_rounds".into(),
                detail: format!("exceeded {} rounds", self.max),
            }
        } else {
            StopDecision::Continue
        }
    }
}

/// Stop when total tokens (input + output) exceed a budget.
pub struct TokenBudgetPolicy {
    pub max_total: u64,
}

impl TokenBudgetPolicy {
    pub fn new(max_total: u64) -> Self {
        Self { max_total }
    }
}

impl StopPolicy for TokenBudgetPolicy {
    fn id(&self) -> &str {
        "token_budget"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.max_total == 0 {
            return StopDecision::Continue;
        }
        let total = stats.total_input_tokens + stats.total_output_tokens;
        if total > self.max_total {
            StopDecision::Stop {
                code: "token_budget".into(),
                detail: format!("token usage {} exceeds budget {}", total, self.max_total),
            }
        } else {
            StopDecision::Continue
        }
    }
}

/// Stop when elapsed time exceeds a limit in milliseconds.
pub struct TimeoutPolicy {
    pub max_ms: u64,
}

impl TimeoutPolicy {
    pub fn new(max_ms: u64) -> Self {
        Self { max_ms }
    }
}

impl StopPolicy for TimeoutPolicy {
    fn id(&self) -> &str {
        "timeout"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.max_ms == 0 {
            return StopDecision::Continue;
        }
        if stats.elapsed_ms > self.max_ms {
            StopDecision::Stop {
                code: "timeout".into(),
                detail: format!(
                    "elapsed {}ms exceeds limit {}ms",
                    stats.elapsed_ms, self.max_ms
                ),
            }
        } else {
            StopDecision::Continue
        }
    }
}

/// Stop after N consecutive tool errors.
pub struct ConsecutiveErrorsPolicy {
    pub max: u32,
}

impl ConsecutiveErrorsPolicy {
    pub fn new(max: u32) -> Self {
        Self { max }
    }
}

impl StopPolicy for ConsecutiveErrorsPolicy {
    fn id(&self) -> &str {
        "consecutive_errors"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.max == 0 {
            return StopDecision::Continue;
        }
        if stats.consecutive_errors >= self.max {
            StopDecision::Stop {
                code: "consecutive_errors".into(),
                detail: format!(
                    "{} consecutive errors (limit {})",
                    stats.consecutive_errors, self.max
                ),
            }
        } else {
            StopDecision::Continue
        }
    }
}

// ---------------------------------------------------------------------------
// Factory: StopConditionSpec -> StopPolicy
// ---------------------------------------------------------------------------

/// Convert declarative stop condition specs into policy instances.
pub fn policies_from_specs(specs: &[StopConditionSpec]) -> Vec<Arc<dyn StopPolicy>> {
    specs
        .iter()
        .filter_map(|spec| -> Option<Arc<dyn StopPolicy>> {
            match spec {
                StopConditionSpec::MaxRounds { rounds } => {
                    Some(Arc::new(MaxRoundsPolicy::new(*rounds)))
                }
                StopConditionSpec::Timeout { seconds } => {
                    Some(Arc::new(TimeoutPolicy::new(*seconds * 1000)))
                }
                StopConditionSpec::TokenBudget { max_total } => {
                    Some(Arc::new(TokenBudgetPolicy::new(*max_total as u64)))
                }
                StopConditionSpec::ConsecutiveErrors { max } => {
                    Some(Arc::new(ConsecutiveErrorsPolicy::new(*max as u32)))
                }
                // StopOnTool, ContentMatch, LoopDetection are not yet implemented
                StopConditionSpec::StopOnTool { .. }
                | StopConditionSpec::ContentMatch { .. }
                | StopConditionSpec::LoopDetection { .. } => None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// StopConditionPlugin — wraps policies into a PhaseHook
// ---------------------------------------------------------------------------

/// Plugin that evaluates stop policies after each inference step.
pub struct StopConditionPlugin {
    policies: Vec<Arc<dyn StopPolicy>>,
}

impl StopConditionPlugin {
    pub fn new(policies: Vec<Arc<dyn StopPolicy>>) -> Self {
        Self { policies }
    }
}

impl Plugin for StopConditionPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "stop-condition",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_phase_hook(
            "stop-condition",
            Phase::AfterInference,
            StopConditionHook {
                policies: self.policies.clone(),
                step_count: AtomicU32::new(0),
                total_input_tokens: AtomicU64::new(0),
                total_output_tokens: AtomicU64::new(0),
                start_time_ms: AtomicU64::new(0),
                consecutive_errors: AtomicU32::new(0),
                initialized: AtomicU32::new(0),
            },
        )
    }
}

/// Internal hook that builds stats and evaluates all policies.
struct StopConditionHook {
    policies: Vec<Arc<dyn StopPolicy>>,
    step_count: AtomicU32,
    total_input_tokens: AtomicU64,
    total_output_tokens: AtomicU64,
    start_time_ms: AtomicU64,
    consecutive_errors: AtomicU32,
    /// 0 = not initialized, 1 = initialized.
    initialized: AtomicU32,
}

impl StopConditionHook {
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn ensure_initialized(&self) {
        if self
            .initialized
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.start_time_ms.store(Self::now_ms(), Ordering::SeqCst);
        }
    }

    fn build_stats(&self, ctx: &PhaseContext) -> StopPolicyStats {
        self.ensure_initialized();

        // Use internal counter for step tracking. RunLifecycleState step_count
        // is incremented after AfterInference (in complete_step), so it would
        // always lag behind when this hook evaluates.
        let step_count = self.step_count.fetch_add(1, Ordering::SeqCst) + 1;

        // Extract token usage and response text from LLM response
        let mut last_tool_names = Vec::new();
        let mut last_response_text = String::new();
        let mut is_error = false;

        if let Some(ref response) = ctx.llm_response {
            match &response.outcome {
                Ok(stream_result) => {
                    let input = stream_result
                        .usage
                        .as_ref()
                        .and_then(|u| u.prompt_tokens)
                        .unwrap_or(0) as u64;
                    let output = stream_result
                        .usage
                        .as_ref()
                        .and_then(|u| u.completion_tokens)
                        .unwrap_or(0) as u64;
                    self.total_input_tokens.fetch_add(input, Ordering::SeqCst);
                    self.total_output_tokens.fetch_add(output, Ordering::SeqCst);

                    last_response_text = stream_result.text();
                    last_tool_names = stream_result
                        .tool_calls
                        .iter()
                        .map(|tc| tc.name.clone())
                        .collect();

                    // Successful inference resets consecutive errors
                    self.consecutive_errors.store(0, Ordering::SeqCst);
                }
                Err(_) => {
                    is_error = true;
                    self.consecutive_errors.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        let elapsed_ms = Self::now_ms().saturating_sub(self.start_time_ms.load(Ordering::SeqCst));
        let consecutive_errors = if is_error {
            self.consecutive_errors.load(Ordering::SeqCst)
        } else {
            0
        };

        StopPolicyStats {
            step_count,
            total_input_tokens: self.total_input_tokens.load(Ordering::SeqCst),
            total_output_tokens: self.total_output_tokens.load(Ordering::SeqCst),
            elapsed_ms,
            consecutive_errors,
            last_tool_names,
            last_response_text,
        }
    }
}

#[async_trait]
impl PhaseHook for StopConditionHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let stats = self.build_stats(ctx);

        for policy in &self.policies {
            if let StopDecision::Stop { code, detail } = policy.evaluate(&stats) {
                let reason = TerminationReason::stopped_with_detail(code, detail);
                let (_, done_reason) = reason.to_run_status();
                let mut cmd = StateCommand::new();
                cmd.update::<RunLifecycle>(RunLifecycleUpdate::Done {
                    done_reason: done_reason.unwrap_or_default(),
                    updated_at: Self::now_ms(),
                });
                return Ok(cmd);
            }
        }

        Ok(StateCommand::new())
    }
}

// ---------------------------------------------------------------------------
// MaxRoundsPlugin — convenience wrapper
// ---------------------------------------------------------------------------

/// Convenience plugin that terminates the run after a maximum number of steps.
///
/// Wraps `StopConditionPlugin` with a single `MaxRoundsPolicy`.
pub struct MaxRoundsPlugin {
    max_rounds: usize,
}

impl MaxRoundsPlugin {
    pub fn new(max_rounds: usize) -> Self {
        Self { max_rounds }
    }
}

impl Plugin for MaxRoundsPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "stop-condition:max-rounds",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        // Delegate to StopConditionPlugin internals
        let policies: Vec<Arc<dyn StopPolicy>> =
            vec![Arc::new(MaxRoundsPolicy::new(self.max_rounds))];
        registrar.register_phase_hook(
            "stop-condition:max-rounds",
            Phase::AfterInference,
            StopConditionHook {
                policies,
                step_count: AtomicU32::new(0),
                total_input_tokens: AtomicU64::new(0),
                total_output_tokens: AtomicU64::new(0),
                start_time_ms: AtomicU64::new(0),
                consecutive_errors: AtomicU32::new(0),
                initialized: AtomicU32::new(0),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ExecutionEnv, PhaseRuntime};
    use crate::state::StateStore;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::inference::{
        InferenceError, LLMResponse, StopReason, StreamResult, TokenUsage,
    };
    use awaken_contract::contract::lifecycle::RunStatus;

    use super::super::state::RunLifecycle;

    /// Plugin that registers the RunLifecycle key needed by stop condition hooks.
    struct LifecycleKeyPlugin;
    impl Plugin for LifecycleKeyPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "lifecycle-key",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_key::<RunLifecycle>(crate::state::StateKeyOptions::default())
        }
    }

    fn make_test_env(
        policies: Vec<Arc<dyn StopPolicy>>,
    ) -> (StateStore, PhaseRuntime, ExecutionEnv) {
        let store = StateStore::new();
        let runtime = PhaseRuntime::new(store.clone()).unwrap();
        store.install_plugin(LifecycleKeyPlugin).unwrap();

        // Initialize RunLifecycle to Running so Done transitions are valid
        let mut patch = crate::state::MutationBatch::new();
        patch.update::<RunLifecycle>(super::super::state::RunLifecycleUpdate::Start {
            run_id: "test".into(),
            updated_at: 0,
        });
        store.commit(patch).unwrap();

        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(LifecycleKeyPlugin),
            Arc::new(StopConditionPlugin::new(policies)),
        ];
        let env = ExecutionEnv::from_plugins(&plugins).unwrap();
        (store, runtime, env)
    }

    // -----------------------------------------------------------------------
    // MaxRoundsPlugin tests — now check RunLifecycle state
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn max_rounds_plugin_sets_done_after_exceeding_limit() {
        let store = StateStore::new();
        let runtime = PhaseRuntime::new(store.clone()).unwrap();
        store.install_plugin(LifecycleKeyPlugin).unwrap();

        // Initialize to Running
        let mut patch = crate::state::MutationBatch::new();
        patch.update::<RunLifecycle>(super::super::state::RunLifecycleUpdate::Start {
            run_id: "test".into(),
            updated_at: 0,
        });
        store.commit(patch).unwrap();

        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(LifecycleKeyPlugin),
            Arc::new(MaxRoundsPlugin::new(2)),
        ];
        let env = ExecutionEnv::from_plugins(&plugins).unwrap();

        // Round 1 and 2: still Running
        runtime
            .run_phase(&env, Phase::AfterInference)
            .await
            .unwrap();
        runtime
            .run_phase(&env, Phase::AfterInference)
            .await
            .unwrap();
        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Running);

        // Round 3: exceeds limit → Done
        runtime
            .run_phase(&env, Phase::AfterInference)
            .await
            .unwrap();
        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Done);
        assert!(
            lifecycle
                .done_reason
                .as_ref()
                .unwrap()
                .contains("max_rounds")
        );
    }

    // -----------------------------------------------------------------------
    // StopPolicy unit tests
    // -----------------------------------------------------------------------

    fn base_stats() -> StopPolicyStats {
        StopPolicyStats {
            step_count: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            elapsed_ms: 0,
            consecutive_errors: 0,
            last_tool_names: vec![],
            last_response_text: String::new(),
        }
    }

    #[test]
    fn max_rounds_policy_continues_at_limit() {
        let policy = MaxRoundsPolicy::new(5);
        let stats = StopPolicyStats {
            step_count: 5,
            ..base_stats()
        };
        assert_eq!(policy.evaluate(&stats), StopDecision::Continue);
    }

    #[test]
    fn max_rounds_policy_stops_over_limit() {
        let policy = MaxRoundsPolicy::new(5);
        let stats = StopPolicyStats {
            step_count: 6,
            ..base_stats()
        };
        assert!(
            matches!(policy.evaluate(&stats), StopDecision::Stop { code, .. } if code == "max_rounds")
        );
    }

    #[test]
    fn token_budget_policy_continues_under_budget() {
        let policy = TokenBudgetPolicy::new(1000);
        let stats = StopPolicyStats {
            total_input_tokens: 400,
            total_output_tokens: 500,
            ..base_stats()
        };
        assert_eq!(policy.evaluate(&stats), StopDecision::Continue);
    }

    #[test]
    fn token_budget_policy_stops_over_budget() {
        let policy = TokenBudgetPolicy::new(1000);
        let stats = StopPolicyStats {
            total_input_tokens: 600,
            total_output_tokens: 500,
            ..base_stats()
        };
        assert!(
            matches!(policy.evaluate(&stats), StopDecision::Stop { code, .. } if code == "token_budget")
        );
    }

    #[test]
    fn timeout_policy_continues_under_limit() {
        let policy = TimeoutPolicy::new(5000);
        let stats = StopPolicyStats {
            elapsed_ms: 4999,
            ..base_stats()
        };
        assert_eq!(policy.evaluate(&stats), StopDecision::Continue);
    }

    #[test]
    fn timeout_policy_stops_over_limit() {
        let policy = TimeoutPolicy::new(5000);
        let stats = StopPolicyStats {
            elapsed_ms: 5001,
            ..base_stats()
        };
        assert!(
            matches!(policy.evaluate(&stats), StopDecision::Stop { code, .. } if code == "timeout")
        );
    }

    #[test]
    fn consecutive_errors_policy_continues_below_limit() {
        let policy = ConsecutiveErrorsPolicy::new(3);
        let stats = StopPolicyStats {
            consecutive_errors: 2,
            ..base_stats()
        };
        assert_eq!(policy.evaluate(&stats), StopDecision::Continue);
    }

    #[test]
    fn consecutive_errors_policy_stops_at_limit() {
        let policy = ConsecutiveErrorsPolicy::new(3);
        let stats = StopPolicyStats {
            consecutive_errors: 3,
            ..base_stats()
        };
        assert!(
            matches!(policy.evaluate(&stats), StopDecision::Stop { code, .. } if code == "consecutive_errors")
        );
    }

    // -----------------------------------------------------------------------
    // Multiple policies: first to fire wins
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_policies_first_stop_wins() {
        let policies: Vec<Arc<dyn StopPolicy>> = vec![
            Arc::new(MaxRoundsPolicy::new(100)),
            Arc::new(TokenBudgetPolicy::new(500)),
            Arc::new(TimeoutPolicy::new(10_000)),
        ];

        let stats = StopPolicyStats {
            step_count: 3,
            total_input_tokens: 300,
            total_output_tokens: 300,
            elapsed_ms: 1000,
            ..base_stats()
        };

        // Token budget should fire first (600 > 500), max_rounds continues (3 < 100)
        let mut result = StopDecision::Continue;
        for policy in &policies {
            let decision = policy.evaluate(&stats);
            if matches!(decision, StopDecision::Stop { .. }) {
                result = decision;
                break;
            }
        }
        assert!(matches!(result, StopDecision::Stop { code, .. } if code == "token_budget"));
    }

    // -----------------------------------------------------------------------
    // policies_from_specs roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn policies_from_specs_converts_known_specs() {
        let specs = vec![
            StopConditionSpec::MaxRounds { rounds: 10 },
            StopConditionSpec::Timeout { seconds: 60 },
            StopConditionSpec::TokenBudget { max_total: 50_000 },
            StopConditionSpec::ConsecutiveErrors { max: 5 },
        ];
        let policies = policies_from_specs(&specs);
        assert_eq!(policies.len(), 4);
        assert_eq!(policies[0].id(), "max_rounds");
        assert_eq!(policies[1].id(), "timeout");
        assert_eq!(policies[2].id(), "token_budget");
        assert_eq!(policies[3].id(), "consecutive_errors");
    }

    #[test]
    fn policies_from_specs_skips_unimplemented_specs() {
        let specs = vec![
            StopConditionSpec::StopOnTool {
                tool_name: "done".into(),
            },
            StopConditionSpec::ContentMatch {
                pattern: "DONE".into(),
            },
            StopConditionSpec::LoopDetection { window: 5 },
        ];
        let policies = policies_from_specs(&specs);
        assert!(policies.is_empty());
    }

    #[test]
    fn policies_from_specs_timeout_converts_seconds_to_ms() {
        let specs = vec![StopConditionSpec::Timeout { seconds: 30 }];
        let policies = policies_from_specs(&specs);
        let stats = StopPolicyStats {
            elapsed_ms: 30_001,
            ..base_stats()
        };
        assert!(matches!(
            policies[0].evaluate(&stats),
            StopDecision::Stop { .. }
        ));

        let stats_under = StopPolicyStats {
            elapsed_ms: 29_999,
            ..base_stats()
        };
        assert_eq!(policies[0].evaluate(&stats_under), StopDecision::Continue);
    }

    // -----------------------------------------------------------------------
    // StopConditionPlugin integration tests
    // -----------------------------------------------------------------------

    fn make_llm_response_with_tokens(input: i32, output: i32) -> LLMResponse {
        LLMResponse::success(StreamResult {
            content: vec![ContentBlock::text("response")],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: Some(input),
                completion_tokens: Some(output),
                total_tokens: Some(input + output),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn make_llm_error() -> LLMResponse {
        LLMResponse::error(InferenceError {
            error_type: "api_error".into(),
            message: "server error".into(),
            error_class: None,
        })
    }

    #[tokio::test]
    async fn stop_condition_plugin_token_budget_fires() {
        let (store, runtime, env) = make_test_env(vec![Arc::new(TokenBudgetPolicy::new(1000))]);

        // First call: 600 tokens, under budget
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(300, 300));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running
        );

        // Second call: adds 600 more => 1200 total, over budget → Done
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(300, 300));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Done);
        assert!(
            lifecycle
                .done_reason
                .as_ref()
                .unwrap()
                .contains("token_budget")
        );
    }

    #[tokio::test]
    async fn stop_condition_plugin_consecutive_errors_fires() {
        let (store, runtime, env) = make_test_env(vec![Arc::new(ConsecutiveErrorsPolicy::new(3))]);

        // 2 errors: should not fire
        for _ in 0..2 {
            let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
                .with_llm_response(make_llm_error());
            runtime.run_phase_with_context(&env, ctx).await.unwrap();
        }
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running
        );

        // 3rd error: should fire → Done
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_error());
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Done);
        assert!(
            lifecycle
                .done_reason
                .as_ref()
                .unwrap()
                .contains("consecutive_errors")
        );
    }

    #[tokio::test]
    async fn stop_condition_plugin_success_resets_consecutive_errors() {
        let (store, runtime, env) = make_test_env(vec![Arc::new(ConsecutiveErrorsPolicy::new(3))]);

        // 2 errors
        for _ in 0..2 {
            let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
                .with_llm_response(make_llm_error());
            runtime.run_phase_with_context(&env, ctx).await.unwrap();
        }

        // 1 success resets the counter
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(100, 50));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        // 2 more errors: still under limit (2 < 3)
        for _ in 0..2 {
            let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
                .with_llm_response(make_llm_error());
            runtime.run_phase_with_context(&env, ctx).await.unwrap();
        }

        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running
        );
    }

    // -----------------------------------------------------------------------
    // Edge case: zero means unlimited (never fires)
    // -----------------------------------------------------------------------

    #[test]
    fn max_rounds_zero_never_fires() {
        let policy = MaxRoundsPolicy::new(0);
        // Even at very high step counts, zero means unlimited
        for step in [0, 1, 100, u32::MAX] {
            let stats = StopPolicyStats {
                step_count: step,
                ..base_stats()
            };
            assert_eq!(
                policy.evaluate(&stats),
                StopDecision::Continue,
                "max_rounds(0) should never fire at step_count={}",
                step
            );
        }
    }

    #[test]
    fn token_budget_zero_never_fires() {
        let policy = TokenBudgetPolicy::new(0);
        for tokens in [0, 1, 1_000_000, u64::MAX / 2] {
            let stats = StopPolicyStats {
                total_input_tokens: tokens,
                total_output_tokens: tokens,
                ..base_stats()
            };
            assert_eq!(
                policy.evaluate(&stats),
                StopDecision::Continue,
                "token_budget(0) should never fire at total={}",
                tokens * 2
            );
        }
    }

    #[test]
    fn timeout_zero_never_fires() {
        let policy = TimeoutPolicy::new(0);
        for ms in [0, 1, 999_999, u64::MAX / 2] {
            let stats = StopPolicyStats {
                elapsed_ms: ms,
                ..base_stats()
            };
            assert_eq!(
                policy.evaluate(&stats),
                StopDecision::Continue,
                "timeout(0) should never fire at elapsed_ms={}",
                ms
            );
        }
    }

    #[test]
    fn consecutive_errors_zero_never_fires() {
        let policy = ConsecutiveErrorsPolicy::new(0);
        for errs in [0, 1, 100, u32::MAX] {
            let stats = StopPolicyStats {
                consecutive_errors: errs,
                ..base_stats()
            };
            assert_eq!(
                policy.evaluate(&stats),
                StopDecision::Continue,
                "consecutive_errors(0) should never fire at consecutive_errors={}",
                errs
            );
        }
    }

    // -----------------------------------------------------------------------
    // Multiple policies: first-stop-wins variations
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_policies_token_budget_fires_first() {
        // MaxRounds is generous (1000), but token budget is tight (500)
        let policies: Vec<Arc<dyn StopPolicy>> = vec![
            Arc::new(MaxRoundsPolicy::new(1000)),
            Arc::new(TokenBudgetPolicy::new(500)),
        ];

        let stats = StopPolicyStats {
            step_count: 2,
            total_input_tokens: 300,
            total_output_tokens: 300,
            ..base_stats()
        };

        let mut result = StopDecision::Continue;
        for policy in &policies {
            let decision = policy.evaluate(&stats);
            if matches!(decision, StopDecision::Stop { .. }) {
                result = decision;
                break;
            }
        }
        assert!(
            matches!(result, StopDecision::Stop { code, .. } if code == "token_budget"),
            "token_budget should fire before max_rounds"
        );
    }

    #[test]
    fn multiple_policies_all_continue() {
        let policies: Vec<Arc<dyn StopPolicy>> = vec![
            Arc::new(MaxRoundsPolicy::new(100)),
            Arc::new(TokenBudgetPolicy::new(10_000)),
            Arc::new(TimeoutPolicy::new(60_000)),
            Arc::new(ConsecutiveErrorsPolicy::new(5)),
        ];

        let stats = StopPolicyStats {
            step_count: 3,
            total_input_tokens: 500,
            total_output_tokens: 500,
            elapsed_ms: 1000,
            consecutive_errors: 1,
            last_tool_names: vec![],
            last_response_text: String::new(),
        };

        for policy in &policies {
            assert_eq!(
                policy.evaluate(&stats),
                StopDecision::Continue,
                "policy '{}' should not fire",
                policy.id()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Stats derivation from context (integration with PhaseContext)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stats_accumulate_tokens_across_steps() {
        let (_store, runtime, env) = make_test_env(vec![
            // Use a generous budget so it does not fire
            Arc::new(TokenBudgetPolicy::new(100_000)),
        ]);

        // Step 1: 100 input + 50 output
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(100, 50));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        // Step 2: 200 input + 150 output
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(200, 150));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        // Step 3: 300 input + 250 output
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(300, 250));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        // Now set a tight budget that the accumulated total (100+200+300 in, 50+150+250 out = 1050) exceeds
        let (store2, runtime2, env2) = make_test_env(vec![Arc::new(TokenBudgetPolicy::new(1000))]);

        // Replay the same three steps to accumulate tokens in the new hook
        for (inp, out) in [(100, 50), (200, 150), (300, 250)] {
            let ctx = PhaseContext::new(Phase::AfterInference, runtime2.store().snapshot())
                .with_llm_response(make_llm_response_with_tokens(inp, out));
            runtime2.run_phase_with_context(&env2, ctx).await.unwrap();
        }

        let lifecycle = store2.read::<RunLifecycle>().unwrap();
        assert_eq!(
            lifecycle.status,
            RunStatus::Done,
            "accumulated tokens (1050) should exceed budget (1000)"
        );
        assert!(
            lifecycle
                .done_reason
                .as_ref()
                .unwrap()
                .contains("token_budget")
        );
    }

    #[tokio::test]
    async fn stats_consecutive_errors_reset_on_success() {
        // Verify thoroughly: errors accumulate, success resets, errors must re-accumulate
        let (store, runtime, env) = make_test_env(vec![Arc::new(ConsecutiveErrorsPolicy::new(3))]);

        // 2 errors
        for _ in 0..2 {
            let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
                .with_llm_response(make_llm_error());
            runtime.run_phase_with_context(&env, ctx).await.unwrap();
        }
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running,
            "2 errors < 3 limit"
        );

        // Success resets
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(10, 10));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running
        );

        // 2 more errors: still under limit because counter was reset
        for _ in 0..2 {
            let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
                .with_llm_response(make_llm_error());
            runtime.run_phase_with_context(&env, ctx).await.unwrap();
        }
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running,
            "2 errors after reset < 3 limit"
        );

        // 3rd error after second reset should fire
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_error());
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Done);
        assert!(
            lifecycle
                .done_reason
                .as_ref()
                .unwrap()
                .contains("consecutive_errors")
        );
    }

    #[tokio::test]
    async fn stats_with_error_response_increments_errors() {
        let (store, runtime, env) = make_test_env(vec![Arc::new(ConsecutiveErrorsPolicy::new(2))]);

        // First error
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_error());
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running,
            "1 error < 2 limit"
        );

        // Second error should trigger
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_error());
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Done);
        assert!(
            lifecycle
                .done_reason
                .as_ref()
                .unwrap()
                .contains("consecutive_errors")
        );
    }

    // -----------------------------------------------------------------------
    // Run isolation: step counting
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stop_condition_does_not_fire_on_first_step() {
        // MaxRounds(1) means: stop when step_count > 1, so step 1 should continue
        let (store, runtime, env) = make_test_env(vec![Arc::new(MaxRoundsPolicy::new(1))]);

        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(10, 10));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running,
            "step_count=1 should not exceed max_rounds=1"
        );

        // Second step: step_count=2 > 1, should fire
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(10, 10));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Done);
    }

    #[tokio::test]
    async fn step_count_matches_internal_counter() {
        // Verify that step_count increments correctly: max_rounds(3) should fire on step 4
        let (store, runtime, env) = make_test_env(vec![Arc::new(MaxRoundsPolicy::new(3))]);

        // Steps 1, 2, 3: all should continue
        for i in 1..=3 {
            let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
                .with_llm_response(make_llm_response_with_tokens(10, 10));
            runtime.run_phase_with_context(&env, ctx).await.unwrap();
            assert_eq!(
                store.read::<RunLifecycle>().unwrap().status,
                RunStatus::Running,
                "step {} should not exceed max_rounds=3",
                i
            );
        }

        // Step 4: step_count=4 > 3, should fire
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(10, 10));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Done);
        assert!(
            lifecycle
                .done_reason
                .as_ref()
                .unwrap()
                .contains("max_rounds")
        );
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: stop policy edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn max_rounds_policy_step_zero_does_not_fire() {
        let policy = MaxRoundsPolicy::new(5);
        let stats = StopPolicyStats {
            step_count: 0,
            ..base_stats()
        };
        assert_eq!(policy.evaluate(&stats), StopDecision::Continue);
    }

    #[test]
    fn max_rounds_policy_fires_above_limit() {
        let policy = MaxRoundsPolicy::new(5);
        for step in [6, 10, 100] {
            let stats = StopPolicyStats {
                step_count: step,
                ..base_stats()
            };
            assert!(
                matches!(policy.evaluate(&stats), StopDecision::Stop { .. }),
                "should fire at step_count={}",
                step
            );
        }
    }

    #[test]
    fn token_budget_policy_boundary_exact_at_limit() {
        let policy = TokenBudgetPolicy::new(1000);
        // Exactly at limit — should not fire (> not >=)
        let stats = StopPolicyStats {
            total_input_tokens: 500,
            total_output_tokens: 500,
            ..base_stats()
        };
        assert_eq!(
            policy.evaluate(&stats),
            StopDecision::Continue,
            "exactly at limit should continue"
        );
    }

    #[test]
    fn token_budget_policy_fires_one_over() {
        let policy = TokenBudgetPolicy::new(1000);
        let stats = StopPolicyStats {
            total_input_tokens: 501,
            total_output_tokens: 500,
            ..base_stats()
        };
        assert!(matches!(
            policy.evaluate(&stats),
            StopDecision::Stop { code, .. } if code == "token_budget"
        ));
    }

    #[test]
    fn timeout_policy_boundary_exact_at_limit() {
        let policy = TimeoutPolicy::new(5000);
        let stats = StopPolicyStats {
            elapsed_ms: 5000,
            ..base_stats()
        };
        assert_eq!(
            policy.evaluate(&stats),
            StopDecision::Continue,
            "exactly at limit should continue"
        );
    }

    #[test]
    fn timeout_policy_fires_one_over() {
        let policy = TimeoutPolicy::new(5000);
        let stats = StopPolicyStats {
            elapsed_ms: 5001,
            ..base_stats()
        };
        assert!(matches!(
            policy.evaluate(&stats),
            StopDecision::Stop { code, .. } if code == "timeout"
        ));
    }

    #[test]
    fn consecutive_errors_policy_boundary_one_below() {
        let policy = ConsecutiveErrorsPolicy::new(3);
        let stats = StopPolicyStats {
            consecutive_errors: 2,
            ..base_stats()
        };
        assert_eq!(policy.evaluate(&stats), StopDecision::Continue);
    }

    #[test]
    fn consecutive_errors_policy_fires_above_limit() {
        let policy = ConsecutiveErrorsPolicy::new(3);
        for errs in [3, 4, 100] {
            let stats = StopPolicyStats {
                consecutive_errors: errs,
                ..base_stats()
            };
            assert!(
                matches!(policy.evaluate(&stats), StopDecision::Stop { .. }),
                "should fire at consecutive_errors={}",
                errs
            );
        }
    }

    #[test]
    fn stop_decision_eq_and_debug() {
        let d1 = StopDecision::Continue;
        let d2 = StopDecision::Continue;
        assert_eq!(d1, d2);

        let d3 = StopDecision::Stop {
            code: "max_rounds".into(),
            detail: "exceeded 5 rounds".into(),
        };
        let d4 = StopDecision::Stop {
            code: "max_rounds".into(),
            detail: "exceeded 5 rounds".into(),
        };
        assert_eq!(d3, d4);
        assert_ne!(d1, d3);

        // Debug should not panic
        let _ = format!("{:?}", d3);
    }

    #[test]
    fn stop_policy_stats_clone() {
        let stats = StopPolicyStats {
            step_count: 5,
            total_input_tokens: 100,
            total_output_tokens: 50,
            elapsed_ms: 1000,
            consecutive_errors: 2,
            last_tool_names: vec!["echo".into()],
            last_response_text: "hello".into(),
        };
        let cloned = stats.clone();
        assert_eq!(cloned.step_count, 5);
        assert_eq!(cloned.last_tool_names, vec!["echo"]);
    }

    #[test]
    fn multiple_policies_max_rounds_fires_first() {
        let policies: Vec<Arc<dyn StopPolicy>> = vec![
            Arc::new(MaxRoundsPolicy::new(5)),
            Arc::new(TokenBudgetPolicy::new(100_000)),
        ];

        let stats = StopPolicyStats {
            step_count: 10,
            total_input_tokens: 50,
            total_output_tokens: 50,
            ..base_stats()
        };

        let mut result = StopDecision::Continue;
        for policy in &policies {
            let decision = policy.evaluate(&stats);
            if matches!(decision, StopDecision::Stop { .. }) {
                result = decision;
                break;
            }
        }
        assert!(
            matches!(result, StopDecision::Stop { code, .. } if code == "max_rounds"),
            "max_rounds should fire first"
        );
    }

    #[test]
    fn multiple_policies_timeout_fires_first() {
        let policies: Vec<Arc<dyn StopPolicy>> = vec![
            Arc::new(MaxRoundsPolicy::new(1000)),
            Arc::new(TimeoutPolicy::new(5000)),
            Arc::new(TokenBudgetPolicy::new(100_000)),
        ];

        let stats = StopPolicyStats {
            step_count: 3,
            elapsed_ms: 6000,
            total_input_tokens: 100,
            total_output_tokens: 100,
            ..base_stats()
        };

        let mut result = StopDecision::Continue;
        for policy in &policies {
            let decision = policy.evaluate(&stats);
            if matches!(decision, StopDecision::Stop { .. }) {
                result = decision;
                break;
            }
        }
        assert!(
            matches!(result, StopDecision::Stop { code, .. } if code == "timeout"),
            "timeout should fire first"
        );
    }

    #[test]
    fn policies_from_specs_mixed_known_and_unknown() {
        let specs = vec![
            StopConditionSpec::MaxRounds { rounds: 10 },
            StopConditionSpec::StopOnTool {
                tool_name: "done".into(),
            },
            StopConditionSpec::Timeout { seconds: 60 },
            StopConditionSpec::LoopDetection { window: 5 },
            StopConditionSpec::ConsecutiveErrors { max: 3 },
        ];
        let policies = policies_from_specs(&specs);
        // Only MaxRounds, Timeout, ConsecutiveErrors should be created
        assert_eq!(policies.len(), 3);
        assert_eq!(policies[0].id(), "max_rounds");
        assert_eq!(policies[1].id(), "timeout");
        assert_eq!(policies[2].id(), "consecutive_errors");
    }

    #[test]
    fn policies_from_specs_empty_input() {
        let policies = policies_from_specs(&[]);
        assert!(policies.is_empty());
    }

    #[test]
    fn stop_condition_spec_serialization_roundtrip() {
        let specs = vec![
            StopConditionSpec::MaxRounds { rounds: 5 },
            StopConditionSpec::Timeout { seconds: 30 },
            StopConditionSpec::TokenBudget { max_total: 1000 },
            StopConditionSpec::ConsecutiveErrors { max: 3 },
            StopConditionSpec::StopOnTool {
                tool_name: "finish".to_string(),
            },
            StopConditionSpec::ContentMatch {
                pattern: "DONE".to_string(),
            },
            StopConditionSpec::LoopDetection { window: 4 },
        ];
        for spec in specs {
            let encoded = serde_json::to_string(&spec).unwrap();
            let restored: StopConditionSpec = serde_json::from_str(&encoded).unwrap();
            assert_eq!(restored, spec);
        }
    }

    // -----------------------------------------------------------------------
    // StopConditionPlugin integration: combined policies
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn combined_policies_token_budget_fires_before_max_rounds() {
        let (store, runtime, env) = make_test_env(vec![
            Arc::new(MaxRoundsPolicy::new(100)),
            Arc::new(TokenBudgetPolicy::new(500)),
        ]);

        // 2 steps with 300 tokens each => 600 total > 500 budget
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(200, 100));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running,
            "first step under budget"
        );

        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(200, 100));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Done);
        assert!(
            lifecycle
                .done_reason
                .as_ref()
                .unwrap()
                .contains("token_budget")
        );
    }

    #[tokio::test]
    async fn stop_condition_not_affected_by_empty_llm_response() {
        let (store, runtime, env) = make_test_env(vec![Arc::new(ConsecutiveErrorsPolicy::new(3))]);

        // Run phase without LLM response (no with_llm_response)
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot());
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        // Should still be running (no error counted since no response)
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running
        );
    }

    #[tokio::test]
    async fn max_rounds_exact_boundary_step_equals_max() {
        // MaxRounds(3): step 1, 2, 3 should continue; step 4 should fire
        let (store, runtime, env) = make_test_env(vec![Arc::new(MaxRoundsPolicy::new(3))]);

        for i in 1..=3 {
            let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
                .with_llm_response(make_llm_response_with_tokens(10, 10));
            runtime.run_phase_with_context(&env, ctx).await.unwrap();
            assert_eq!(
                store.read::<RunLifecycle>().unwrap().status,
                RunStatus::Running,
                "step {} should continue",
                i
            );
        }

        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(10, 10));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Done,
            "step 4 should trigger stop"
        );
    }

    #[tokio::test]
    async fn consecutive_errors_exact_threshold() {
        let (store, runtime, env) = make_test_env(vec![Arc::new(ConsecutiveErrorsPolicy::new(1))]);

        // First error should trigger immediately
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_error());
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        let lifecycle = store.read::<RunLifecycle>().unwrap();
        assert_eq!(lifecycle.status, RunStatus::Done);
        assert!(
            lifecycle
                .done_reason
                .as_ref()
                .unwrap()
                .contains("consecutive_errors")
        );
    }

    #[tokio::test]
    async fn stop_condition_interleaved_error_success_error() {
        let (store, runtime, env) = make_test_env(vec![Arc::new(ConsecutiveErrorsPolicy::new(2))]);

        // Error
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_error());
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running
        );

        // Success resets
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tokens(10, 10));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running
        );

        // Error again
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_error());
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running,
            "1 error after reset < 2 limit"
        );

        // Second consecutive error
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_error());
        runtime.run_phase_with_context(&env, ctx).await.unwrap();
        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Done,
            "2 consecutive errors should trigger"
        );
    }

    // -----------------------------------------------------------------------
    // LLM response with tool call names
    // -----------------------------------------------------------------------

    fn make_llm_response_with_tool_calls(tool_names: &[&str]) -> LLMResponse {
        use awaken_contract::contract::message::ToolCall;
        LLMResponse::success(StreamResult {
            content: vec![ContentBlock::text("calling tools")],
            tool_calls: tool_names
                .iter()
                .enumerate()
                .map(|(i, name)| ToolCall::new(format!("c{}", i), *name, serde_json::json!({})))
                .collect(),
            usage: Some(TokenUsage {
                prompt_tokens: Some(50),
                completion_tokens: Some(20),
                total_tokens: Some(70),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    #[tokio::test]
    async fn stop_condition_with_tool_calls_resets_errors() {
        let (store, runtime, env) = make_test_env(vec![Arc::new(ConsecutiveErrorsPolicy::new(3))]);

        // 2 errors
        for _ in 0..2 {
            let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
                .with_llm_response(make_llm_error());
            runtime.run_phase_with_context(&env, ctx).await.unwrap();
        }

        // Success with tool calls should reset
        let ctx = PhaseContext::new(Phase::AfterInference, runtime.store().snapshot())
            .with_llm_response(make_llm_response_with_tool_calls(&["echo", "search"]));
        runtime.run_phase_with_context(&env, ctx).await.unwrap();

        assert_eq!(
            store.read::<RunLifecycle>().unwrap().status,
            RunStatus::Running,
            "success with tool calls should reset errors"
        );
    }

    // -----------------------------------------------------------------------
    // MaxRoundsPlugin convenience tests
    // -----------------------------------------------------------------------

    #[test]
    fn max_rounds_plugin_descriptor_name() {
        let plugin = MaxRoundsPlugin::new(5);
        assert_eq!(plugin.descriptor().name, "stop-condition:max-rounds");
    }

    #[test]
    fn stop_condition_plugin_descriptor_name() {
        let plugin = StopConditionPlugin::new(vec![]);
        assert_eq!(plugin.descriptor().name, "stop-condition");
    }
}
