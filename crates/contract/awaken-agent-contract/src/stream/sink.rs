use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("stream sink is closed")]
    Closed,
}

/// Best-effort live progress delivery (G10). `&self` + `Send + Sync` so one sink
/// can be shared across the run as `Arc<dyn Sink>`; a sink failure never mutates
/// committed runtime truth.
#[async_trait]
pub trait Sink: Send + Sync {
    async fn send(&self, event: crate::stream::event::Event) -> Result<(), Error>;

    /// Deliver the same event with optional Runtime observation context.
    ///
    /// The default deliberately lowers to the historical [`Self::send`] port so
    /// existing external adapters remain source-compatible. Context-aware
    /// adapters override this method and forward the same [`Observation`](crate::stream::event::Observation)
    /// through the existing live channel.
    async fn send_observation(
        &self,
        observation: crate::stream::event::Observation,
    ) -> Result<(), Error> {
        self.send(observation.event).await
    }
}
