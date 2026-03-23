//! LLM retry policy with optional model fallback.
//!
//! Provides [`LlmRetryPolicy`] for configuring retry behavior and
//! [`RetryingExecutor`] which wraps any [`LlmExecutor`] to apply the policy.
//!
//! Delay between retries is **not** handled here because the async timer
//! (`tokio::time::sleep`) is only available as a dev-dependency. Callers
//! that need backoff should inject delay logic via a custom executor or
//! middleware.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::StreamResult;

/// Policy for retrying failed LLM inference.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LlmRetryPolicy {
    /// Maximum number of retry attempts (0 = no retry, only the initial attempt).
    pub max_retries: u32,
    /// Fallback model names to try in order after the primary model exhausts retries.
    pub fallback_models: Vec<String>,
}

impl Default for LlmRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            fallback_models: Vec::new(),
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
}

impl RetryingExecutor {
    /// Wrap an executor with a retry policy.
    pub fn new(inner: Arc<dyn LlmExecutor>, policy: LlmRetryPolicy) -> Self {
        Self { inner, policy }
    }

    /// Attempt execution with retries for a single model variant of the request.
    async fn try_with_retries(
        &self,
        request: &InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let mut last_error = None;

        for attempt in 0..=self.policy.max_retries {
            match self.inner.execute(request.clone()).await {
                Ok(result) => return Ok(result),
                Err(err) => {
                    if !is_retryable(&err) {
                        return Err(err);
                    }
                    last_error = Some(err);
                    if attempt == self.policy.max_retries {
                        break;
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
        let executor = RetryingExecutor::new(inner.clone(), LlmRetryPolicy::no_retry());

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        assert_eq!(inner.call_count(), 1);
    }

    #[tokio::test]
    async fn retry_succeeds_on_second_attempt() {
        let inner = Arc::new(FailNThenSucceed::new(1));
        let policy = LlmRetryPolicy::default().with_max_retries(2);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 2);
    }

    #[tokio::test]
    async fn retry_exhausts_all_attempts_returns_last_error() {
        let inner = Arc::new(FailNThenSucceed::new(100)); // never succeeds
        let policy = LlmRetryPolicy::default().with_max_retries(3);
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
        let policy = LlmRetryPolicy::default().with_max_retries(5);
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
        let policy = LlmRetryPolicy::default()
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
        // Fails 4 times then succeeds. With max_retries=1:
        // primary: 2 calls (fail), fallback: 2 calls (fail), then call 5 succeeds.
        // Actually we need a model-aware mock for this. Let's use a simpler approach:
        // fail 3 times (primary 2 + fallback first attempt), succeed on 4th.
        let inner = Arc::new(FailNThenSucceed::new(3));
        let policy = LlmRetryPolicy::default()
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
        let policy = LlmRetryPolicy::default().with_max_retries(3);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 1, "should call executor exactly once");
    }

    #[tokio::test]
    async fn retrying_executor_delegates_name() {
        let inner = Arc::new(FailNThenSucceed::new(0));
        let executor = RetryingExecutor::new(inner, LlmRetryPolicy::default());
        assert_eq!(executor.name(), "mock");
    }

    #[tokio::test]
    async fn non_retryable_error_during_fallback_stops_immediately() {
        // Primary fails with retryable, fallback fails with non-retryable (Cancelled).
        // Should stop without trying further fallbacks.
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
                    // Fallback returns non-retryable error
                    let _ = n;
                    Err(InferenceExecutionError::Cancelled)
                }
            }

            fn name(&self) -> &str {
                "primary-retryable-fallback-fatal"
            }
        }

        let inner = Arc::new(PrimaryRetryableFallbackFatal { calls: cc });
        let policy = LlmRetryPolicy::default()
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
            .with_fallback_model("model-b");
        assert_eq!(policy.max_retries, 5);
        assert_eq!(policy.fallback_models, vec!["model-a", "model-b"]);
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
        let policy = LlmRetryPolicy::default().with_max_retries(3);
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
        let policy = LlmRetryPolicy::default().with_max_retries(2);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 2);
    }

    #[tokio::test]
    async fn zero_retries_with_fallback_tries_fallback_once() {
        let inner = Arc::new(FailNThenSucceed::new(1)); // primary fails, fallback succeeds
        let policy = LlmRetryPolicy::default()
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
        let policy = LlmRetryPolicy::default().with_max_retries(1);
        // No fallback models
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        assert_eq!(inner.call_count(), 2); // initial + 1 retry
    }

    #[tokio::test]
    async fn all_error_types_handled() {
        // Test each retryable error type
        for error_fn in [
            (|_: u32| InferenceExecutionError::Provider("down".into())) as fn(u32) -> _,
            |_| InferenceExecutionError::RateLimited("429".into()),
            |_| InferenceExecutionError::Timeout("timeout".into()),
        ] {
            let inner = Arc::new(FailNThenSucceed::new(1).with_error(error_fn));
            let policy = LlmRetryPolicy::default().with_max_retries(2);
            let executor = RetryingExecutor::new(inner.clone(), policy);

            let result = executor.execute(test_request()).await;
            assert!(result.is_ok(), "should recover from retryable error");
        }
    }

    #[tokio::test]
    async fn max_retries_zero_and_no_fallback_just_one_attempt() {
        let inner = Arc::new(FailNThenSucceed::new(100));
        let policy = LlmRetryPolicy::no_retry();
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
        // This will always fail, but let's use a different mock that succeeds
        let inner = Arc::new(FailNThenSucceed::new(0)); // never fails
        let policy = LlmRetryPolicy::default()
            .with_max_retries(3)
            .with_fallback_model("fallback-a");
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 1, "should not attempt fallback");
        let _ = recorder; // suppress unused warning
    }
}
