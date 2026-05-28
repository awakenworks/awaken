//! LLM retry policy with exponential backoff.
//!
//! Provides [`LlmRetryPolicy`] for configuring retry behavior and
//! [`RetryingExecutor`] which wraps any [`LlmExecutor`] to apply the policy.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use awaken_runtime_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceStream, LlmExecutor,
};
use awaken_runtime_contract::contract::inference::StreamResult;

use super::circuit_breaker::CircuitBreaker;

/// Maximum backoff cap (8 seconds).
const MAX_BACKOFF_MS: u64 = 8_000;

/// Policy for retrying failed LLM inference.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LlmRetryPolicy {
    /// Maximum number of retry attempts (0 = no retry, only the initial attempt).
    pub max_retries: u32,
    /// Base delay in milliseconds for exponential backoff between retries.
    /// Actual delay = min(base_ms * 2^attempt, 8000ms). Set to 0 to disable backoff.
    #[serde(default = "default_backoff_base_ms")]
    pub backoff_base_ms: u64,
    /// Base delay for `Overloaded` errors, which signal provider-wide surges.
    /// Uses the same exponential curve and cap as `backoff_base_ms` but a
    /// longer base to give the provider more headroom.
    #[serde(default = "default_overloaded_backoff_base_ms")]
    pub overloaded_backoff_base_ms: u64,
    /// Maximum number of mid-stream retries (independent of `max_retries`).
    /// Applies only when a stream interruption is recovered by
    /// `execute_streaming`; the initial open of a stream is still governed
    /// by `max_retries`.
    #[serde(default = "default_max_stream_retries")]
    pub max_stream_retries: u32,
    /// Per-event idle window during streaming. If no delta arrives within
    /// this window the current attempt is treated as a stall and the
    /// recovery path is entered. Doubles for thinking/reasoning models.
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
}

fn default_backoff_base_ms() -> u64 {
    500
}

fn default_overloaded_backoff_base_ms() -> u64 {
    2_000
}

fn default_max_stream_retries() -> u32 {
    2
}

fn default_stream_idle_timeout_secs() -> u64 {
    60
}

impl Default for LlmRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 2,
            backoff_base_ms: default_backoff_base_ms(),
            overloaded_backoff_base_ms: default_overloaded_backoff_base_ms(),
            max_stream_retries: default_max_stream_retries(),
            stream_idle_timeout_secs: default_stream_idle_timeout_secs(),
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

    /// Set the backoff base delay in milliseconds.
    pub fn with_backoff_base_ms(mut self, ms: u64) -> Self {
        self.backoff_base_ms = ms;
        self
    }

    /// Set the backoff base delay for `Overloaded` errors in milliseconds.
    pub fn with_overloaded_backoff_base_ms(mut self, ms: u64) -> Self {
        self.overloaded_backoff_base_ms = ms;
        self
    }

    /// Set the maximum number of mid-stream retries.
    pub fn with_max_stream_retries(mut self, n: u32) -> Self {
        self.max_stream_retries = n;
        self
    }

    /// Set the per-event stream idle timeout in seconds.
    pub fn with_stream_idle_timeout_secs(mut self, secs: u64) -> Self {
        self.stream_idle_timeout_secs = secs;
        self
    }

    /// Compute the backoff delay for a given retry attempt (0-indexed).
    fn backoff_delay(&self, attempt: u32) -> Duration {
        Self::backoff_delay_with_base(self.backoff_base_ms, attempt)
    }

    /// Compute the backoff delay for an `Overloaded` error.
    fn overloaded_backoff_delay(&self, attempt: u32) -> Duration {
        Self::backoff_delay_with_base(self.overloaded_backoff_base_ms, attempt)
    }

    fn backoff_delay_with_base(base_ms: u64, attempt: u32) -> Duration {
        if base_ms == 0 {
            return Duration::ZERO;
        }
        let delay_ms = base_ms
            .saturating_mul(1u64 << attempt.min(16))
            .min(MAX_BACKOFF_MS);
        Duration::from_millis(delay_ms)
    }

    /// Select the delay to wait before the next retry attempt. Picks the
    /// larger of the error-type-specific exponential backoff and any
    /// provider-supplied `Retry-After` hint.
    pub fn delay_before_retry(&self, err: &InferenceExecutionError, attempt: u32) -> Duration {
        let base = match err {
            InferenceExecutionError::Overloaded { .. } => self.overloaded_backoff_delay(attempt),
            _ => self.backoff_delay(attempt),
        };
        match err.retry_after() {
            Some(hint) if hint > base => hint,
            _ => base,
        }
    }
}

/// Whether an error is retryable by the retry subsystem.
fn is_retryable(err: &InferenceExecutionError) -> bool {
    err.is_retryable()
}

/// An [`LlmExecutor`] wrapper that applies a [`LlmRetryPolicy`].
///
/// On transient failure the wrapper retries the inner executor up to
/// `policy.max_retries` times for the requested model.
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
                cb.check(&request.upstream_model)?;
            }

            match self.inner.execute(request.clone()).await {
                Ok(result) => {
                    if let Some(ref cb) = self.circuit_breaker {
                        cb.record_success(&request.upstream_model);
                    }
                    return Ok(result);
                }
                Err(err) => {
                    if err.counts_toward_circuit_breaker()
                        && let Some(ref cb) = self.circuit_breaker
                    {
                        cb.record_failure(&request.upstream_model);
                    }
                    if !is_retryable(&err) {
                        return Err(err);
                    }
                    if attempt == self.policy.max_retries {
                        last_error = Some(err);
                        break;
                    }
                    // Exponential backoff between retries (not before the first attempt).
                    let delay = self.policy.delay_before_retry(&err, attempt);
                    last_error = Some(err);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_error.expect("at least one attempt was made"))
    }

    /// Attempt to open a streaming response with retries for one model variant.
    ///
    /// Retries apply only while creating the stream. Once a provider has returned
    /// a stream, mid-stream errors are recovered by `execute_streaming` (see
    /// `loop_runner::inference`), not here.
    async fn try_stream_with_retries(
        &self,
        request: &InferenceRequest,
    ) -> Result<InferenceStream, InferenceExecutionError> {
        let mut last_error = None;

        for attempt in 0..=self.policy.max_retries {
            if let Some(ref cb) = self.circuit_breaker {
                cb.check(&request.upstream_model)?;
            }

            match self.inner.execute_stream(request.clone()).await {
                Ok(stream) => {
                    if let Some(ref cb) = self.circuit_breaker {
                        cb.record_success(&request.upstream_model);
                    }
                    return Ok(stream);
                }
                Err(err) => {
                    if err.counts_toward_circuit_breaker()
                        && let Some(ref cb) = self.circuit_breaker
                    {
                        cb.record_failure(&request.upstream_model);
                    }
                    if !is_retryable(&err) {
                        return Err(err);
                    }
                    if attempt == self.policy.max_retries {
                        last_error = Some(err);
                        break;
                    }
                    let delay = self.policy.delay_before_retry(&err, attempt);
                    last_error = Some(err);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_error.expect("at least one stream attempt was made"))
    }
}

#[async_trait]
impl LlmExecutor for RetryingExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.try_with_retries(&request).await
    }

    fn execute_stream(
        &self,
        request: InferenceRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<InferenceStream, InferenceExecutionError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move { self.try_stream_with_retries(&request).await })
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

/// Plugin config key for [`LlmRetryPolicy`] in `AgentSpec.sections["retry"]`.
pub struct RetryConfigKey;

impl awaken_runtime_contract::registry_spec::PluginConfigKey for RetryConfigKey {
    const KEY: &'static str = "retry";
    type Config = LlmRetryPolicy;
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::contract::content::ContentBlock;
    use awaken_runtime_contract::contract::inference::{StopReason, TokenUsage};
    use awaken_runtime_contract::contract::message::Message;
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
            upstream_model: "primary-model".into(),
            routing_key: None,
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
    async fn execute_stream_retries_stream_start_until_success() {
        let inner = Arc::new(FailNThenSucceed::new(1));
        let policy = test_policy().with_max_retries(2);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute_stream(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 2);
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

    #[test]
    fn default_policy_values() {
        let policy = LlmRetryPolicy::default();
        assert_eq!(policy.max_retries, 2);
        assert_eq!(policy.backoff_base_ms, 500);
        assert_eq!(policy.overloaded_backoff_base_ms, 2_000);
        assert_eq!(policy.max_stream_retries, 2);
        assert_eq!(policy.stream_idle_timeout_secs, 60);
    }

    #[test]
    fn no_retry_policy_values() {
        let policy = LlmRetryPolicy::no_retry();
        assert_eq!(policy.max_retries, 0);
    }

    #[test]
    fn rate_limit_error_is_retryable() {
        assert!(is_retryable(&InferenceExecutionError::rate_limited("429")));
    }

    #[test]
    fn overloaded_error_is_retryable() {
        assert!(is_retryable(&InferenceExecutionError::overloaded("529")));
    }

    #[test]
    fn context_overflow_is_not_retryable() {
        assert!(!is_retryable(&InferenceExecutionError::ContextOverflow(
            "too long".into()
        )));
    }

    #[test]
    fn context_overflow_does_not_count_toward_breaker() {
        let err = InferenceExecutionError::ContextOverflow("too long".into());
        assert!(!err.counts_toward_circuit_breaker());
    }

    #[test]
    fn invalid_request_does_not_count_toward_breaker() {
        assert!(
            !InferenceExecutionError::InvalidRequest("schema".into())
                .counts_toward_circuit_breaker()
        );
    }

    #[test]
    fn unauthorized_does_not_count_toward_breaker() {
        assert!(
            !InferenceExecutionError::Unauthorized("key".into()).counts_toward_circuit_breaker()
        );
    }

    #[test]
    fn all_models_unavailable_is_fail_fast() {
        let err = InferenceExecutionError::AllModelsUnavailable;
        assert!(!err.is_retryable());
        assert!(!err.counts_toward_circuit_breaker());
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
            .with_backoff_base_ms(100);
        assert_eq!(policy.max_retries, 5);
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
                .with_error(|_| InferenceExecutionError::rate_limited("rate limited")),
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
    async fn retry_budget_exhausted_returns_primary_error() {
        let inner = Arc::new(FailNThenSucceed::new(100));
        let policy = test_policy().with_max_retries(1);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        assert_eq!(inner.call_count(), 2); // initial + 1 retry
    }

    #[tokio::test]
    async fn all_error_types_handled() {
        for error_fn in [
            (|_: u32| InferenceExecutionError::Provider("down".into())) as fn(u32) -> _,
            |_| InferenceExecutionError::rate_limited("429"),
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
    async fn max_retries_zero_just_one_attempt() {
        let inner = Arc::new(FailNThenSucceed::new(100));
        let policy = LlmRetryPolicy::no_retry().with_backoff_base_ms(0);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        assert_eq!(inner.call_count(), 1);
    }

    #[tokio::test]
    async fn success_on_first_try_skips_retries() {
        let inner = Arc::new(FailNThenSucceed::new(0)); // never fails
        let policy = test_policy().with_max_retries(3);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        assert_eq!(inner.call_count(), 1, "should not retry after success");
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: retry budget exhaustion and policy serde
    // -----------------------------------------------------------------------

    #[test]
    fn retry_policy_serde_roundtrip() {
        let policy = LlmRetryPolicy::default()
            .with_max_retries(5)
            .with_backoff_base_ms(200)
            .with_overloaded_backoff_base_ms(4_000)
            .with_max_stream_retries(3)
            .with_stream_idle_timeout_secs(90);
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: LlmRetryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.max_retries, 5);
        assert_eq!(parsed.backoff_base_ms, 200);
        assert_eq!(parsed.overloaded_backoff_base_ms, 4_000);
        assert_eq!(parsed.max_stream_retries, 3);
        assert_eq!(parsed.stream_idle_timeout_secs, 90);
    }

    #[test]
    fn retry_policy_serde_default_backoff() {
        // Deserializing without optional fields should use defaults.
        let json = r#"{"max_retries":2}"#;
        let parsed: LlmRetryPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.backoff_base_ms, 500);
        assert_eq!(parsed.overloaded_backoff_base_ms, 2_000);
        assert_eq!(parsed.max_stream_retries, 2);
        assert_eq!(parsed.stream_idle_timeout_secs, 60);
    }

    #[test]
    fn retry_policy_rejects_model_fallback_fields() {
        let legacy_field = ["fallback", "models"].join("_");
        let parsed = serde_json::from_value::<LlmRetryPolicy>(
            serde_json::json!({ "max_retries": 2, legacy_field: [] }),
        );
        assert!(parsed.is_err());

        let removed_field = ["fallback", "upstream", "models"].join("_");
        let parsed = serde_json::from_value::<LlmRetryPolicy>(
            serde_json::json!({ "max_retries": 2, removed_field: [] }),
        );
        assert!(parsed.is_err());
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

    // -----------------------------------------------------------------------
    // Circuit breaker opens mid-retry
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn circuit_breaker_opens_during_retry_sequence() {
        use crate::engine::circuit_breaker::CircuitBreakerConfig;

        let cb = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(60),
            half_open_max: 1,
        }));
        let inner = Arc::new(FailNThenSucceed::new(100)); // always fails
        let policy = test_policy().with_max_retries(5);
        let executor = RetryingExecutor::new(inner.clone(), policy).with_circuit_breaker(cb);

        let result = executor.execute(test_request()).await;
        assert!(result.is_err());
        // 2 actual calls trip the CB (failure_threshold=2), 3rd attempt blocked by CB
        assert_eq!(inner.call_count(), 2);
    }

    // -----------------------------------------------------------------------
    // Phase 1: Retry-After, Overloaded base, AllModelsUnavailable, permanent
    //          errors bypass the circuit breaker.
    // -----------------------------------------------------------------------

    #[test]
    fn delay_before_retry_respects_retry_after_when_longer() {
        let policy = LlmRetryPolicy::default().with_backoff_base_ms(100);
        let err = InferenceExecutionError::RateLimited {
            message: "slow".into(),
            retry_after: Some(Duration::from_secs(5)),
        };
        // 100ms exp backoff at attempt 0 < 5s hint → hint wins.
        assert_eq!(policy.delay_before_retry(&err, 0), Duration::from_secs(5));
    }

    #[test]
    fn delay_before_retry_uses_exponential_when_longer_than_retry_after() {
        let policy = LlmRetryPolicy::default().with_backoff_base_ms(10_000);
        let err = InferenceExecutionError::RateLimited {
            message: "fast hint".into(),
            retry_after: Some(Duration::from_millis(100)),
        };
        // 10s base capped at 8s at attempt 0, still > 100ms hint.
        assert_eq!(
            policy.delay_before_retry(&err, 0),
            Duration::from_millis(MAX_BACKOFF_MS)
        );
    }

    #[test]
    fn delay_before_retry_uses_overloaded_base_for_overloaded_errors() {
        let policy = LlmRetryPolicy::default()
            .with_backoff_base_ms(500)
            .with_overloaded_backoff_base_ms(2_000);
        let overloaded = InferenceExecutionError::overloaded("surge");
        // At attempt 0 the overloaded base dominates: 2000ms vs 500ms.
        assert_eq!(
            policy.delay_before_retry(&overloaded, 0),
            Duration::from_millis(2_000)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_retry_after_waits_hint_duration() {
        let inner = Arc::new(FailNThenSucceed::new(1).with_error(|_| {
            InferenceExecutionError::RateLimited {
                message: "slow down".into(),
                retry_after: Some(Duration::from_secs(3)),
            }
        }));
        let policy = LlmRetryPolicy::default()
            .with_max_retries(2)
            .with_backoff_base_ms(10); // short base so Retry-After dominates
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let start = tokio::time::Instant::now();
        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_secs(3),
            "expected >=3s retry-after wait, got {elapsed:?}"
        );
        assert_eq!(inner.call_count(), 2);
    }

    #[tokio::test]
    async fn context_overflow_error_is_not_retried() {
        let inner =
            Arc::new(FailNThenSucceed::new(5).with_error(|_| {
                InferenceExecutionError::ContextOverflow("prompt too long".into())
            }));
        let policy = test_policy().with_max_retries(3);
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let result = executor.execute(test_request()).await;
        assert!(matches!(
            result,
            Err(InferenceExecutionError::ContextOverflow(_))
        ));
        assert_eq!(inner.call_count(), 1, "permanent error must not retry");
    }

    #[tokio::test]
    async fn context_overflow_does_not_trip_circuit_breaker() {
        use crate::engine::circuit_breaker::CircuitBreakerConfig;

        let inner = Arc::new(
            FailNThenSucceed::new(100)
                .with_error(|_| InferenceExecutionError::ContextOverflow("too long".into())),
        );
        let cb = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(60),
            half_open_max: 1,
        }));

        let policy = test_policy().with_max_retries(0);
        let executor =
            RetryingExecutor::new(inner.clone(), policy).with_circuit_breaker(cb.clone());

        // Five independent calls: none should trip the breaker.
        for _ in 0..5 {
            let _ = executor.execute(test_request()).await;
        }
        assert!(
            cb.check("primary-model").is_ok(),
            "ContextOverflow must not increment the breaker"
        );
    }

    #[tokio::test]
    async fn open_circuit_short_circuits_current_model() {
        use crate::engine::circuit_breaker::CircuitBreakerConfig;

        let cb = Arc::new(CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            cooldown: Duration::from_secs(60),
            half_open_max: 1,
        }));
        cb.record_failure("primary-model");

        let inner = Arc::new(FailNThenSucceed::new(0)); // would succeed if called
        let policy = test_policy().with_max_retries(2);
        let executor =
            RetryingExecutor::new(inner.clone(), policy).with_circuit_breaker(cb.clone());

        let result = executor.execute(test_request()).await;
        assert!(
            matches!(result, Err(InferenceExecutionError::Provider(_))),
            "expected circuit breaker provider error, got {result:?}"
        );
        assert_eq!(inner.call_count(), 0, "no inner call should be made");
    }

    // Backoff sleep verification with paused time
    // -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn backoff_actually_sleeps() {
        let inner = Arc::new(FailNThenSucceed::new(2));
        let policy = LlmRetryPolicy::default()
            .with_max_retries(3)
            .with_backoff_base_ms(1000); // 1s base
        let executor = RetryingExecutor::new(inner.clone(), policy);

        let start = tokio::time::Instant::now();
        let result = executor.execute(test_request()).await;
        assert!(result.is_ok());

        // With paused time, elapsed reflects actual sleep calls:
        // Attempt 0 fails → sleep 1s (1000 * 2^0)
        // Attempt 1 fails → sleep 2s (1000 * 2^1)
        // Attempt 2 succeeds
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_secs(3),
            "expected >= 3s backoff, got {elapsed:?}"
        );
    }

    // ── Property-based tests ──

    mod proptest_retry {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn llm_retry_policy_serde_roundtrip(
                max_retries in 0u32..10,
                backoff_base_ms in 0u64..10000,
                overloaded_backoff_base_ms in 0u64..10000,
                max_stream_retries in 0u32..10,
                stream_idle_timeout_secs in 1u64..300,
            ) {
                let policy = LlmRetryPolicy {
                    max_retries,
                    backoff_base_ms,
                    overloaded_backoff_base_ms,
                    max_stream_retries,
                    stream_idle_timeout_secs,
                };
                let json = serde_json::to_string(&policy).unwrap();
                let parsed: LlmRetryPolicy = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(parsed.max_retries, max_retries);
                prop_assert_eq!(parsed.backoff_base_ms, backoff_base_ms);
                prop_assert_eq!(parsed.overloaded_backoff_base_ms, overloaded_backoff_base_ms);
                prop_assert_eq!(parsed.max_stream_retries, max_stream_retries);
                prop_assert_eq!(parsed.stream_idle_timeout_secs, stream_idle_timeout_secs);
            }

            #[test]
            fn backoff_delay_is_monotonically_non_decreasing(
                base_ms in 1u64..1000,
            ) {
                let policy = LlmRetryPolicy::default().with_backoff_base_ms(base_ms);
                let mut prev = Duration::ZERO;
                for attempt in 0..10u32 {
                    let delay = policy.backoff_delay(attempt);
                    prop_assert!(
                        delay >= prev,
                        "delay should be monotonically non-decreasing: attempt={attempt}, delay={delay:?}, prev={prev:?}"
                    );
                    prev = delay;
                }
            }

            #[test]
            fn backoff_delay_never_exceeds_cap(
                base_ms in 0u64..10000,
                attempt in 0u32..100,
            ) {
                let policy = LlmRetryPolicy::default().with_backoff_base_ms(base_ms);
                let delay = policy.backoff_delay(attempt);
                prop_assert!(
                    delay <= Duration::from_millis(MAX_BACKOFF_MS),
                    "delay {delay:?} exceeds {MAX_BACKOFF_MS}ms cap"
                );
            }

            #[test]
            fn backoff_delay_zero_base_always_zero(
                attempt in 0u32..100,
            ) {
                let policy = LlmRetryPolicy::default().with_backoff_base_ms(0);
                let delay = policy.backoff_delay(attempt);
                prop_assert_eq!(delay, Duration::ZERO);
            }
        }
    }
}
