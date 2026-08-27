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

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
#[cfg(test)]
use awaken_session_contract::SessionExecutionState;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, ManagedSessionRepository, PersistedSession,
    ScopedPersistedSession, SessionCreateResult, SessionIdempotencyReceipt, SessionMutation,
    SessionMutationPayload, SessionMutationResult, SessionRecoveryQuarantine, SessionRecoveryScan,
    SessionRepositoryConflict, SessionRepositoryError, SessionRevision,
};

mod deployments;
mod dream;
mod extraction;
mod row_codec;
mod schema;
use row_codec::{EncodedSessionRow, decode, encode};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use schema::session_bundle;
use sqlx::Row;
use sqlx::postgres::PgPool;

/// The session store's table namespace / bundle prefix: the table is
/// `managed_session`, the ledger `managed_schema_migrations`.
const NS: &str = "managed";
const RECOVERY_BATCH_SIZE: i64 = 256;
const SESSION_CREATE_REVISION: SessionRevision = SessionRevision(1);

fn aggregate_str(session: &PersistedSession) -> Result<String, SessionRepositoryError> {
    encode(session).map_err(corrupt)
}

fn referenced_vault_ids(session: &PersistedSession) -> BTreeSet<String> {
    session
        .frozen_baseline()
        .map(|baseline| {
            baseline
                .mcp_authoring
                .ordered_vault_ids
                .iter()
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn referenced_mcp_credential_source_ids(session: &PersistedSession) -> BTreeSet<String> {
    session
        .mcp
        .desired_attachments()
        .into_iter()
        .filter_map(|attachment| {
            attachment
                .credential
                .as_ref()
                .map(|access| access.credential.id.clone())
        })
        .collect()
}

pub(crate) fn lifecycle_str(fact: &ManagedLifecycleFact) -> String {
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

/// One transaction's complete view of a Session identity. Backends populate
/// this from a single statement so create replay and mutation replay cannot
/// classify different snapshots or maintain parallel live/tombstone rules.
enum TransactionalSessionIdentity {
    Absent,
    Live {
        owner_scope: String,
        row: EncodedSessionRow,
    },
    Tombstone {
        owner_scope: String,
        revision: SessionRevision,
    },
}

struct RawSessionIdentity {
    kind: i64,
    owner_scope: Option<String>,
    revision: Option<i64>,
    aggregate_json: Option<String>,
}

fn transactional_session_identity(
    rows: Vec<RawSessionIdentity>,
) -> Result<TransactionalSessionIdentity, SessionRepositoryError> {
    match rows.as_slice() {
        [] => Ok(TransactionalSessionIdentity::Absent),
        [
            RawSessionIdentity {
                kind: 0,
                owner_scope: Some(owner_scope),
                revision: Some(revision),
                aggregate_json: Some(aggregate_json),
            },
        ] => Ok(TransactionalSessionIdentity::Live {
            owner_scope: owner_scope.clone(),
            row: EncodedSessionRow {
                aggregate_json: aggregate_json.clone(),
                revision: *revision,
            },
        }),
        [
            RawSessionIdentity {
                kind: 1,
                owner_scope: Some(owner_scope),
                revision: Some(revision),
                aggregate_json: None,
            },
        ] => Ok(TransactionalSessionIdentity::Tombstone {
            owner_scope: owner_scope.clone(),
            revision: SessionRevision(
                u64::try_from(*revision)
                    .map_err(|_| corrupt("negative Session identity revision"))?,
            ),
        }),
        [_, _, ..] => Err(corrupt(
            "Session identity has both a live aggregate and tombstone",
        )),
        [_] => Err(corrupt("invalid Session identity row")),
    }
}

#[derive(Clone, Copy)]
enum MissingCreateReceipt {
    AllowInsertFence,
    RejectOccupied,
}

impl TransactionalSessionIdentity {
    fn owner_and_revision(
        &self,
    ) -> Result<Option<(&str, SessionRevision)>, SessionRepositoryError> {
        match self {
            Self::Absent => Ok(None),
            Self::Live { owner_scope, row } => Ok(Some((
                owner_scope,
                SessionRevision(
                    u64::try_from(row.revision)
                        .map_err(|_| corrupt("negative Session identity revision"))?,
                ),
            ))),
            Self::Tombstone {
                owner_scope,
                revision,
            } => Ok(Some((owner_scope, *revision))),
        }
    }
}

fn classify_create_replay(
    owner_scope: &str,
    session_id: &str,
    idempotency: &IdempotencyRecord,
    receipt: Option<SessionIdempotencyReceipt>,
    identity: TransactionalSessionIdentity,
    missing_receipt: MissingCreateReceipt,
) -> Result<Option<PersistedSession>, SessionRepositoryError> {
    let Some(receipt) = receipt else {
        return match identity {
            TransactionalSessionIdentity::Absent => Ok(None),
            TransactionalSessionIdentity::Live { .. }
                if matches!(missing_receipt, MissingCreateReceipt::AllowInsertFence) =>
            {
                Ok(None)
            }
            TransactionalSessionIdentity::Live { .. } => Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::AlreadyExists,
            )),
            TransactionalSessionIdentity::Tombstone { .. } => Err(
                SessionRepositoryError::Conflict(SessionRepositoryConflict::Tombstoned),
            ),
        };
    };
    if receipt.committed_revision != SESSION_CREATE_REVISION {
        return Err(corrupt(
            "Session create receipt has a non-create committed revision",
        ));
    }
    let TransactionalSessionIdentity::Live {
        owner_scope: stored_owner,
        row,
    } = identity
    else {
        return match identity {
            TransactionalSessionIdentity::Absent => {
                Err(corrupt("Session create receipt has no durable aggregate"))
            }
            TransactionalSessionIdentity::Tombstone { .. } => Err(
                SessionRepositoryError::Conflict(SessionRepositoryConflict::Tombstoned),
            ),
            TransactionalSessionIdentity::Live { .. } => unreachable!(),
        };
    };
    if stored_owner != owner_scope {
        return Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::AlreadyExists,
        ));
    }
    if receipt.payload_hash != idempotency.payload_hash {
        return Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::IdempotencyMismatch,
        ));
    }
    let session = decode(row).map_err(corrupt)?;
    if session.session_id != session_id || session.revision < receipt.committed_revision {
        return Err(corrupt(
            "Session create receipt does not match its durable aggregate",
        ));
    }
    Ok(Some(session))
}

fn classify_mutation_replay(
    owner_scope: &str,
    stored_hash: &str,
    committed_revision: SessionRevision,
    expected_committed_revision: SessionRevision,
    requested_hash: &str,
    identity: &TransactionalSessionIdentity,
) -> Result<SessionMutationResult, SessionRepositoryError> {
    let Some((stored_owner, current_revision)) = identity.owner_and_revision()? else {
        return Err(corrupt(
            "Session mutation receipt has no durable aggregate or tombstone",
        ));
    };
    if stored_owner != owner_scope {
        return Ok(SessionMutationResult::Conflict { current_revision });
    }
    if current_revision < committed_revision {
        return Err(corrupt(
            "Session mutation receipt revision exceeds durable identity",
        ));
    }
    if stored_hash != requested_hash
        || committed_revision == SESSION_CREATE_REVISION
        || committed_revision != expected_committed_revision
    {
        return Ok(SessionMutationResult::IdempotencyMismatch);
    }
    Ok(SessionMutationResult::Replayed {
        new_revision: committed_revision,
    })
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
