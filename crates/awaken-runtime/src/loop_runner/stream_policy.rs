use std::time::Duration;

use crate::engine::retry::LlmRetryPolicy;
use crate::registry::ResolvedAgent;
use awaken_runtime_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InterruptCause,
};

/// Fetch the active retry policy. Falls back to defaults for agents that
/// do not configure one. The agent-level override plumbing lives in
/// `engine::retry::RetryConfigKey`; for now, treat missing config as
/// "use defaults".
pub(super) fn stream_retry_policy_for(_agent: &ResolvedAgent) -> LlmRetryPolicy {
    LlmRetryPolicy::default()
}

/// Model-aware idle-stall threshold. Thinking / reasoning models receive
/// a 2x window to accommodate long silent reasoning phases.
pub(super) fn idle_timeout_for(request: &InferenceRequest, policy: &LlmRetryPolicy) -> Duration {
    let base = Duration::from_secs(policy.stream_idle_timeout_secs);
    let model = request.upstream_model.as_str();
    let name_hits = model.contains("thinking")
        || model.contains("reasoning")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4");
    let options_hits = request
        .overrides
        .as_ref()
        .and_then(|o| o.reasoning_effort.as_ref())
        .is_some();
    if name_hits || options_hits {
        base * 2
    } else {
        base
    }
}

pub(super) fn stream_retry_backoff(
    cause: &InterruptCause,
    attempt: u32,
    policy: &LlmRetryPolicy,
) -> Duration {
    // Mid-stream retries use the normal backoff; Overloaded-style
    // surges propagate through `RetryingExecutor` on stream open, not
    // here. For idle stalls, use a short delay to probe quickly.
    match cause {
        InterruptCause::IdleStall => Duration::from_millis(200),
        _ => policy.delay_before_retry(
            &InferenceExecutionError::Provider("mid-stream".into()),
            attempt,
        ),
    }
}
