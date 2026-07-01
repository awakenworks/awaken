//! `awaken-protocol-managed` — the Managed Agents runtime-protocol adapter.
//!
//! This is the anti-corruption boundary between the public Anthropic Managed
//! Agents wire and the neutral runtime. It owns the public DTOs, the projection
//! from committed `Message`s to public events, and the axum router; it drives one
//! [`SessionRuntime`] port and constructs no runtime itself. It is the only crate
//! permitted to name Anthropic protocol vocabulary (G16).
//!
//! Scope (M1): `POST /v1/sessions`, `POST /v1/sessions/{id}/events` (`user.message`),
//! `GET /v1/sessions/{id}/events`, and the SSE stream. HITL / custom tools /
//! outcomes are wired in later milestones.

pub mod dto;
pub mod project;
mod router;
mod state;

pub use router::router;
pub use state::{Decision, ManagedState, RunError, SessionRuntime, StateError, TurnOutcome};
