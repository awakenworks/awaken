//! A caller-declared inference budget for direct adapter calls in tests.
//!
//! Adapters own no timeout; a test that needs tighter bounds than the default
//! budget declares them here, exactly as Runtime or a probe would.
use std::time::Duration;

use awaken_runtime_contract::llm::{
    ModelAttemptTiming, ModelInferenceBudget, ModelInferenceClock, TokioModelInferenceClock,
};
use awaken_runtime_contract::model_operation::with_model_invocation_context;

/// One attempt whose complete response must arrive within `response` and
/// whose stream may stay silent for at most `idle`.
pub fn budget(response: Duration, idle: Duration) -> ModelInferenceBudget {
    ModelInferenceBudget::new(response, response, idle, Duration::from_millis(1))
        .expect("test budget is ordered")
}

/// Run one direct adapter call under `budget`, starting its clock now.
pub async fn with_budget<T>(
    budget: ModelInferenceBudget,
    future: impl std::future::Future<Output = T>,
) -> T {
    let timing = ModelAttemptTiming::standalone(TokioModelInferenceClock.now(), budget)
        .expect("test budget is representable");
    with_model_invocation_context(None, timing, future).await
}
