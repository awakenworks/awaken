//! `awaken-sandbox` — the execution-plane library (opposite the control-plane
//! `awaken`). It hosts the roles a sandbox pod runs: the ACP `bridge` (stdio<->TCP
//! for a process-as-container CLI), `hand` (the neutral tool-execution endpoint), and
//! `memoryd` (the memory sidecar). Kept as a lib so each role is unit-testable without
//! spawning the binary.

pub mod bridge;

/// The `hand` role (ADR-0044/0045 tool executor). Behind the `hand` feature — it pulls
/// the tool implementations + executor channel, so the default (acp) build stays thin.
#[cfg(feature = "hand")]
pub mod hand;

/// The `memoryd` role (ADR-0053 memory sidecar). Behind the `memoryd` feature — it
/// pulls the write-through FUSE server + the durable sqlite store, so the default
/// (acp) build stays thin.
#[cfg(feature = "memoryd")]
pub mod memoryd;
