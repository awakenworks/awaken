//! Engine layer: genai-backed LLM executor and type conversion.
//!
//! Bridges awaken's provider-neutral types to the `genai` crate.
//! - `convert`: Message, Tool, Usage, StopReason conversions
//! - `streaming`: StreamCollector for accumulating ChatStreamEvents
//! - `executor`: `GenaiExecutor` implementing `LlmExecutor`

pub mod circuit_breaker;
pub mod convert;
pub mod executor;
pub mod mock;
pub mod pool_executor;
pub mod pool_router;
pub mod retry;
pub mod scripted;
pub mod streaming;

#[cfg(test)]
mod executor_tests;

pub use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
pub use executor::GenaiExecutor;
pub use mock::{MockLlmExecutor, MockProviderProfile};
pub use pool_executor::{PoolExecutor, PoolMemberExecutor};
pub use pool_router::{HealthMask, PoolRouter, RouterMember};
pub use retry::{LlmRetryPolicy, RetryConfigKey, RetryingExecutor};
pub use scripted::{ProviderScriptEvent, ScriptedLlmExecutor};
