//! The native Managed Agents wire transfer objects — the *only* place Anthropic
//! protocol vocabulary lives (G16), each type mapping 1:1 onto the official SDK's
//! beta `managed-agents` shapes (`anthropic-beta: managed-agents-2026-04-01`).
//!
//! Pure serde types only. The logic that *assembles* them from neutral domain
//! state lives in `project` and `state`; the routes that serve them live in
//! `routes`; our non-SDK vocabulary lives in `ext`.
//!
//! ## Naming convention (uniform across every submodule)
//!
//! We do *not* copy the SDK's TypeScript type names verbatim: they are both
//! verbose (redundant with the module path — `types::vault::Vault`) and internally
//! inconsistent (`BetaManagedAgentsVault` / `BetaManagedAgentsDeployment` but plain
//! `BetaEnvironment` / `BetaUserProfile`). Instead the correspondence is exact but
//! expressed two ways, applied to every type:
//!
//! - **Idiomatic Rust name, namespaced by resource module.** Response objects are
//!   the bare resource noun (`Session`, `Vault`, `Credential`, `Deployment`,
//!   `Environment`, `Work`, …); request bodies are `<Resource>{Create,Update}Params`;
//!   delete receipts are `Deleted<Resource>`.
//! - **The exact SDK type in the doc.** Every wire type's doc opens with its SDK
//!   counterpart in backticks (e.g. `` `BetaManagedAgentsSession` ``), so `rg
//!   'BetaManagedAgentsSession' src/types` finds the Rust type 1:1.
//!
//! One submodule per SDK resource, plus the shared cursor-page shape:
//! - [`session`] — sessions, events, and the error envelope.
//! - [`agent`] — the `agents` resource (versioned agent configurations).
//! - [`deployment`] — the `deployments` + `deployment-runs` resources.
//! - [`environment`] — the `environments` resource + its work queue.
//! - [`user_profile`] — the `user-profiles` resource. **NB: a separate beta**
//!   (`anthropic-beta: user-profiles-2026-03-24`), not `managed-agents-2026-04-01`;
//!   see the module doc.
//! - [`vault`] — the `vaults` resource (vaults + credentials).
//! - [`page`] — the SDK cursor-page envelope shared by every CRUD family.

pub mod agent;
pub mod deployment;
pub mod environment;
pub mod page;
pub mod session;
pub mod user_profile;
pub mod vault;

pub use page::{Page, PageQuery, paginate};
pub use session::*;
