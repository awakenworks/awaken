use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use awaken_contract::contract::executor::{InferenceExecutionError, InferenceRequest, LlmExecutor};
use awaken_contract::contract::inference::StreamResult;
use awaken_contract::contract::message::Message;
use awaken_runtime::engine::{LlmRetryPolicy, RetryingExecutor};

struct AlwaysUnauthorized {
    calls: AtomicU32,
}

impl AlwaysUnauthorized {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
        }
    }

    fn call_count(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmExecutor for AlwaysUnauthorized {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(InferenceExecutionError::Unauthorized(
            "pre_consume_token_quota_failed: quota exhausted".into(),
        ))
    }

    fn name(&self) -> &str {
        "always-unauthorized"
    }
}

fn request() -> InferenceRequest {
    InferenceRequest {
        upstream_model: "primary-model".into(),
        routing_key: None,
        messages: vec![Message::user("hello")],
        tools: Vec::new(),
        system: Vec::new(),
        overrides: None,
        enable_prompt_cache: false,
    }
}

#[tokio::test]
async fn quota_unauthorized_error_is_not_retried() {
    let inner = Arc::new(AlwaysUnauthorized::new());
    let executor = RetryingExecutor::new(
        inner.clone(),
        LlmRetryPolicy::default()
            .with_max_retries(3)
            .with_backoff_base_ms(0),
    );

    let result = executor.execute(request()).await;

    assert!(matches!(
        result,
        Err(InferenceExecutionError::Unauthorized(ref message))
            if message.contains("pre_consume_token_quota_failed")
    ));
    assert_eq!(inner.call_count(), 1, "quota 403 must not retry");
}
