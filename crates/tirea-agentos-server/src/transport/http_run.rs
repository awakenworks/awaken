use bytes::Bytes;
use serde::Serialize;
use std::future::Future;
use std::sync::Arc;
use tirea_agentos::contracts::AgentEvent;
use tirea_contract::{RuntimeInput, Transcoder};
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

use super::http_sse::HttpSseServerEndpoint;
use super::runtime_endpoint::{RunStarter, RuntimeEndpoint};
use super::{
    relay_binding, RelayCancellation, SessionId, TranscoderEndpoint, TransportBinding,
    TransportCapabilities,
};

/// Wire an HTTP SSE relay for a single agent run.
///
/// Sets up the full pipeline: runtime endpoint → protocol transcoder →
/// transport binding → SSE server endpoint, then spawns the relay and
/// event pump tasks.
///
/// Returns the SSE byte receiver to feed into an HTTP response body.
///
/// `on_relay_done` is called after the relay finishes, receiving
/// the SSE sender (for trailer support).
pub fn wire_http_sse_relay<E, F, Fut, ErrFmt>(
    run_starter: RunStarter,
    encoder: E,
    ingress_rx: mpsc::UnboundedReceiver<RuntimeInput>,
    thread_id: String,
    fanout: Option<broadcast::Sender<Bytes>>,
    resumable_downstream: bool,
    protocol_label: &'static str,
    on_relay_done: F,
    error_formatter: ErrFmt,
) -> mpsc::Receiver<Bytes>
where
    E: Transcoder<Input = AgentEvent> + 'static,
    E::Output: Serialize + Send + 'static,
    F: FnOnce(mpsc::Sender<Bytes>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
    ErrFmt: Fn(String) -> Bytes + Send + 'static,
{
    let (sse_tx, sse_rx) = mpsc::channel::<Bytes>(64);

    let upstream: Arc<HttpSseServerEndpoint<E::Output>> = match fanout {
        Some(f) => Arc::new(HttpSseServerEndpoint::with_fanout(
            ingress_rx,
            sse_tx.clone(),
            f,
        )),
        None => Arc::new(HttpSseServerEndpoint::new(ingress_rx, sse_tx.clone())),
    };

    let runtime_ep = Arc::new(RuntimeEndpoint::new(run_starter));
    let downstream = Arc::new(TranscoderEndpoint::new(runtime_ep, encoder));

    let binding = TransportBinding {
        session: SessionId { thread_id },
        caps: TransportCapabilities {
            upstream_async: true,
            downstream_streaming: true,
            single_channel_bidirectional: false,
            resumable_downstream,
        },
        upstream,
        downstream,
    };
    let relay_cancel = RelayCancellation::new();
    tokio::spawn(async move {
        if let Err(err) = relay_binding(binding, relay_cancel.clone()).await {
            warn!(error = %err, "{protocol_label} transport relay failed");
            let _ = sse_tx.send(error_formatter(err.to_string())).await;
        }
        on_relay_done(sse_tx).await;
    });

    sse_rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tirea_agentos::contracts::{AgentEvent, RunRequest, ToolCallDecision};
    use tirea_agentos::orchestrator::RunStream;
    use tirea_contract::Transcoder;

    /// Minimal encoder: maps each AgentEvent to a JSON string event.
    struct TestEncoder;

    impl Transcoder for TestEncoder {
        type Input = AgentEvent;
        type Output = String;

        fn prologue(&mut self) -> Vec<String> {
            vec!["[start]".to_string()]
        }

        fn transcode(&mut self, item: &AgentEvent) -> Vec<String> {
            match item {
                AgentEvent::TextDelta { delta } => vec![format!("text:{delta}")],
                _ => vec!["other".to_string()],
            }
        }

        fn epilogue(&mut self) -> Vec<String> {
            vec!["[end]".to_string()]
        }
    }

    fn fake_run_stream(
        events: Vec<AgentEvent>,
    ) -> (RunStream, mpsc::UnboundedReceiver<ToolCallDecision>) {
        let (decision_tx, decision_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(64);

        tokio::spawn(async move {
            for e in events {
                let _ = event_tx.send(e).await;
            }
        });

        let stream: Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send>> =
            Box::pin(async_stream::stream! {
                let mut rx = event_rx;
                while let Some(item) = rx.recv().await {
                    yield item;
                }
            });

        let run = RunStream {
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
            decision_tx,
            events: stream,
        };

        (run, decision_rx)
    }

    fn fake_starter(
        events: Vec<AgentEvent>,
    ) -> (RunStarter, mpsc::UnboundedReceiver<ToolCallDecision>) {
        let (run, decision_rx) = fake_run_stream(events);
        let starter: RunStarter =
            Box::new(move |_request| Box::pin(async move { Ok((run, None)) }));
        (starter, decision_rx)
    }

    fn test_run_request() -> RunRequest {
        RunRequest {
            agent_id: "test".into(),
            thread_id: None,
            run_id: None,
            parent_run_id: None,
            resource_id: None,
            state: None,
            messages: vec![],
            initial_decisions: vec![],
        }
    }

    fn collect_sse_strings(chunks: Vec<Bytes>) -> Vec<String> {
        chunks
            .into_iter()
            .filter_map(|b| {
                let s = String::from_utf8(b.to_vec()).ok()?;
                // SSE format: "data: <json>\n\n" — extract inner JSON string
                let trimmed = s.trim();
                let payload = trimmed.strip_prefix("data: ")?;
                serde_json::from_str::<String>(payload).ok()
            })
            .collect()
    }

    #[tokio::test]
    async fn events_flow_through_to_sse_bytes() {
        let events = vec![
            AgentEvent::TextDelta {
                delta: "hello".to_string(),
            },
            AgentEvent::TextDelta {
                delta: "world".to_string(),
            },
        ];
        let (starter, _decision_rx) = fake_starter(events);
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel::<RuntimeInput>();

        // Inject Run as first message
        ingress_tx
            .send(RuntimeInput::Run(test_run_request()))
            .unwrap();
        drop(ingress_tx);

        let mut sse_rx = wire_http_sse_relay(
            starter,
            TestEncoder,
            ingress_rx,
            "thread-1".to_string(),
            None,
            false,
            "test",
            |_sse_tx| async {},
            |msg| Bytes::from(format!("data: {{\"error\":\"{msg}\"}}\n\n")),
        );

        let mut chunks = Vec::new();
        while let Some(chunk) = sse_rx.recv().await {
            chunks.push(chunk);
        }

        let events = collect_sse_strings(chunks);
        assert_eq!(events[0], "[start]");
        assert_eq!(events[1], "text:hello");
        assert_eq!(events[2], "text:world");
        assert_eq!(events[3], "[end]");
        assert_eq!(events.len(), 4);
    }

    #[tokio::test]
    async fn on_relay_done_callback_is_invoked() {
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = called.clone();

        let (starter, _decision_rx) = fake_starter(vec![]);
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel::<RuntimeInput>();
        ingress_tx
            .send(RuntimeInput::Run(test_run_request()))
            .unwrap();
        drop(ingress_tx);

        let mut sse_rx = wire_http_sse_relay(
            starter,
            TestEncoder,
            ingress_rx,
            "thread-1".to_string(),
            None,
            false,
            "test",
            move |_sse_tx| async move {
                called_clone.store(true, Ordering::SeqCst);
            },
            |msg| Bytes::from(format!("data: {{\"error\":\"{msg}\"}}\n\n")),
        );

        // Drain to completion
        while sse_rx.recv().await.is_some() {}

        assert!(
            called.load(Ordering::SeqCst),
            "on_relay_done should be called"
        );
    }

    #[tokio::test]
    async fn trailer_via_callback_sse_tx() {
        let (starter, _decision_rx) = fake_starter(vec![AgentEvent::TextDelta {
            delta: "x".to_string(),
        }]);
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel::<RuntimeInput>();
        ingress_tx
            .send(RuntimeInput::Run(test_run_request()))
            .unwrap();
        drop(ingress_tx);

        let mut sse_rx = wire_http_sse_relay(
            starter,
            TestEncoder,
            ingress_rx,
            "thread-1".to_string(),
            None,
            false,
            "test",
            |sse_tx| async move {
                let _ = sse_tx.send(Bytes::from("data: [DONE]\n\n")).await;
            },
            |msg| Bytes::from(format!("data: {{\"error\":\"{msg}\"}}\n\n")),
        );

        let mut chunks = Vec::new();
        while let Some(chunk) = sse_rx.recv().await {
            chunks.push(chunk);
        }

        // Last chunk should be the trailer sent via callback
        let last = String::from_utf8(chunks.last().unwrap().to_vec()).unwrap();
        assert_eq!(last.trim(), "data: [DONE]");
    }

    #[tokio::test]
    async fn fanout_receives_all_sse_events() {
        let events = vec![AgentEvent::TextDelta {
            delta: "hi".to_string(),
        }];
        let (starter, _decision_rx) = fake_starter(events);
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel::<RuntimeInput>();
        let (fanout_tx, mut fanout_rx) = broadcast::channel::<Bytes>(64);

        ingress_tx
            .send(RuntimeInput::Run(test_run_request()))
            .unwrap();
        drop(ingress_tx);

        let mut sse_rx = wire_http_sse_relay(
            starter,
            TestEncoder,
            ingress_rx,
            "thread-1".to_string(),
            Some(fanout_tx),
            true,
            "test",
            |_sse_tx| async {},
            |msg| Bytes::from(format!("data: {{\"error\":\"{msg}\"}}\n\n")),
        );

        // Collect from SSE
        let mut sse_chunks = Vec::new();
        while let Some(chunk) = sse_rx.recv().await {
            sse_chunks.push(chunk);
        }

        // Collect from fanout (non-blocking, it already has all items)
        let mut fanout_chunks = Vec::new();
        while let Ok(chunk) = fanout_rx.try_recv() {
            fanout_chunks.push(chunk);
        }

        let sse_events = collect_sse_strings(sse_chunks);
        let fanout_events = collect_sse_strings(fanout_chunks);

        assert!(sse_events.contains(&"text:hi".to_string()));
        assert!(fanout_events.contains(&"text:hi".to_string()));
    }

    #[tokio::test]
    async fn decision_ingress_forwarded_to_run_decision_tx() {
        let (starter, mut decision_rx) = fake_starter(vec![AgentEvent::TextDelta {
            delta: "a".to_string(),
        }]);
        let (ingress_tx, ingress_rx) = mpsc::unbounded_channel::<RuntimeInput>();

        // Inject Run as first message, keep tx alive for decisions
        ingress_tx
            .send(RuntimeInput::Run(test_run_request()))
            .unwrap();

        let mut sse_rx = wire_http_sse_relay(
            starter,
            TestEncoder,
            ingress_rx,
            "thread-1".to_string(),
            None,
            false,
            "test",
            |_sse_tx| async {},
            |msg| Bytes::from(format!("data: {{\"error\":\"{msg}\"}}\n\n")),
        );

        // Send a decision through the ingress channel
        let decision = ToolCallDecision::resume("d1", serde_json::json!({"approved": true}), 1);
        ingress_tx.send(RuntimeInput::Decision(decision)).unwrap();
        drop(ingress_tx);

        // Drain SSE to let relay process
        while sse_rx.recv().await.is_some() {}

        // The decision should have been forwarded through the relay to decision_tx
        let received = decision_rx.try_recv();
        assert!(received.is_ok(), "decision should be forwarded to run");
        assert_eq!(received.unwrap().target_id, "d1");
    }
}
