//! Persistent thread model: messages, changesets, and metadata.

pub mod changeset;
pub mod message;
pub mod model;

pub use changeset::{CheckpointReason, RunMeta, ThreadChangeSet, Version};
pub use message::{gen_message_id, Message, MessageMetadata, Role, ToolCall, Visibility};
pub use model::{Thread, ThreadMetadata};
