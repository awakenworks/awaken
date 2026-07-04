//! Anti-corruption layer (ADR-0043 / G16) between the public Anthropic Managed
//! Agents wire (`awaken-protocol-managed`, mirrored from `@anthropic-ai/sdk`) and
//! the neutral management-plane domain (`awaken-credential-vault`, `awaken-model-catalog`).
//! The wire keeps Anthropic's snake_case tags (`mcp_oauth`/`static_bearer`/
//! `environment_variable`); the domain keeps neutral names — this crate maps
//! between them, and is the only place the two vocabularies meet.
//!
//! P0: crate skeleton only — mappings land in Phase 2 alongside the managed
//! vault/credential DTOs.

#![forbid(unsafe_code)]

/// The Managed Agents beta wire header this bridge targets.
pub const MANAGED_BETA: &str = "managed-agents-2026-04-01";
