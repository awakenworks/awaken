//! Admin config API (ADR-0043) — the self-hosted-only management surface the
//! Anthropic wire does not define: CRUD over provider / protocol-endpoint /
//! offering / model / inference-profile. Built on `awaken-api-contract`
//! (`QuerySchema` allowlist / `ApiError` / pagination) + `awaken-query`, and
//! generates the TS contract (P2).
//!
//! P0: crate skeleton only — routers land in Phase 2. The domain types it exposes
//! live in `awaken-model-catalog` / `awaken-credential-vault`.

#![forbid(unsafe_code)]

/// The API surface version this crate serves.
pub const API_VERSION: &str = "0";
