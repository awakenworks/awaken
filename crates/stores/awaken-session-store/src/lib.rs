// @formal entrypoint session.owner_saved_atomically
// @formal entrypoint session.visible_row_always_owned
// @formal entrypoint session_creation.exact_replay_conflict_and_tombstone_are_closed
// @formal entrypoint session_time.root_observations_are_revision_bound_and_replay_stable
// @formal entrypoint webhook.committed_outbox_row_is_recoverable
// @formal entrypoint webhook.lifecycle_commit_and_outbox_are_atomic
//! Durable [`SessionRepository`] and [`LifecycleOutbox`] backends: the home for the Managed
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

use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::sync::Arc;

use async_trait::async_trait;
use awaken_session_repository_contract::{LifecycleOutbox, SessionRepository};
#[cfg(test)]
use awaken_session_state_contract::SessionExecutionState;
use awaken_session_types::SessionRevision;
use futures_util::TryStreamExt;
use {
    awaken_session_state_contract::{
        IdempotencyRecord, ManagedLifecycleFact, PersistedSession, ScopedPersistedSession,
        SessionCreateResult, SessionEnvironmentPhase, SessionEnvironmentState,
        SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload, SessionMutationResult,
        SessionRepositoryConflict, SessionRepositoryError, SessionScan, SessionWorkerObligation,
    },
    awaken_session_types::SessionRecoveryQuarantine,
};

mod deployments;
mod dream;
mod event_admissions;
mod extraction;
mod row_codec;
mod schema;
use row_codec::{EncodedSessionRow, decode, encode, normalize_published_row};
use schema::{BUNDLE_ID, CONVERGED_BUNDLE_ID, converged_session_bundle, selected_session_schema};
#[cfg(test)]
use schema::{
    compacted_published_session_bundle, original_published_session_bundle, session_bundle,
};
use sqlx::Row;
use sqlx::postgres::PgPool;

/// The session store's table namespace / bundle prefix: the table is
/// `managed_session`, the ledger `managed_schema_migrations`.
const NS: &str = "managed";
const RECOVERY_BATCH_SIZE: i64 = 256;
const SESSION_CREATE_REVISION: SessionRevision = SessionRevision(1);

/// Disposable capacity fields stored beside the existing reconciliation row.
/// The Session root is re-read before every claim; these fields never grant one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkerObligationIndex {
    owner: Option<String>,
    preparation_candidate: bool,
    unbound_preparation: bool,
    unbound_terminal: bool,
    invalid: bool,
}

fn worker_obligation_index(session: &PersistedSession) -> WorkerObligationIndex {
    let mut index = WorkerObligationIndex {
        owner: None,
        preparation_candidate: session.is_run_free_initial_preparation_candidate(),
        unbound_preparation: false,
        unbound_terminal: false,
        invalid: false,
    };
    match session.worker_obligation() {
        Ok(SessionWorkerObligation::None) => {}
        Ok(SessionWorkerObligation::RetainedOwner(owner)) => {
            index.owner = Some(owner.to_owned());
        }
        Ok(SessionWorkerObligation::UnboundInitialPreparation) => {
            index.unbound_preparation = true;
        }
        Ok(SessionWorkerObligation::UnboundTerminalCleanup) => {
            index.unbound_terminal = true;
        }
        Err(_) => index.invalid = true,
    }
    index
}

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
    SessionRepositoryError::AuthorityUnavailable(error.to_string())
}

fn corrupt(error: impl std::fmt::Display) -> SessionRepositoryError {
    SessionRepositoryError::Corrupt(error.to_string())
}

const ENVIRONMENT_COUNT_SQL: &str =
    "SELECT session_id, aggregate_json, revision FROM managed_session ORDER BY session_id";

/// Keep the bound exclusive key in the index condition of a cached plan.
/// A nullable-parameter OR instead leaves it as a prefix filter in PostgreSQL
/// generic plans and SQLite scans. Callers pass only fixed SQL identifiers and
/// placeholders here; the actual cursor always remains a bound value.
fn keyset_predicate(column: &str, parameter: &str, has_after: bool) -> String {
    if has_after {
        format!("{column} > {parameter}")
    } else {
        format!("{parameter} IS NULL")
    }
}

fn reconciliation_scan_sql(
    preparation_only: bool,
    has_after: bool,
    after_parameter: &str,
) -> String {
    let preparation = if preparation_only {
        "work.preparation_candidate = 1 AND "
    } else {
        ""
    };
    let cursor = keyset_predicate("work.session_id", after_parameter, has_after);
    let limit = RECOVERY_BATCH_SIZE + 1;
    format!(
        "SELECT session.scope_id, session.session_id, session.aggregate_json, \
                session.revision, work.observed_revision \
         FROM managed_session_reconciliation_work work \
         JOIN managed_session session ON session.session_id = work.session_id \
         WHERE {preparation}{cursor} ORDER BY work.session_id LIMIT {limit}"
    )
}

async fn count_environment_phase(
    rows: impl futures_util::TryStream<Ok = (String, String, i64), Error = SessionRepositoryError>,
    phase: SessionEnvironmentPhase,
) -> Result<u64, SessionRepositoryError> {
    rows.try_fold(
        0_u64,
        |count, (stored_id, aggregate_json, revision)| async move {
            let session = decode(EncodedSessionRow {
                aggregate_json,
                revision,
            })
            .map_err(corrupt)?;
            if session.session_id != stored_id {
                return Err(corrupt(
                    "managed Session aggregate id does not match its global phase-count index",
                ));
            }
            let selected = matches!(
                (phase, &session.environment),
                (
                    SessionEnvironmentPhase::Unmaterialized,
                    SessionEnvironmentState::Unmaterialized
                ) | (
                    SessionEnvironmentPhase::Resident,
                    SessionEnvironmentState::Resident { .. }
                ) | (
                    SessionEnvironmentPhase::Suspending,
                    SessionEnvironmentState::Suspending { .. }
                ) | (
                    SessionEnvironmentPhase::Hibernated,
                    SessionEnvironmentState::Hibernated { .. }
                ) | (
                    SessionEnvironmentPhase::Restoring,
                    SessionEnvironmentState::Restoring { .. }
                )
            );
            count
                .checked_add(u64::from(selected))
                .ok_or_else(|| corrupt("Session Environment phase count overflow"))
        },
    )
    .await
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransactionalSessionIdentityRowPlan {
    Live,
    Tombstone,
    Reject,
}

/// Decode the shape of the one row returned by the transactional identity
/// UNION. The owner and live aggregate are columns of that same selected row;
/// absent/blank owners and mixed live/tombstone shapes fail closed.
#[must_use]
const fn transactional_session_identity_row_plan(
    kind: i64,
    owner_nonempty: bool,
    revision_present: bool,
    aggregate_present: bool,
) -> TransactionalSessionIdentityRowPlan {
    match (kind, owner_nonempty, revision_present, aggregate_present) {
        (0, true, true, true) => TransactionalSessionIdentityRowPlan::Live,
        (1, true, true, false) => TransactionalSessionIdentityRowPlan::Tombstone,
        _ => TransactionalSessionIdentityRowPlan::Reject,
    }
}

#[must_use]
const fn session_create_shape_is_valid(
    owner_nonempty: bool,
    session_id_nonempty: bool,
    idempotency_key_nonempty: bool,
    payload_hash_nonempty: bool,
    fresh_revision: bool,
    lifecycle_targets_match: bool,
) -> bool {
    owner_nonempty
        && session_id_nonempty
        && idempotency_key_nonempty
        && payload_hash_nonempty
        && fresh_revision
        && lifecycle_targets_match
}

fn validate_session_create_command(
    owner_scope: &str,
    session: &PersistedSession,
    idempotency: &IdempotencyRecord,
    lifecycle_facts: &[ManagedLifecycleFact],
) -> Result<(), SessionRepositoryError> {
    if !session_create_shape_is_valid(
        !owner_scope.trim().is_empty(),
        !session.session_id.as_str().trim().is_empty(),
        !idempotency.key.trim().is_empty(),
        !idempotency.payload_hash.trim().is_empty(),
        session.revision == SessionRevision(0),
        lifecycle_facts
            .iter()
            .all(|fact| fact.object_id == session.session_id),
    ) {
        return Err(SessionRepositoryError::InvalidMutation(
            "invalid Session create command".into(),
        ));
    }
    Ok(())
}

#[cfg(kani)]
/// @obligation session.owner_saved_atomically
/// @obligation session.visible_row_always_owned
/// @proof-input-debt free_symbol session_create_shape_is_valid fresh_revision
/// @proof-input-debt free_symbol session_create_shape_is_valid idempotency_key_nonempty
/// @proof-input-debt free_symbol session_create_shape_is_valid lifecycle_targets_match
/// @proof-input-debt free_symbol session_create_shape_is_valid owner_nonempty
/// @proof-input-debt free_symbol session_create_shape_is_valid payload_hash_nonempty
/// @proof-input-debt free_symbol session_create_shape_is_valid session_id_nonempty
/// @proof-input-debt free_symbol transactional_session_identity_row_plan aggregate_present
/// @proof-input-debt free_symbol transactional_session_identity_row_plan owner_nonempty
/// @proof-input-debt free_symbol transactional_session_identity_row_plan revision_present
#[kani::proof]
fn session_identity_and_create_shapes_require_one_owned_live_row() {
    // Cause/effect tables exhaust row kind/presence and every create command
    // validity axis. A live identity is admitted only as kind=0 with non-empty
    // owner, revision and aggregate in the same row; tombstones require the
    // same owner/revision but no aggregate. Create accepts only the conjunction
    // of owner, id, idempotency, fresh revision and lifecycle-target facts.
    let kind = kani::any();
    let owner = kani::any();
    let revision = kani::any();
    let aggregate = kani::any();
    let row = transactional_session_identity_row_plan(kind, owner, revision, aggregate);
    assert_eq!(
        row == TransactionalSessionIdentityRowPlan::Live,
        kind == 0 && owner && revision && aggregate
    );
    assert_eq!(
        row == TransactionalSessionIdentityRowPlan::Tombstone,
        kind == 1 && owner && revision && !aggregate
    );
    let session_id = kani::any();
    let key = kani::any();
    let hash = kani::any();
    let fresh = kani::any();
    let lifecycle = kani::any();
    assert_eq!(
        session_create_shape_is_valid(owner, session_id, key, hash, fresh, lifecycle),
        owner && session_id && key && hash && fresh && lifecycle
    );
}

fn transactional_session_identity(
    rows: Vec<RawSessionIdentity>,
) -> Result<TransactionalSessionIdentity, SessionRepositoryError> {
    let mut rows = rows.into_iter();
    let Some(row) = rows.next() else {
        return Ok(TransactionalSessionIdentity::Absent);
    };
    if rows.next().is_some() {
        return Err(corrupt(
            "Session identity has both a live aggregate and tombstone",
        ));
    }
    let plan = transactional_session_identity_row_plan(
        row.kind,
        row.owner_scope
            .as_deref()
            .is_some_and(|owner| !owner.trim().is_empty()),
        row.revision.is_some(),
        row.aggregate_json.is_some(),
    );
    match (plan, row.owner_scope, row.revision, row.aggregate_json) {
        (
            TransactionalSessionIdentityRowPlan::Live,
            Some(owner_scope),
            Some(revision),
            Some(aggregate_json),
        ) => Ok(TransactionalSessionIdentity::Live {
            owner_scope,
            row: EncodedSessionRow {
                aggregate_json,
                revision,
            },
        }),
        (
            TransactionalSessionIdentityRowPlan::Tombstone,
            Some(owner_scope),
            Some(revision),
            None,
        ) => Ok(TransactionalSessionIdentity::Tombstone {
            owner_scope,
            revision: SessionRevision(
                u64::try_from(revision)
                    .map_err(|_| corrupt("negative Session identity revision"))?,
            ),
        }),
        _ => Err(corrupt("invalid Session identity row")),
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

/// One current-root admission for both SQL adapters, after receipt replay and
/// the transactional owner/revision fence but before any row, receipt or outbox
/// write. Decoding the already-selected row is required to preserve observations
/// even for callers that constructed their replacement without the application.
fn validate_current_mutation(
    mutation: &SessionMutation,
    current_row: EncodedSessionRow,
) -> Result<Option<String>, SessionRepositoryError> {
    let current = decode(current_row).map_err(corrupt)?;
    if current.session_id != mutation.payload.session_id() {
        return Err(corrupt(
            "managed Session aggregate id does not match its mutation target",
        ));
    }
    match &mutation.payload {
        SessionMutationPayload::Replace(replacement) => {
            current
                .validate_observation_replacement(replacement)
                .map_err(|error| SessionRepositoryError::InvalidMutation(error.to_string()))?;
            Ok(None)
        }
        SessionMutationPayload::Delete(tombstone) => {
            if !current.admits_tombstone(tombstone.session_id.as_str(), tombstone.deleted_revision)
            {
                return Err(SessionRepositoryError::InvalidMutation(
                    "Session tombstone requires hidden terminal disposition and completed cleanup"
                        .into(),
                ));
            }
            Ok(Some(event_admissions::encode(&current)?))
        }
    }
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
