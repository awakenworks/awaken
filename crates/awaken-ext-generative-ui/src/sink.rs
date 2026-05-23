//! Forwarding sink: sub-agent TextDelta -> parent ToolCallStreamDelta.
//!
//! Re-exported from [`awaken_runtime::child_agent::sink`] under the legacy
//! name `StreamingSubagentSink`. New code should prefer
//! [`awaken_runtime::StreamingPassthroughSink`] directly.

pub use awaken_runtime::child_agent::sink::StreamingPassthroughSink as StreamingSubagentSink;
