use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use awaken_runtime_contract::contract::executor::{
    InferenceExecutionError, InferenceStream, LlmStreamEvent,
};
use futures::Stream;

use super::PoolExecutorInner;

pub(super) struct PoolObservedStream {
    inner: InferenceStream,
    pool: Arc<PoolExecutorInner>,
    session_key: String,
    attempt_key: Option<String>,
    member_idx: usize,
    finished: bool,
}

impl PoolObservedStream {
    pub(super) fn new(
        inner: InferenceStream,
        pool: Arc<PoolExecutorInner>,
        session_key: String,
        attempt_key: Option<String>,
        member_idx: usize,
    ) -> Self {
        Self {
            inner,
            pool,
            session_key,
            attempt_key,
            member_idx,
            finished: false,
        }
    }
}

impl Stream for PoolObservedStream {
    type Item = Result<LlmStreamEvent, InferenceExecutionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Err(err))) => {
                this.finished = true;
                this.pool.record_stream_attempt_failure_once(
                    &this.session_key,
                    this.attempt_key.as_deref(),
                    this.member_idx,
                    &err,
                    true,
                );
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                if !this.finished {
                    this.finished = true;
                    this.pool.record_stream_attempt_success(
                        &this.session_key,
                        this.attempt_key.as_deref(),
                        this.member_idx,
                    );
                }
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for PoolObservedStream {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.pool
            .record_stream_attempt_abandoned(self.attempt_key.as_deref(), self.member_idx);
    }
}
