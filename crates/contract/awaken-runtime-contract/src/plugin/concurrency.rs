//! Plugin-owned, execution-time concurrency constraints.
//!
//! A tool or routed executor declares its intrinsic resource behavior. Plugins
//! may add constraints derived from resolved configuration and call arguments,
//! but composition is always conservative: the runtime unions every claim and
//! no plugin can widen another source's declaration.

use crate::tool::{ToolCall, ToolConcurrency};

/// One named source of additional resource claims for a concrete tool call.
pub trait ToolConcurrencyConstraint: Send + Sync {
    /// Stable contribution id, checked against the plugin's capability bound.
    fn id(&self) -> &str;

    /// Return only the claims this source adds. [`ToolConcurrency::Parallel`]
    /// is the identity value when the call is outside this source's concern.
    fn constrain(&self, call: &ToolCall) -> ToolConcurrency;
}
