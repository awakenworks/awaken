//! Managed Session command failures and their protocol-facing classification.

use awaken_session_contract::{LiveInboxError, RunError};

/// Why a session operation failed (mapped to an HTTP status by the router).
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("session not found")]
    NotFound,
    /// A write was sent to an archived (terminated, read-only) session; the router
    /// maps it to 409 `invalid_request_error`.
    #[error("session is archived and is read-only")]
    Archived,
    /// The root Session revision changed while a command was being compiled.
    /// Callers re-read and retry the complete command; stale snapshots are never
    /// merged or written back.
    #[error("session changed concurrently; read the latest revision and retry")]
    Conflict,
    #[error("session idempotency key was reused with another request")]
    IdempotencyMismatch,
    /// A session create named a vault that does not exist (`vault_ids`); the
    /// router maps it to the standard 404 envelope naming the vault id.
    #[error("vault `{0}` not found")]
    VaultNotFound(String),
    #[error(transparent)]
    Run(#[from] RunError),
    /// A live-inbox operation was refused (inactive queue, unknown message,
    /// or a stale reorder); the router maps each case to its own status.
    #[error(transparent)]
    LiveInbox(#[from] LiveInboxError),
}
