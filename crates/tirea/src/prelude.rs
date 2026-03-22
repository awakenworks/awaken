//! Prelude for convenient imports.
//!
//! This module re-exports the most commonly used types and traits for
//! tool and plugin development. Using the prelude reduces boilerplate
//! and ensures all extension traits are available.
//!
//! # Example
//!
//! ```ignore
//! use tirea::prelude::*;
//!
//! struct MyTool;
//!
//! #[async_trait]
//! impl Tool for MyTool {
//!     fn descriptor(&self) -> ToolDescriptor {
//!         ToolDescriptor::new("my_tool", "My Tool", "Does something useful")
//!     }
//!
//!     async fn execute(&self, args: Value, ctx: &Thread) -> Result<ToolResult, ToolError> {
//!         // Extension traits are auto-imported (requires "core" feature)
//!         ctx.allow_tool("follow_up"); // PermissionContextExt
//!
//!         Ok(ToolResult::success("my_tool", json!({"status": "done"})))
//!     }
//! }
//! ```

// Re-export async_trait for convenience
pub use async_trait::async_trait;

// Re-export serde_json for tool implementations
pub use serde_json::{json, Value};

// Derive helpers for TypedTool implementations
pub use schemars::JsonSchema;
pub use serde::Deserialize;

// ── Always available: contracts + state ──────────────────────────────────

// Core execution state object (state + runtime metadata + activity wiring)
pub use crate::contracts::Thread;

// Raw state-only context for lower-level integrations
pub use tirea_state::StateContext;

// Tool trait and types
pub use crate::contracts::runtime::tool_call::{
    Tool, ToolDescriptor, ToolError, ToolResult, ToolStatus, TypedTool,
};

// Message types
pub use crate::contracts::thread::{Message, Role, ToolCall};

// Phase types for plugins
pub use crate::contracts::runtime::phase::{
    Phase, RunAction, StepContext, StepOutcome, ToolCallAction,
};
pub use crate::contracts::runtime::tool_call::ToolGate;
pub use crate::contracts::{Suspension, SuspensionResponse};

// ── Core entry points (require "core" feature) ──────────────────────────

#[cfg(feature = "core")]
pub use crate::runtime::{AgentOs, PreparedRun, RunStream};

#[cfg(feature = "core")]
pub use crate::composition::{tool_map, tool_map_from_arc, AgentDefinition, AgentOsBuilder};

// Plugin SPI
pub use crate::contracts::runtime::phase::{ActionSet, BeforeInferenceAction};
pub use crate::contracts::AgentBehavior;
#[cfg(feature = "core")]
pub use crate::runtime::prompt_segments::{
    consume_after_emit_context_messages_action, remove_context_message_action,
    remove_context_messages_by_prefix_action, upsert_context_message_action, PromptSegmentAction,
    PromptSegmentState,
};

// ── Extension types (require "core" feature) ─────────────────────────────

#[cfg(feature = "core")]
pub use tirea_extension_permission::{
    PermissionAction, PermissionPlugin, ToolPermissionBehavior, ToolPolicyPlugin,
};

// ── Skills extension (require "skills" feature) ──────────────────────────

#[cfg(feature = "skills")]
pub use crate::skills::{SkillDiscoveryPlugin, SkillRegistry, SkillSubsystem};
