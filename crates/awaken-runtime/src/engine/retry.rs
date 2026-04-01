//! LLM retry policy with exponential backoff and optional model fallback.
//!
//! Provides [`LlmRetryPolicy`] for configuring retry behavior and
//! [`RetryingExecutor`] which wraps any [`LlmExecutor`] to apply the policy.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::StreamResult;

use super::circuit_breaker::CircuitBreaker;

/// Maximum backoff cap (8 seconds).
const MAX_BACKOFF_MS: u64 = 8_000;

/// Policy for retrying failed LLM inference.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LlmRetryPolicy {
    /// Maximum number of retry attempts (0 = no retry, only the initial attempt).
    pub max_retries: u32,
    /// Fallback model names to try in order after the primary model exhausts retries.
    pub fallback_models: Vec<String>,
    /// Base delay in milliseconds for exponential backoff between retries.
    /// Actual delay = min(base_ms * 2^attempt, 8000ms). Set to 0 to disable backoff.
    #[serde(default = "default_backoff_base_ms")]
    pub backoff_base_ms: u64,
}

fn default_backoff_base_ms() -> u64 {
    500
}

impl Default for LlmRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            fallback_models: Vec::new(),
            backoff_base_ms: default_backoff_base_ms(),
        }
    }
}

impl LlmRetryPolicy {
    /// Create a policy that never retries.
    pub fn no_retry() -> Self {
        Self {
            max_retries: 0,
            ..Default::default()
        }
    }

    /// Set the maximum number of retry attempts.
    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    /// Append a fallback model name.
    pub fn with_fallback_model(mut self, model: impl Into<String>) -> Self {
        self.fallback_models.push(model.into());
        self
    }

    /// Set the backoff base delay in milliseconds.
    pub fn with_backoff_base_ms(mut self, ms: u64) -> Self {
        self.backoff_base_ms = ms;
        self
    }

    /// Compute the backoff delay for a given retry attempt (0-indexed).
    fn backoff_delay(&self, attempt: u32) -> Duration {
        if self.backoff_base_ms == 0 {
            return Duration::ZERO;
        }
        let delay_ms = self
            .backoff_base_ms
            .saturating_mul(1u64 << attempt.min(16))
            .min(MAX_BACKOFF_MS);
        Duration::from_millis(delay_ms)
    }
}

/// Whether an error is retryable.
///
/// Transient errors (rate limits, timeouts, provider errors) are retryable.
/// Terminal errors (cancellation) are not.
fn is_retryable(err: &InferenceExecutionError) -> bool {
    matches!(
        err,
        InferenceExecutionError::RateLimited(_)
            | InferenceExecutionError::Timeout(_)
            | InferenceExecutionError::Provider(_)
    )
}

/// An [`LlmExecutor`] wrapper that applies a [`LlmRetryPolicy`].
///
/// On transient failure the wrapper retries the inner executor up to
/// `policy.max_retries` times for the primary model, then tries each
/// fallback model with the same retry budget.
pub struct RetryingExecutor {
    inner: Arc<dyn LlmExecutor>,
    policy: LlmRetryPolicy,
    circuit_breaker: Option<Arc<CircuitBreaker>>,
}

impl RetryingExecutor {
    /// Wrap an executor with a retry policy.
    pub fn new(inner: Arc<dyn LlmExecutor>, policy: LlmRetryPolicy) -> Self {
        Self {
            inner,
            policy,
            circuit_breaker: None,
        }
    }

    /// Attach a circuit breaker that is checked before each attempt.
    pub fn with_circuit_breaker(mut self, cb: Arc<CircuitBreaker>) -> Self {
        self.circuit_breaker = Some(cb);
        self
    }

    /// Attempt execution with retries for a single model variant of the request.
    async fn try_with_retries(
        &self,
        request: &InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut last_error = None;

        for attempt in 0..=self.policy.max_retries {
            // Check circuit breaker before each attempt.
            if let Some(ref cb) = self.circuit_breaker {
                cb.check(&request.model)?;
            }

            match self.inner.execute(request.clone()).await {
                Ok(result) => {
                    if let Some(ref cb) = self.circuit_breaker {
                        cb.record_success(&request.model);
                    }
                    return Ok(result);
                }
                Err(err) => {
                    if let Some(ref cb) = self.circuit_breaker {
                        cb.record_failure(&request.model);
                    }
                    if !is_retryable(&err) {
                        return Err(err);
                    }
                    last_error = Some(err);
                    if attempt == self.policy.max_retries {
                        break;
                    }
                    // Exponential backoff between retries (not before the first attempt).
                    let delay = self.policy.backoff_delay(attempt);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_error.expect("at least one attempt was made"))
    }
}

#[async_trait]
impl LlmExecutor for RetryingExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        // Try primary model.
        match self.try_with_retries(&request).await {
            Ok(result) => return Ok(result),
            Err(err) if !is_retryable(&err) || self.policy.fallback_models.is_empty() => {
                return Err(err);
            }
            Err(_) => {}
        }

        // Try fallback models in order.
        let mut last_error = None;
        for (i, fallback_model) in self.policy.fallback_models.iter().enumerate() {
            let mut fallback_request = request.clone();
            fallback_request.model = fallback_model.clone();

            match self.try_with_retries(&fallback_request).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    let is_last = i == self.policy.fallback_models.len() - 1;
                    if !is_retryable(&err) || is_last {
                        last_error = Some(err);
                        break;
                    }
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.expect("at least one fallback was attempted"))
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

/// Plugin config key for [`LlmRetryPolicy`] in `AgentSpec.sections["retry"]`.
pub struct RetryConfigKey;

impl awaken_contract::registry_spec::PluginConfigKey for RetryConfigKey {
    const KEY: &'static str = "retry";
    type Config = LlmRetryPolicy;
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::inference::{StopReason, TokenUsage};
    use awaken_contract::contract::message::Message;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// All test policies use zero backoff so tests run instantly.
    fn test_policy() -> LlmRetryPolicy {
        LlmRetryPolicy::default().with_backoff_base_ms(0)
    }

    /// Mock executor that fails a configurable number of times before succeeding.
    struct FailNThenSucceed {
        fail_count: u32,
        error_kind: fn(u32) -> InferenceExecutionError,
        calls: AtomicU32,
    }

    impl FailNThenSucceed {
        fn new(fail_count: u32) -> Self {
            Self {
                fail_count,
                error_kind: |_| InferenceExecutionError::Provider("transient".into()),
                calls: AtomicU32::new(0),
            }
        }

        fn with_error(mut self, f: fn(u32) -> InferenceExecutionError) -> Self {
            self.error_kind = f;
            self
        }

        fn call_count(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    fn ok_result() -> StreamResult {
        StreamResult {
            content: vec![ContentBlock::text("ok")],
            tool_calls: vec![],
            usage: Some(TokenUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
                ..Default::default()
            }),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        }
    }

    fn test_request() -> InferenceRequest {
        InferenceRequest {
            model: "primary-model".into(),
            messages: vec![Message::user("hello")],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        }
    }

    #[async_trait]
    impl LlmExecutor for FailNThenSucceed {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.fail_count {
                Err((self.error_kind)(call))
            } else {
                Ok(ok_result())
            }
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    /// Mock that records which model was requested and always fails.
    struct ModelRecorder {
        models: std::sync::Mutex<Vec<String>>,
        error: InferenceExecutionError,
    }

    impl ModelRecorder {
        fn always_fail_with(err: InferenceExecutionError) -> Self {
            Self {
                models: std::sync::Mutex::new(Vec::new()),
                error: err,
            }
        }

        fn recorded_models(&self) -> Vec<String> {
            self.models.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmExecutor for ModelRecorder {
        async fn execute(
            &self,
            request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            self.models.lock().unwrap().push(request.model.clone());
            match &self.error {
                InferenceExecutionError::Provider(s) => {
                    Err(InferenceExecutionError::Provider(s.clone()))
                }
                InferenceExecutionError::RateLimited(s) => {
                    Err(InferenceExecutionError::RateLimited(s.clone()))
                }
                InferenceExecutionError::Timeout(s) => {
                    Err(InferenceExecutionError::Timeout(s.clone()))
                }
                InferenceExecutionError::Cancelled => Err(InferenceExecutionError::Cancelled),
            }
        }

        fn name(&self) -> &str {
            "model-recorder"
        }
    }

    #[tokio::test]
    async fn no_retry_policy_first_failure_is_terminal() {
        let inner = Arc::new(FailNThenSucceed::new(1));
        let executor = RetryingExecutor::new(
            inner.clone(),
            LlmRetryPolicy::no_retry().with_backoff_base_ms(0),
        );

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        assert_eq!(inner.call_count(), 1);
    }

    #[tokio::test]
    async fn retry_succeeds_on_second_attempt() {
        let inner = Arc::new(FailNThenSucceed::new(1));
        let policy = test_policy().with_max_retries(2);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 2);
    }

    #[tokio::test]
    async fn retry_exhausts_all_attempts_returns_last_error() {
        let inner = Arc::new(FailNThenSucceed::new(100)); // never succeeds
        let policy = test_policy().with_max_retries(3);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        // 1 initial + 3 retries = 4 total
        assert_eq!(inner.call_count(), 4);
    }

    #[tokio::test]
    async fn non_retryable_error_is_not_retried() {
        let inner =
            Arc::new(FailNThenSucceed::new(1).with_error(|_| InferenceExecutionError::Cancelled));
        let policy = test_policy().with_max_retries(5);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        assert_eq!(inner.call_count(), 1);
    }

    #[tokio::test]
    async fn fallback_model_used_after_primary_exhausts_retries() {
        let inner = Arc::new(ModelRecorder::always_fail_with(
            InferenceExecutionError::RateLimited("overloaded".into()),
        ));
        let policy = test_policy()
            .with_max_retries(1)
            .with_fallback_model("fallback-a")
            .with_fallback_model("fallback-b");
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());

        let models = inner.recorded_models();
        // primary: 2 attempts (1 initial + 1 retry)
        // fallback-a: 2 attempts
        // fallback-b: 2 attempts
        assert_eq!(models.len(), 6);
        assert_eq!(models[0], "primary-model");
        assert_eq!(models[1], "primary-model");
        assert_eq!(models[2], "fallback-a");
        assert_eq!(models[3], "fallback-a");
        assert_eq!(models[4], "fallback-b");
        assert_eq!(models[5], "fallback-b");
    }

    #[tokio::test]
    async fn fallback_succeeds_after_primary_fails() {
        let inner = Arc::new(FailNThenSucceed::new(3));
        let policy = test_policy()
            .with_max_retries(1)
            .with_fallback_model("fallback-model");
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        // primary: 2 calls (fail), fallback: 1 fail + 1 success = 4
        assert_eq!(inner.call_count(), 4);
    }

    #[tokio::test]
    async fn succeeds_on_first_try_no_retry_needed() {
        let inner = Arc::new(FailNThenSucceed::new(0)); // never fails
        let policy = test_policy().with_max_retries(3);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 1, "should call executor exactly once");
    }

    #[tokio::test]
    async fn retrying_executor_delegates_name() {
        let inner = Arc::new(FailNThenSucceed::new(0));
        let executor = RetryingExecutor::new(inner, test_policy());
        assert_eq!(executor.name(), "mock");
    }

    #[tokio::test]
    async fn non_retryable_error_during_fallback_stops_immediately() {
        let call_count = Arc::new(AtomicU32::new(0));
        let cc = call_count.clone();

        struct PrimaryRetryableFallbackFatal {
            calls: Arc<AtomicU32>,
        }

        #[async_trait]
        impl LlmExecutor for PrimaryRetryableFallbackFatal {
            async fn execute(
                &self,
                request: InferenceRequest,
            ) -> Result<StreamResult, InferenceExecutionError> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if request.model.starts_with("primary") {
                    Err(InferenceExecutionError::Provider("down".into()))
                } else {
                    let _ = n;
                    Err(InferenceExecutionError::Cancelled)
                }
            }

            fn name(&self) -> &str {
                "primary-retryable-fallback-fatal"
            }
        }

        let inner = Arc::new(PrimaryRetryableFallbackFatal { calls: cc });
        let policy = test_policy()
            .with_max_retries(0)
            .with_fallback_model("fallback-a")
            .with_fallback_model("fallback-b");
        let executor = RetryingExecutor::new(inner, policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        // primary: 1 call, fallback-a: 1 call (Cancelled, stops immediately)
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn default_policy_values() {
        let policy = LlmRetryPolicy::default();
        assert_eq!(policy.max_retries, 2);
        assert!(policy.fallback_models.is_empty());
        assert_eq!(policy.backoff_base_ms, 500);
    }

    #[test]
    fn no_retry_policy_values() {
        let policy = LlmRetryPolicy::no_retry();
        assert_eq!(policy.max_retries, 0);
        assert!(policy.fallback_models.is_empty());
    }

    #[test]
    fn rate_limit_error_is_retryable() {
        assert!(is_retryable(&InferenceExecutionError::RateLimited(
            "429".into()
        )));
    }

    #[test]
    fn server_error_is_retryable() {
        assert!(is_retryable(&InferenceExecutionError::Provider(
            "500 internal".into()
        )));
    }

    #[test]
    fn timeout_error_is_retryable() {
        assert!(is_retryable(&InferenceExecutionError::Timeout(
            "timed out".into()
        )));
    }

    #[test]
    fn cancelled_error_is_not_retryable() {
        assert!(!is_retryable(&InferenceExecutionError::Cancelled));
    }

    #[test]
    fn builder_methods_chain() {
        let policy = LlmRetryPolicy::default()
            .with_max_retries(5)
            .with_fallback_model("model-a")
            .with_fallback_model("model-b")
            .with_backoff_base_ms(100);
        assert_eq!(policy.max_retries, 5);
        assert_eq!(policy.fallback_models, vec!["model-a", "model-b"]);
        assert_eq!(policy.backoff_base_ms, 100);
    }

    // -----------------------------------------------------------------------
    // Backoff delay tests
    // -----------------------------------------------------------------------

    #[test]
    fn backoff_delay_zero_base() {
        let policy = LlmRetryPolicy::default().with_backoff_base_ms(0);
        assert_eq!(policy.backoff_delay(0), Duration::ZERO);
        assert_eq!(policy.backoff_delay(5), Duration::ZERO);
    }

    #[test]
    fn backoff_delay_exponential() {
        let policy = LlmRetryPolicy::default().with_backoff_base_ms(500);
        assert_eq!(policy.backoff_delay(0), Duration::from_millis(500)); // 500 * 2^0
        assert_eq!(policy.backoff_delay(1), Duration::from_millis(1000)); // 500 * 2^1
        assert_eq!(policy.backoff_delay(2), Duration::from_millis(2000)); // 500 * 2^2
        assert_eq!(policy.backoff_delay(3), Duration::from_millis(4000)); // 500 * 2^3
    }

    #[test]
    fn backoff_delay_caps_at_max() {
        let policy = LlmRetryPolicy::default().with_backoff_base_ms(500);
        // 500 * 2^4 = 8000 (at the cap)
        assert_eq!(policy.backoff_delay(4), Duration::from_millis(8000));
        // 500 * 2^5 = 16000, capped to 8000
        assert_eq!(policy.backoff_delay(5), Duration::from_millis(8000));
    }

    // -----------------------------------------------------------------------
    // Circuit breaker integration tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn circuit_breaker_blocks_when_open() {
        use crate::engine::circuit_breaker::CircuitBreakerConfig;

        let inner = Arc::new(FailNThenSucceed::new(100));
        let cb = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: std::time::Duration::from_secs(60),
            half_open_max: 1,
        }));

        // Pre-open the circuit breaker
        cb.record_failure("primary-model");
        cb.record_failure("primary-model");

        let policy = test_policy().with_max_retries(3);
        let executor = RetryingExecutor::new(inner.clone(), policy).with_circuit_breaker(cb);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        // Should not have called inner at all — circuit breaker rejected
        assert_eq!(inner.call_count(), 0);
    }

    #[tokio::test]
    async fn circuit_breaker_records_success() {
        use crate::engine::circuit_breaker::CircuitBreakerConfig;

        let inner = Arc::new(FailNThenSucceed::new(0));
        let cb = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: std::time::Duration::from_secs(60),
            half_open_max: 1,
        }));

        // Record one failure — not enough to trip
        cb.record_failure("primary-model");

        let policy = test_policy().with_max_retries(1);
        let executor =
            RetryingExecutor::new(inner.clone(), policy).with_circuit_breaker(cb.clone());

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());

        // After success, a subsequent failure should not trip (counter was reset)
        cb.record_failure("primary-model");
        assert!(cb.check("primary-model").is_ok());
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: additional retry policy tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn retry_on_rate_limit_then_succeed() {
        let inner = Arc::new(
            FailNThenSucceed::new(2)
                .with_error(|_| InferenceExecutionError::RateLimited("rate limited".into())),
        );
        let policy = test_policy().with_max_retries(3);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 3); // 2 failures + 1 success
    }

    #[tokio::test]
    async fn retry_on_timeout_then_succeed() {
        let inner = Arc::new(
            FailNThenSucceed::new(1)
                .with_error(|_| InferenceExecutionError::Timeout("timed out".into())),
        );
        let policy = test_policy().with_max_retries(2);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 2);
    }

    #[tokio::test]
    async fn zero_retries_with_fallback_tries_fallback_once() {
        let inner = Arc::new(FailNThenSucceed::new(1)); // primary fails, fallback succeeds
        let policy = test_policy()
            .with_max_retries(0)
            .with_fallback_model("fallback");
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 2); // primary once + fallback once
    }

    #[tokio::test]
    async fn no_fallbacks_configured_returns_primary_error() {
        let inner = Arc::new(FailNThenSucceed::new(100));
        let policy = test_policy().with_max_retries(1);
        // No fallback models
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        assert_eq!(inner.call_count(), 2); // initial + 1 retry
    }

    #[tokio::test]
    async fn all_error_types_handled() {
        for error_fn in [
            (|_: u32| InferenceExecutionError::Provider("down".into())) as fn(u32) -> _,
            |_| InferenceExecutionError::RateLimited("429".into()),
            |_| InferenceExecutionError::Timeout("timeout".into()),
        ] {
            let inner = Arc::new(FailNThenSucceed::new(1).with_error(error_fn));
            let policy = test_policy().with_max_retries(2);
            let executor = RetryingExecutor::new(inner.clone(), policy);

            let result = executor.execute(test_request()).await;
            assert!(result.is_ok(), "should recover from retryable error");
        }
    }

    #[tokio::test]
    async fn max_retries_zero_and_no_fallback_just_one_attempt() {
        let inner = Arc::new(FailNThenSucceed::new(100));
        let policy = LlmRetryPolicy::no_retry().with_backoff_base_ms(0);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        assert_eq!(inner.call_count(), 1);
    }

    #[tokio::test]
    async fn success_on_first_try_no_fallback_attempted() {
        let recorder = Arc::new(ModelRecorder::always_fail_with(
            InferenceExecutionError::Provider("down".into()),
        ));
        let inner = Arc::new(FailNThenSucceed::new(0)); // never fails
        let policy = test_policy()
            .with_max_retries(3)
            .with_fallback_model("fallback-a");
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 1, "should not attempt fallback");
        let _ = recorder; // suppress unused warning
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: retry budget exhaustion and policy serde
    // -----------------------------------------------------------------------

    #[test]
    fn retry_policy_serde_roundtrip() {
        let policy = LlmRetryPolicy::default()
            .with_max_retries(5)
            .with_fallback_model("fallback-a")
            .with_fallback_model("fallback-b")
            .with_backoff_base_ms(200);
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: LlmRetryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.max_retries, 5);
        assert_eq!(parsed.fallback_models, vec!["fallback-a", "fallback-b"]);
        assert_eq!(parsed.backoff_base_ms, 200);
    }

    #[test]
    fn retry_policy_serde_default_backoff() {
        // Deserializing without backoff_base_ms should use the default.
        let json = r#"{"max_retries":2,"fallback_models":[]}"#;
        let parsed: LlmRetryPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.backoff_base_ms, 500);
    }

    #[tokio::test]
    async fn retry_budget_exact_boundary() {
        let inner = Arc::new(FailNThenSucceed::new(2));
        let policy = test_policy().with_max_retries(2);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 3);
    }

    #[tokio::test]
    async fn retry_budget_one_over_boundary() {
        let inner = Arc::new(FailNThenSucceed::new(3));
        let policy = test_policy().with_max_retries(2);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        assert_eq!(inner.call_count(), 3, "1 initial + 2 retries = 3 calls");
    }
}
