//! Agent Client Protocol (ACP) JSON-RPC 2.0 encoding adapter.
//!
//! Maps [`tirea_contract::AgentEvent`] to ACP `session/update` and
//! `session/request_permission` notifications.
#![allow(missing_docs)]

mod encoder;
mod events;
mod types;

pub use encoder::AcpEncoder;
pub use events::{
    AcpError, AcpEvent, AcpFinished, AcpToolCall, AcpToolCallUpdate, RequestPermissionParams,
    SessionUpdateParams,
};
pub use types::{PermissionOption, StopReason, ToolCallStatus};
