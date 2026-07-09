//! The native Managed Agents wire transfer objects — the *only* place Anthropic
//! protocol vocabulary lives (G16), each type mapping 1:1 onto the official SDK's
//! beta `managed-agents` shapes (`anthropic-beta: managed-agents-2026-04-01`).
//!
//! Pure serde types only. The logic that *assembles* them from neutral domain
//! state lives in `project` and `state`; the routes that serve them live in
//! `routes`; our non-SDK vocabulary lives in `ext`.
//!
//! One submodule per SDK resource, plus the shared cursor-page shape:
//! - [`session`] — sessions, events, and the error envelope.
//! - [`user_profile`] — the `user-profiles` resource.
//! - [`vault`] — the `vaults` resource (vaults + credentials).
//! - [`page`] — the SDK cursor-page envelope shared by every CRUD family.

pub mod page;
pub mod session;
pub mod user_profile;
pub mod vault;

pub use page::Page;
pub use session::*;
