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
//!         ctx.allow_tool("follow_up");           // PermissionContextExt
//!         ctx.add_reminder("Remember to check"); // ReminderContextExt
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
pub use crate::contracts::runtime::plugin::phase::{
    Phase, RunAction, StepContext, StepOutcome, ToolCallAction, ToolContext,
};
pub use crate::contracts::{Suspension, SuspensionResponse};

// ── Extension types (require "core" feature) ─────────────────────────────

#[cfg(feature = "core")]
pub use crate::extensions::permission::{
    PermissionContextExt, PermissionPlugin, PermissionState, ToolPermissionBehavior,
    ToolPolicyPlugin,
};

#[cfg(feature = "core")]
pub use crate::extensions::reminder::{
    ReminderContextExt, ReminderPlugin, ReminderState, SystemReminder,
};

// ── Skills extension (require "skills" feature) ──────────────────────────

#[cfg(feature = "skills")]
pub use crate::skills::{SkillPlugin, SkillRegistry, SkillSubsystem};
