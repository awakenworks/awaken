//! Shared Managed wire mechanics; no domain state belongs here.

pub mod headers;
mod response_context;
pub(crate) mod scope;

pub use response_context::{with_managed_response_context, with_managed_workspace_header};
