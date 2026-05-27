use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, InferenceStream, LlmExecutor,
};
use awaken_contract::contract::inference::StreamResult;

use super::PoolExecutor;
use super::pool_observed_stream::PoolObservedStream;

#[async_trait]
impl LlmExecutor for PoolExecutor {
    async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let inner = &self.inner;
        let session_key = inner.session_key(&request);
        let mut idx = inner.select_active(&session_key);
        let mut tried = vec![false; inner.members.len()];
        loop {
            tried[idx] = true;
            if let Err(err) = inner.check_member(idx) {
                match inner.next_on_unavailable(&session_key, idx, &tried) {
                    Some(next) => {
                        idx = next;
                        continue;
                    }
                    None => return Err(inner.no_member_available_error(err, &tried)),
                }
            }

            let req = inner.request_for(idx, &request);
            match inner.members[idx].executor.execute(req).await {
                Ok(result) => {
                    inner.breaker.record_success(&inner.members[idx].model_id);
                    inner.reset_switch_budget(&session_key);
                    return Ok(result);
                }
                Err(err) => {
                    inner.record_failure(idx, &err);
                    match inner.next_on_error(&session_key, idx, &err, &tried) {
                        Some(next) => idx = next,
                        None if inner.router.should_switch_on_error(&err) => {
                            return Err(inner.error_driven_no_member_available_error(err, &tried));
                        }
                        None => return Err(err),
                    }
                }
            }
        }
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
        Box::pin(async move {
            let inner = Arc::clone(&self.inner);
            let session_key = inner.session_key(&request);
            let attempt_key = inner.stream_attempt_key(&session_key, &request);
            let mut idx = inner.select_active(&session_key);
            let mut tried = inner.stream_tried_mask(attempt_key.as_deref());
            loop {
                tried[idx] = true;
                if let Err(err) = inner.check_member(idx) {
                    match inner.next_on_unavailable(&session_key, idx, &tried) {
                        Some(next) => {
                            idx = next;
                            continue;
                        }
                        None => return Err(inner.no_member_available_error(err, &tried)),
                    }
                }

                let req = inner.request_for(idx, &request);
                match inner.members[idx].executor.execute_stream(req).await {
                    Ok(stream) => {
                        inner.mark_stream_active(attempt_key.as_deref(), idx);
                        let observed = PoolObservedStream::new(
                            stream,
                            Arc::clone(&inner),
                            session_key,
                            attempt_key,
                            idx,
                        );
                        return Ok(Box::pin(observed) as InferenceStream);
                    }
                    Err(err) => {
                        inner.record_failure(idx, &err);
                        inner.mark_stream_open_failure(attempt_key.as_deref(), idx);
                        match inner.next_on_error(&session_key, idx, &err, &tried) {
                            Some(next) => {
                                idx = next;
                                if attempt_key.is_some() {
                                    inner.merge_stream_tried_mask(
                                        &mut tried,
                                        attempt_key.as_deref(),
                                    );
                                }
                            }
                            None if inner.router.should_switch_on_error(&err) => {
                                return Err(
                                    inner.error_driven_no_member_available_error(err, &tried)
                                );
                            }
                            None => return Err(err),
                        }
                    }
                }
            }
        })
    }

    fn name(&self) -> &str {
        &self.inner.pool_id
    }

    fn supports_upstream_model_override(&self) -> bool {
        false
    }

    fn record_stream_success(&self, request: &InferenceRequest) {
        let session_key = self.inner.session_key(request);
        let attempt_key = self.inner.stream_attempt_key(&session_key, request);
        if let Some(idx) = attempt_key.as_deref().and_then(|key| {
            self.inner
                .stream_attempts
                .read()
                .get(key)
                .and_then(|attempt| attempt.active)
        }) {
            self.inner
                .breaker
                .record_success(&self.inner.members[idx].model_id);
            self.inner.reset_switch_budget(&session_key);
        }
        self.inner.clear_stream_attempt(attempt_key.as_deref());
    }

    fn record_stream_failure(&self, request: &InferenceRequest, err: &InferenceExecutionError) {
        self.inner.record_stream_member_failure(request, err);
    }
}
