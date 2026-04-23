//! Web search tool extension for Awaken AI Agent runtime
//!
//! This crate provides a configurable web search tool with pluggable
//! search providers. The default implementation uses SerpAPI.
//!
//! # Quick Start
//!
//! ```rust,no_run,ignore
//! use awaken_runtime::builder::AgentRuntimeBuilder;
//! use awaken_ext_web_search::{WebSearchTool, SerpApiProvider};
//! use std::sync::Arc;
//!
//! // From API key
//! let provider = SerpApiProvider::new("your-api-key", None).unwrap();
//! let tool = WebSearchTool::with_provider(Arc::new(provider));
//!
//! // Or using environment variable SERPAPI_KEY
//! let provider = SerpApiProvider::new("", None).unwrap();
//! let tool = WebSearchTool::with_provider(Arc::new(provider));
//!
//! // Or using convenience constructor
//! let tool = WebSearchTool::new("", None).unwrap();
//!
//! let mut builder = AgentRuntimeBuilder::new();
//! builder = builder.with_tool(WebSearchTool::TOOL_ID, Arc::new(tool));
//! ```
//!
//! # Adding a custom provider
//!
//! Implement the [`SearchProvider`] trait:
//!
//! ```rust
//! use awaken_ext_web_search::SearchProvider;
//! use awaken_contract::contract::tool::{ToolCallContext, ToolError};
//! use awaken_ext_web_search::SearchResult;
//! use async_trait::async_trait;
//!
//! struct MyCustomProvider;
//!
//! #[async_trait]
//! impl SearchProvider for MyCustomProvider {
//!     async fn search(
//!         &self,
//!         query: &str,
//!         num_results: usize,
//!         ctx: &ToolCallContext,
//!     ) -> Result<Vec<SearchResult>, ToolError> {
//!         // your implementation here
//! #        Ok(vec![])
//!     }
//! }
//! ```

#![forbid(unsafe_code)]

pub mod providers;
pub mod tool;

pub use tool::{CompositeMode, CompositeSearchProvider, SearchProvider, SearchResult, WebSearchTool};

#[cfg(feature = "serpapi")]
pub use providers::SerpApiProvider;

/// Convenience prelude for importing common types
pub mod prelude {
    pub use super::{SearchProvider, SearchResult, WebSearchTool};

    #[cfg(feature = "serpapi")]
    pub use super::providers::SerpApiProvider;
}
