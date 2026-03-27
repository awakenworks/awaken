//! Application state and server startup.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::contract::storage::ThreadRunStore;
use awaken_runtime::{AgentResolver, AgentRuntime};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::mailbox::Mailbox;
use crate::transport::replay_buffer::EventReplayBuffer;

/// Server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind address (e.g. "0.0.0.0:3000").
    pub address: String,
    /// Maximum SSE channel buffer size.
    #[serde(default = "default_sse_buffer")]
    pub sse_buffer_size: usize,
    /// Maximum number of SSE frames to buffer per run for reconnection replay.
    #[serde(default = "default_replay_buffer_capacity")]
    pub replay_buffer_capacity: usize,
}

fn default_sse_buffer() -> usize {
    64
}

fn default_replay_buffer_capacity() -> usize {
    1024
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:3000".to_string(),
            sse_buffer_size: default_sse_buffer(),
            replay_buffer_capacity: default_replay_buffer_capacity(),
        }
    }
}

/// Shared application state for all routes.
#[derive(Clone)]
pub struct AppState {
    /// Agent runtime for executing runs.
    pub runtime: Arc<AgentRuntime>,
    /// Unified mailbox service (persistent run queue).
    pub mailbox: Arc<Mailbox>,
    /// Unified thread + run persistence (atomic checkpoint).
    pub store: Arc<dyn ThreadRunStore>,
    /// Agent resolver for protocol-specific lookups.
    pub resolver: Arc<dyn AgentResolver>,
    /// Server configuration.
    pub config: ServerConfig,
    /// Per-run replay buffers for SSE stream resumption.
    pub replay_buffers: Arc<Mutex<HashMap<String, Arc<EventReplayBuffer>>>>,
}

impl AppState {
    /// Create a new AppState with all required dependencies.
    pub fn new(
        runtime: Arc<AgentRuntime>,
        mailbox: Arc<Mailbox>,
        store: Arc<dyn ThreadRunStore>,
        resolver: Arc<dyn AgentResolver>,
        config: ServerConfig,
    ) -> Self {
        Self {
            runtime,
            mailbox,
            store,
            resolver,
            config,
            replay_buffers: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_default_values() {
        let config = ServerConfig::default();
        assert_eq!(config.address, "0.0.0.0:3000");
        assert_eq!(config.sse_buffer_size, 64);
        assert_eq!(config.replay_buffer_capacity, 1024);
    }

    #[test]
    fn server_config_serde_roundtrip() {
        let config = ServerConfig {
            address: "127.0.0.1:8080".to_string(),
            sse_buffer_size: 128,
            replay_buffer_capacity: 512,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.address, "127.0.0.1:8080");
        assert_eq!(parsed.sse_buffer_size, 128);
        assert_eq!(parsed.replay_buffer_capacity, 512);
    }

    #[test]
    fn server_config_deserialize_with_defaults() {
        let json = r#"{"address": "localhost:9000"}"#;
        let config: ServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.address, "localhost:9000");
        assert_eq!(config.sse_buffer_size, 64);
    }
}
