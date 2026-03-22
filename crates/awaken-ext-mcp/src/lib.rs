//! Model Context Protocol (MCP) client integration for external tool servers.
//!
//! Provides [`McpToolRegistryManager`] for connecting to MCP servers and
//! exposing their tools as awaken [`Tool`](awaken_contract::contract::tool::Tool) instances.

pub mod config;
pub mod error;
pub mod id_mapping;
pub mod manager;
pub mod plugin;
pub mod progress;
pub mod sampling;
pub mod transport;

pub use config::{McpServerConnectionConfig, TransportTypeId};
pub use error::McpError;
pub use manager::{
    McpPromptEntry, McpRefreshHealth, McpResourceEntry, McpToolRegistry, McpToolRegistryManager,
};
pub use plugin::McpPlugin;
pub use progress::McpProgressUpdate;
pub use sampling::SamplingHandler;
pub use transport::{
    McpPromptArgument, McpPromptDefinition, McpPromptMessage, McpPromptResult,
    McpResourceDefinition, McpToolTransport,
};
