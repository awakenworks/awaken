//! Admin config API (ADR-0043) — the self-hosted-only management surface the
//! Anthropic wire does not define: CRUD over provider / protocol-endpoint /
//! offering and credential entry. Built on `awaken-api-contract` (`ApiError`
//! problem details / pagination / allowlisted queries) and generates the TS
//! contract (P2).
//!
//! The domain types it authors live in `awaken-model-catalog` / `awaken-credential-vault`;
//! this crate is the HTTP adapter over their repos, storing nothing itself.

#![forbid(unsafe_code)]

mod router;

pub use router::{AdminState, ResolveRequest, ResolvedInferenceView, admin_router};

/// The API surface version this crate serves.
pub const API_VERSION: &str = "0";
