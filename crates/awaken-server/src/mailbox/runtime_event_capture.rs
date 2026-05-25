use std::sync::Arc;

use awaken_contract::contract::commit_coordinator::{
    CanonicalEventStager, OutboxServerEventPublisher,
};
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::mailbox::RunDispatch;
use awaken_contract::{
    AgentEventNormalizationContext, DurableEventSink, RuntimeEventDurability,
    ScopedAgentEventNormalizer,
};
use awaken_runtime::EventBuffer;

use crate::transport::channel_sink::ReconnectableEventSink;

use super::{Mailbox, MailboxError};

#[derive(Clone)]
pub(super) struct RuntimeEventCaptureConfig {
    pub(super) mode: RuntimeEventDurability,
    pub(super) origin: String,
}
impl Mailbox {
    /// Enable optional canonical runtime event capture for this mailbox.
    ///
    /// The wrapper preserves the existing public constructor shape: callers opt in
    /// by applying this builder before putting the mailbox behind an `Arc`.
    pub fn with_runtime_event_capture(
        mut self,
        mode: RuntimeEventDurability,
        origin: impl Into<String>,
    ) -> Result<Self, MailboxError> {
        let origin = origin.into();
        if matches!(mode, RuntimeEventDurability::Disabled) {
            self.runtime_event_capture = None;
            return Ok(self);
        }
        if !self.executor.has_commit_coordinator() {
            return Err(MailboxError::Validation(
                "runtime event capture requires an executor with CommitCoordinator".to_string(),
            ));
        }
        AgentEventNormalizationContext::new("validation-thread", "validation-run", &origin)
            .map_err(|error| MailboxError::Validation(error.to_string()))?;
        self.runtime_event_capture = Some(RuntimeEventCaptureConfig { mode, origin });
        Ok(self)
    }

    /// Attach a publisher for advisory server-authored canonical events.
    pub fn with_server_event_publisher(
        mut self,
        publisher: Arc<dyn OutboxServerEventPublisher>,
        origin: impl Into<String>,
    ) -> Result<Self, MailboxError> {
        let origin = origin.into();
        AgentEventNormalizationContext::new("validation-thread", "validation-run", &origin)
            .map_err(|error| MailboxError::Validation(error.to_string()))?;
        self.server_event_publisher = Some(publisher);
        self.server_event_origin = origin;
        Ok(self)
    }

    fn wrap_runtime_event_sink(
        &self,
        inner: Arc<dyn EventSink>,
        thread_id: &str,
        run_id: &str,
        correlation_id: Option<String>,
        resumed: bool,
        event_buffer: Option<Arc<EventBuffer>>,
    ) -> Arc<dyn EventSink> {
        let Some(capture) = &self.runtime_event_capture else {
            return inner;
        };
        let context =
            AgentEventNormalizationContext::new(thread_id, run_id, capture.origin.clone()).expect(
                "mailbox runtime event capture context must use non-empty thread/run/origin",
            );
        let context = if let Some(c) = correlation_id {
            context.with_correlation_id(c)
        } else {
            context
        };
        let normalizer: Arc<ScopedAgentEventNormalizer> = Arc::new(if resumed {
            ScopedAgentEventNormalizer::new_resumed(context)
        } else {
            ScopedAgentEventNormalizer::new(context)
        });
        let buffer = event_buffer.expect("runtime event capture requires a per-run EventBuffer");
        Arc::new(DurableEventSink::new(
            inner,
            buffer as Arc<dyn CanonicalEventStager>,
            normalizer,
            capture.mode,
        ))
    }

    fn wrap_reconnectable_runtime_event_sink(
        &self,
        inner: Arc<ReconnectableEventSink>,
        thread_id: &str,
        run_id: &str,
        correlation_id: String,
        resumed: bool,
        event_buffer: Option<Arc<EventBuffer>>,
    ) -> Arc<dyn EventSink> {
        self.wrap_runtime_event_sink(
            inner as Arc<dyn EventSink>,
            thread_id,
            run_id,
            Some(correlation_id),
            resumed,
            event_buffer,
        )
    }

    pub(super) fn wrap_dispatch_runtime_event_sink(
        &self,
        inner: Arc<ReconnectableEventSink>,
        dispatch: &RunDispatch,
        correlation_id: String,
        resumed: bool,
        event_buffer: Option<Arc<EventBuffer>>,
    ) -> Arc<dyn EventSink> {
        self.wrap_reconnectable_runtime_event_sink(
            inner,
            &dispatch.thread_id,
            &dispatch.run_id,
            correlation_id,
            resumed,
            event_buffer,
        )
    }
}
