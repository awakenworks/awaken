//! Transport-agnostic event relay pipeline.
//!
//! Consumes [`AgentEvent`]s from a channel, runs them through a [`Transcoder`],
//! and yields serialized output items as a stream. Both SSE and NATS transports
//! consume this shared pipeline.

use serde::Serialize;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::transport::Transcoder;

/// Shared relay logic: prologue -> transcode each event -> epilogue.
///
/// The `event_stream` parameter accepts any `futures::Stream` of `AgentEvent`.
/// Both unbounded and bounded receivers can be adapted via wrapper streams.
pub fn relay_events_stream<E, S>(
    mut encoder: E,
    event_stream: S,
) -> impl futures::Stream<Item = Vec<u8>> + Send + 'static
where
    E: Transcoder<Input = AgentEvent> + 'static,
    E::Output: Serialize + Send + 'static,
    S: futures::Stream<Item = AgentEvent> + Send + Unpin + 'static,
{
    use futures::StreamExt;
    let mut event_stream = event_stream;
    async_stream::stream! {
        // Emit prologue
        for item in encoder.prologue() {
            if let Ok(bytes) = serde_json::to_vec(&item) {
                yield bytes;
            }
        }

        // Transcode each agent event
        while let Some(event) = event_stream.next().await {
            for item in encoder.transcode(&event) {
                if let Ok(bytes) = serde_json::to_vec(&item) {
                    yield bytes;
                }
            }
        }

        // Emit epilogue
        for item in encoder.epilogue() {
            if let Ok(bytes) = serde_json::to_vec(&item) {
                yield bytes;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::transport::Identity;
    use futures::StreamExt;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    #[tokio::test]
    async fn relay_events_identity_transcoder() {
        let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
        let encoder = Identity::<AgentEvent>::default();
        let stream = relay_events_stream(encoder, UnboundedReceiverStream::new(rx));
        tokio::pin!(stream);

        tx.send(AgentEvent::TextDelta {
            delta: "hello".into(),
        })
        .unwrap();
        drop(tx);

        let items: Vec<Vec<u8>> = stream.collect().await;
        assert_eq!(items.len(), 1);
        let json = String::from_utf8(items[0].clone()).unwrap();
        assert!(json.contains("text_delta"));
        assert!(json.contains("hello"));
    }

    #[tokio::test]
    async fn relay_events_with_prologue_epilogue() {
        use serde_json::Value;

        struct TestTranscoder;
        impl Transcoder for TestTranscoder {
            type Input = AgentEvent;
            type Output = Value;

            fn prologue(&mut self) -> Vec<Value> {
                vec![serde_json::json!({"type": "start"})]
            }

            fn transcode(&mut self, _item: &AgentEvent) -> Vec<Value> {
                vec![serde_json::json!({"type": "event"})]
            }

            fn epilogue(&mut self) -> Vec<Value> {
                vec![serde_json::json!({"type": "end"})]
            }
        }

        let (tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
        let stream = relay_events_stream(TestTranscoder, UnboundedReceiverStream::new(rx));
        tokio::pin!(stream);

        tx.send(AgentEvent::StepEnd).unwrap();
        drop(tx);

        let items: Vec<Vec<u8>> = stream.collect().await;
        assert_eq!(items.len(), 3);

        let first = String::from_utf8(items[0].clone()).unwrap();
        assert!(first.contains("start"));
        let last = String::from_utf8(items[2].clone()).unwrap();
        assert!(last.contains("end"));
    }

    #[tokio::test]
    async fn relay_events_empty_stream() {
        let (_tx, rx) = mpsc::unbounded_channel::<AgentEvent>();
        let encoder = Identity::<AgentEvent>::default();
        let stream = relay_events_stream(encoder, UnboundedReceiverStream::new(rx));
        tokio::pin!(stream);

        drop(_tx);

        let items: Vec<Vec<u8>> = stream.collect().await;
        // Identity has no prologue/epilogue, no events = empty
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn relay_events_bounded_works() {
        let (tx, rx) = mpsc::channel::<AgentEvent>(16);
        let encoder = Identity::<AgentEvent>::default();
        let stream = relay_events_stream(encoder, tokio_stream::wrappers::ReceiverStream::new(rx));
        tokio::pin!(stream);

        tx.send(AgentEvent::TextDelta {
            delta: "bounded".into(),
        })
        .await
        .unwrap();
        drop(tx);

        let items: Vec<Vec<u8>> = stream.collect().await;
        assert_eq!(items.len(), 1);
        let json = String::from_utf8(items[0].clone()).unwrap();
        assert!(json.contains("bounded"));
    }
}
