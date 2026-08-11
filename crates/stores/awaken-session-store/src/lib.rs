//! Durable [`ManagedSessionRepository`] backends: the home for the Managed
//! session aggregate (agent / model / title / metadata / accepted MCP servers),
//! over the store's own `managed` migration scope ([`session_bundle`]). Two
//! backends — [`SqliteManagedSessionRepository`] (embedded, `sessions.db`) and
//! [`PostgresManagedSessionRepository`] (network DB) — share the one portable
//! bundle, exactly like the config/catalog/credential stores.
//!
//! It is its OWN scope (`managed_session` table + `managed_schema_migrations`
//! ledger), NOT a table in the authoring-plane `admin.db`: a live session
//! instance is a different aggregate from the agent/MCP *definitions* admin holds,
//! so mixing them would cross a bounded-context line (ADR-0039 "one repository per
//! aggregate"). Secrets never land here — only the wire-echo MCP `{name,type,url}`
//! values, per the port's contract (G3).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
#[cfg(test)]
use awaken_session_contract::SessionExecutionState;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, ManagedSessionRepository, PersistedSession,
    ScopedPersistedSession, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRecoveryQuarantine, SessionRecoveryScan, SessionRepositoryConflict,
    SessionRepositoryError, SessionRevision,
};

mod deployments;
mod dream;
mod extraction;
mod row_codec;
mod schema;
use row_codec::{EncodedSessionRow, decode};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use schema::session_bundle;
use sqlx::Row;
use sqlx::postgres::PgPool;

/// The session store's table namespace / bundle prefix: the table is
/// `managed_session`, the ledger `managed_schema_migrations`.
const NS: &str = "managed";
const SQLITE_WRITE_WAIT: Duration = Duration::from_secs(30);

fn aggregate_str(session: &PersistedSession) -> Result<String, SessionRepositoryError> {
    serde_json::to_string(session).map_err(corrupt)
}

fn lifecycle_str(fact: &ManagedLifecycleFact) -> String {
    serde_json::to_string(fact).expect("Managed lifecycle fact serializes")
}

fn db_revision(revision: SessionRevision) -> Result<i64, SessionRepositoryError> {
    i64::try_from(revision.0).map_err(|_| {
        SessionRepositoryError::InvalidMutation("Session revision exceeds i64 storage".into())
    })
}

fn storage(error: impl std::fmt::Display) -> SessionRepositoryError {
    SessionRepositoryError::Unavailable(error.to_string())
}

fn corrupt(error: impl std::fmt::Display) -> SessionRepositoryError {
    SessionRepositoryError::Corrupt(error.to_string())
}

fn decode_lifecycle(data: &str) -> Result<ManagedLifecycleFact, serde_json::Error> {
    serde_json::from_str(data)
}

/// SQLite persistence for [`PersistedSession`]. One row per session, keyed by id.
mod postgres;
mod sqlite;

pub use postgres::PostgresManagedSessionRepository;
pub use sqlite::SqliteManagedSessionRepository;

#[cfg(test)]
mod tests;
