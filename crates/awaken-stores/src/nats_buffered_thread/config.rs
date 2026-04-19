use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadConsistency {
    ReadYourWrites,
    Strong,
    Eventual,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NatsBufferedThreadConfig {
    pub url: String,
    pub credentials: Option<String>,
    pub stream_name: String,
    pub consumer_name: String,
    pub hot_bucket: String,
    pub max_age: Duration,
    pub flush_interval: Duration,
    pub flush_batch_size: usize,
    /// JetStream consumer ack-wait window. Messages pulled by a flusher that crashes
    /// before acking are redelivered after this interval. Keep moderate in production
    /// (e.g. 30s) so transient slow checkpoints don't trigger duplicate work; tests
    /// may set it short to exercise crash-recovery paths quickly.
    pub ack_wait: Duration,
    pub read_consistency: ReadConsistency,
}

impl NatsBufferedThreadConfig {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }
}

impl Default for NatsBufferedThreadConfig {
    fn default() -> Self {
        Self {
            url: "nats://localhost:4222".to_string(),
            credentials: None,
            stream_name: "THREADLOG".to_string(),
            consumer_name: "thread-flusher".to_string(),
            hot_bucket: "thread-hot".to_string(),
            max_age: Duration::from_secs(24 * 3600),
            flush_interval: Duration::from_millis(500),
            flush_batch_size: 64,
            ack_wait: Duration::from_secs(30),
            read_consistency: ReadConsistency::ReadYourWrites,
        }
    }
}
