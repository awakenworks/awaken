use std::time::Duration;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct NatsMailboxConfig {
    pub url: String,
    pub credentials: Option<String>,
    pub stream_name: String,
    pub consumer_name: String,
    pub dispatch_bucket: String,
    pub epoch_bucket: String,
    pub thread_index_bucket: String,
    pub sweeper_interval: Duration,
    pub dedup_window: Duration,
}

impl NatsMailboxConfig {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }
}

impl Default for NatsMailboxConfig {
    fn default() -> Self {
        Self {
            url: "nats://localhost:4222".to_string(),
            credentials: None,
            stream_name: "DISPATCH".to_string(),
            consumer_name: "dispatch-worker".to_string(),
            dispatch_bucket: "dispatch-state".to_string(),
            epoch_bucket: "thread-epoch".to_string(),
            thread_index_bucket: "thread-index".to_string(),
            sweeper_interval: Duration::from_secs(5),
            dedup_window: Duration::from_secs(120),
        }
    }
}
