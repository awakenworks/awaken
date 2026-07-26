//! Persistence for the adapter-side Managed session aggregate.
//!
//! The runtime's committed transcript (ADR-0039) is durable, but the wire
//! `Session` object carries configuration that is NOT in the transcript — the
//! bound agent, the resolved model, the title/metadata, and the accepted MCP
//! servers. Without persisting it, a session rehydrated after a restart (or first
//! seen by another process sharing the store) reports placeholder defaults
//! (`agent = "assistant"`, empty `mcp_servers`, no title). This port stores that
//! aggregate so rehydration restores the real values.
//!
//! Secrets never cross this port: MCP entries keep only the wire-echo
//! `{name, type, url}` values, never a credential — those are re-materialized from
//! the vault at prepare time (G3).

use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::SessionLifecycleFact;

/// Monotonic root revision for every mutation of one Session aggregate.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionRevision(pub u64);

/// Secret-free execution pin needed to rebuild process-local runtime wiring
/// around an adopted Session environment. Credential fields are durable row or
/// sealed-secret references; raw material never crosses this value object.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedSessionRuntime {
    pub mcp_servers: Vec<crate::McpServerBinding>,
    #[serde(default)]
    pub delegate_ids: Vec<String>,
    pub runtime: Option<String>,
    pub deny_egress: bool,
    pub sandbox: Option<Value>,
}

/// The durable, adapter-side configuration of one Managed session, keyed by its
/// id (which is also its thread id). Everything here is what the wire `Session`
/// object needs beyond the runtime's committed transcript.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedSession {
    pub session_id: String,
    /// The one optimistic-concurrency fence for baseline, Resource, MCP,
    /// environment and lifecycle mutations. New, not-yet-inserted values use 0.
    #[serde(default)]
    pub revision: SessionRevision,
    pub agent_id: String,
    /// The resolved model the session runs (the request override, else the host
    /// default at create time).
    pub model: String,
    pub title: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub environment_id: String,
    /// Opaque, secret-free binding to the runtime-owned Session environment.
    /// The Session context persists the bytes but never interprets them; only the
    /// runtime that produced the binding may validate and adopt it after restart.
    pub environment_binding: Option<String>,
    /// The create-time runtime selection and external-resource references needed
    /// to restore process-local wiring around the durable environment.
    pub runtime: PersistedSessionRuntime,
    /// The accepted MCP servers in the SDK wire shape (`{name, type, url}`) — the
    /// echo the agent object reports; never a credential.
    pub mcp_servers: Vec<Value>,
    /// Durable resource activation state. Its `active` manifest is the exact,
    /// secret-free Session pin; `pending` and activation records make external
    /// realization/release recoverable without importing authorization concepts.
    pub resources: crate::SessionResourceState,
    /// Durable lifecycle projection used when a process rehydrates the session.
    pub status: String,
    pub archived_at: Option<String>,
}

/// One durable Session together with its intrinsic Workspace partition.
///
/// Recovery consumes this envelope atomically instead of looking up an owner in
/// a second step. It contains no principal, role, policy, credential, or
/// authorization decision; `workspace_id` is resource routing state only.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScopedPersistedSession {
    pub workspace_id: String,
    pub session: PersistedSession,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionTombstone {
    pub session_id: String,
    pub deleted_revision: SessionRevision,
    pub deleted_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdempotencyRecord {
    pub key: String,
    pub payload_hash: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SessionMutationPayload {
    Replace(PersistedSession),
    Delete(SessionTombstone),
}

impl SessionMutationPayload {
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Replace(session) => &session.session_id,
            Self::Delete(tombstone) => &tombstone.session_id,
        }
    }

    #[must_use]
    pub fn stable_hash(&self) -> String {
        crate::stable_fingerprint(self)
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionMutation {
    pub expected_revision: SessionRevision,
    pub idempotency: IdempotencyRecord,
    pub payload: SessionMutationPayload,
    #[serde(default)]
    pub lifecycle_facts: Vec<SessionLifecycleFact>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMutationResult {
    Applied { new_revision: SessionRevision },
    Replayed { new_revision: SessionRevision },
    Conflict { current_revision: SessionRevision },
    IdempotencyMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionMutationValidationError {
    #[error("Session mutation idempotency key is empty")]
    EmptyIdempotencyKey,
    #[error("Session mutation payload hash is empty")]
    EmptyPayloadHash,
    #[error("Session mutation id is empty")]
    EmptySessionId,
    #[error("Session mutation revision is exhausted")]
    RevisionExhausted,
    #[error("replacement carries a revision different from expected_revision")]
    ReplacementRevisionMismatch,
    #[error("tombstone deleted_revision is not the next root revision")]
    TombstoneRevisionMismatch,
    #[error("lifecycle fact targets another Session")]
    LifecycleSessionMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionRepositoryError {
    #[error("Session repository rejected invalid mutation: {0}")]
    InvalidMutation(String),
    #[error("Session already exists")]
    AlreadyExists,
    #[error("Session was deleted")]
    Tombstoned,
    #[error("Session idempotency key was reused with another payload")]
    IdempotencyMismatch,
    #[error("Session repository storage failed: {0}")]
    Storage(String),
}

impl SessionMutation {
    /// Validate all command-local causes before a repository reads or writes.
    /// A successful return is the only root revision the transaction may commit.
    pub fn validate(&self) -> Result<SessionRevision, SessionMutationValidationError> {
        if self.idempotency.key.trim().is_empty() {
            return Err(SessionMutationValidationError::EmptyIdempotencyKey);
        }
        if self.idempotency.payload_hash.trim().is_empty() {
            return Err(SessionMutationValidationError::EmptyPayloadHash);
        }
        let session_id = self.payload.session_id();
        if session_id.trim().is_empty() {
            return Err(SessionMutationValidationError::EmptySessionId);
        }
        let next = SessionRevision(
            self.expected_revision
                .0
                .checked_add(1)
                .ok_or(SessionMutationValidationError::RevisionExhausted)?,
        );
        match &self.payload {
            SessionMutationPayload::Replace(session)
                if session.revision != self.expected_revision =>
            {
                return Err(SessionMutationValidationError::ReplacementRevisionMismatch);
            }
            SessionMutationPayload::Delete(tombstone) if tombstone.deleted_revision != next => {
                return Err(SessionMutationValidationError::TombstoneRevisionMismatch);
            }
            SessionMutationPayload::Replace(_) | SessionMutationPayload::Delete(_) => {}
        }
        if self
            .lifecycle_facts
            .iter()
            .any(|fact| fact.session_id != session_id)
        {
            return Err(SessionMutationValidationError::LifecycleSessionMismatch);
        }
        Ok(next)
    }
}

/// The port the Managed adapter drives to persist and restore [`PersistedSession`]
/// rows. The default in-memory impl keeps single-process behavior; a durable impl
/// (e.g. SQLite alongside the transcript store) lets a session survive a restart
/// and be reported faithfully by another process.
#[async_trait]
pub trait ManagedSessionRepository: Send + Sync {
    /// Insert one new aggregate together with owner, idempotency and outbox.
    async fn create(
        &self,
        _owner_scope: &str,
        _session: PersistedSession,
        _idempotency: IdempotencyRecord,
        _lifecycle_facts: Vec<SessionLifecycleFact>,
    ) -> Result<SessionRevision, SessionRepositoryError> {
        Err(SessionRepositoryError::Storage(
            "repository does not implement root Session create".into(),
        ))
    }

    /// Commit the one root-revision CAS transaction.
    async fn commit_mutation(
        &self,
        _owner_scope: &str,
        _mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        Err(SessionRepositoryError::Storage(
            "repository does not implement root Session mutation".into(),
        ))
    }

    /// Persist a session and its owning scope in one repository operation.
    ///
    /// The owner is part of the adapter-side persistence envelope rather than the
    /// tenancy-agnostic [`PersistedSession`] value. Keeping both arguments on one
    /// required method makes the crash invariant explicit: a visible session row
    /// can never exist without its ownership fence.
    async fn save_owned(&self, owner_scope: &str, mut session: PersistedSession) {
        loop {
            let Some(current) = self.get(&session.session_id).await else {
                session.revision = SessionRevision(0);
                let payload = SessionMutationPayload::Replace(session.clone());
                let hash = payload.stable_hash();
                let created = self
                    .create(
                        owner_scope,
                        session.clone(),
                        IdempotencyRecord {
                            key: format!("compat:save:0:{hash}"),
                            payload_hash: hash,
                        },
                        Vec::new(),
                    )
                    .await;
                match created {
                    Ok(_) => return,
                    Err(SessionRepositoryError::AlreadyExists) => continue,
                    Err(error) => panic!("persist managed Session: {error}"),
                }
            };
            if self.owner(&session.session_id).await.as_deref() != Some(owner_scope) {
                panic!("cannot move a managed Session between owner scopes");
            }
            session.revision = current.revision;
            let payload = SessionMutationPayload::Replace(session.clone());
            let hash = payload.stable_hash();
            let result = self
                .commit_mutation(
                    owner_scope,
                    SessionMutation {
                        expected_revision: current.revision,
                        idempotency: IdempotencyRecord {
                            key: format!("compat:save:{}:{hash}", current.revision.0),
                            payload_hash: hash,
                        },
                        payload,
                        lifecycle_facts: Vec::new(),
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("persist managed Session: {error}"));
            match result {
                SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. } => {
                    return;
                }
                SessionMutationResult::Conflict { .. } => continue,
                SessionMutationResult::IdempotencyMismatch => {
                    panic!("managed Session compatibility idempotency mismatch")
                }
            }
        }
    }

    /// Atomically persist the session, owner fence, and lifecycle outbox fact.
    async fn save_owned_with_lifecycle(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        fact: SessionLifecycleFact,
    ) {
        loop {
            let Some(current) = self.get(&session.session_id).await else {
                session.revision = SessionRevision(0);
                let payload = SessionMutationPayload::Replace(session.clone());
                let hash = payload.stable_hash();
                match self
                    .create(
                        owner_scope,
                        session.clone(),
                        IdempotencyRecord {
                            key: format!("compat:save-lifecycle:0:{hash}"),
                            payload_hash: hash,
                        },
                        vec![fact.clone()],
                    )
                    .await
                {
                    Ok(_) => return,
                    Err(SessionRepositoryError::AlreadyExists) => continue,
                    Err(error) => panic!("persist managed Session lifecycle: {error}"),
                }
            };
            if self.owner(&session.session_id).await.as_deref() != Some(owner_scope) {
                panic!("cannot move a managed Session between owner scopes");
            }
            session.revision = current.revision;
            let payload = SessionMutationPayload::Replace(session.clone());
            let hash = payload.stable_hash();
            let result = self
                .commit_mutation(
                    owner_scope,
                    SessionMutation {
                        expected_revision: current.revision,
                        idempotency: IdempotencyRecord {
                            key: format!("compat:save-lifecycle:{}:{hash}", current.revision.0),
                            payload_hash: hash,
                        },
                        payload,
                        lifecycle_facts: vec![fact.clone()],
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("persist managed Session lifecycle: {error}"));
            match result {
                SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. } => {
                    return;
                }
                SessionMutationResult::Conflict { .. } => continue,
                SessionMutationResult::IdempotencyMismatch => {
                    panic!("managed Session compatibility idempotency mismatch")
                }
            }
        }
    }

    /// Commit a lifecycle transition fact idempotently by stable id.
    async fn append_lifecycle(&self, fact: SessionLifecycleFact);

    /// Atomically mark the durable session terminated and commit its fact.
    async fn archive_with_lifecycle(
        &self,
        session_id: &str,
        archived_at: &str,
        fact: SessionLifecycleFact,
    ) {
        loop {
            let Some(mut current) = self.get(session_id).await else {
                return;
            };
            let owner = self
                .owner(session_id)
                .await
                .unwrap_or_else(|| "default".into());
            current.status = "terminated".into();
            current.archived_at = Some(archived_at.into());
            let payload = SessionMutationPayload::Replace(current.clone());
            let hash = payload.stable_hash();
            match self
                .commit_mutation(
                    &owner,
                    SessionMutation {
                        expected_revision: current.revision,
                        idempotency: IdempotencyRecord {
                            key: format!("compat:archive:{}:{hash}", current.revision.0),
                            payload_hash: hash,
                        },
                        payload,
                        lifecycle_facts: vec![fact.clone()],
                    },
                )
                .await
                .expect("archive managed Session")
            {
                SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. } => {
                    return;
                }
                SessionMutationResult::Conflict { .. } => continue,
                SessionMutationResult::IdempotencyMismatch => {
                    panic!("managed Session archive idempotency mismatch")
                }
            }
        }
    }

    /// Atomically delete the session row and commit its terminal fact.
    async fn delete_with_lifecycle(&self, session_id: &str, fact: SessionLifecycleFact) {
        loop {
            let Some(current) = self.get(session_id).await else {
                return;
            };
            let owner = self
                .owner(session_id)
                .await
                .unwrap_or_else(|| "default".into());
            let deleted_revision = SessionRevision(
                current
                    .revision
                    .0
                    .checked_add(1)
                    .expect("Session revision exhausted"),
            );
            let payload = SessionMutationPayload::Delete(SessionTombstone {
                session_id: session_id.into(),
                deleted_revision,
                deleted_at: fact.timestamp.to_string(),
            });
            let hash = payload.stable_hash();
            match self
                .commit_mutation(
                    &owner,
                    SessionMutation {
                        expected_revision: current.revision,
                        idempotency: IdempotencyRecord {
                            key: format!("compat:delete:{}:{hash}", current.revision.0),
                            payload_hash: hash,
                        },
                        payload,
                        lifecycle_facts: vec![fact.clone()],
                    },
                )
                .await
                .expect("delete managed Session")
            {
                SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. } => {
                    return;
                }
                SessionMutationResult::Conflict { .. } => continue,
                SessionMutationResult::IdempotencyMismatch => {
                    panic!("managed Session delete idempotency mismatch")
                }
            }
        }
    }

    async fn pending_lifecycle(&self) -> Vec<SessionLifecycleFact>;

    async fn complete_lifecycle(&self, fact_id: &str);

    /// Persist under the self-hosted default scope. Production request paths with
    /// an edge-resolved owner use [`Self::save_owned`] directly; this convenience
    /// keeps scope-free local/test callers deterministic without reintroducing a
    /// second owner write.
    async fn save(&self, session: PersistedSession) {
        self.save_owned("default", session).await;
    }

    /// The stored configuration for `session_id`, if any.
    async fn get(&self, session_id: &str) -> Option<PersistedSession>;

    /// Atomically attach the runtime's opaque environment binding to an existing
    /// Session row. Returns `false` when the Session is unknown. This narrow
    /// update avoids overwriting concurrent resource/lifecycle mutations with a
    /// stale aggregate snapshot.
    async fn bind_environment(&self, session_id: &str, binding: &str) -> bool {
        loop {
            let Some(mut current) = self.get(session_id).await else {
                return false;
            };
            let owner = self
                .owner(session_id)
                .await
                .unwrap_or_else(|| "default".into());
            current.environment_binding = Some(binding.into());
            let payload = SessionMutationPayload::Replace(current.clone());
            let hash = payload.stable_hash();
            match self
                .commit_mutation(
                    &owner,
                    SessionMutation {
                        expected_revision: current.revision,
                        idempotency: IdempotencyRecord {
                            key: format!("compat:bind:{}:{hash}", current.revision.0),
                            payload_hash: hash,
                        },
                        payload,
                        lifecycle_facts: Vec::new(),
                    },
                )
                .await
                .expect("bind managed Session environment")
            {
                SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. } => {
                    return true;
                }
                SessionMutationResult::Conflict { .. } => continue,
                SessionMutationResult::IdempotencyMismatch => {
                    panic!("managed Session bind idempotency mismatch")
                }
            }
        }
    }

    /// Sessions with a Prepared/Releasing resource transition. Implementations
    /// must preserve their ordinary tenancy fence; the coordinator obtains the
    /// already-trusted owner separately through [`Self::owner`].
    async fn pending_resource_sessions(&self) -> Vec<ScopedPersistedSession> {
        Vec::new()
    }

    /// The atomically persisted owner scope of `session_id`, if the row exists.
    async fn owner(&self, _session_id: &str) -> Option<String> {
        None
    }
}

// In-memory and durable adapters live outward in `awaken-session-store`.
// Workspace ownership is persisted atomically beside each row through
// `save_owned*`; authorization scope decorators do not belong in this resource
// persistence port.

#[cfg(test)]
mod mutation_tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum PayloadKind {
        Replace,
        Delete,
    }

    #[derive(Clone)]
    struct Rule {
        id: &'static str,
        payload: PayloadKind,
        key_nonempty: bool,
        hash_nonempty: bool,
        session_id_nonempty: bool,
        revision_available: bool,
        payload_revision_exact: bool,
        lifecycle_session_exact: bool,
        expected: Result<SessionRevision, SessionMutationValidationError>,
    }

    fn session(id: &str, revision: SessionRevision) -> PersistedSession {
        PersistedSession {
            session_id: id.into(),
            revision,
            agent_id: "assistant".into(),
            model: "model".into(),
            title: None,
            metadata: Default::default(),
            environment_id: "environment".into(),
            environment_binding: None,
            runtime: Default::default(),
            mcp_servers: Vec::new(),
            resources: Default::default(),
            status: "idle".into(),
            archived_at: None,
        }
    }

    /// Cause-effect graph:
    ///
    /// C1 key present -> C2 hash present -> C3 Session id present
    /// -> C4 next revision exists -> C5 payload revision is exact
    /// -> C6 every lifecycle fact targets the same Session -> E1 next revision.
    /// Every failed cause yields its stable E2 validation error and no write.
    ///
    /// Decision table (`-` means evaluation already terminated):
    ///
    /// | Rule | Kind | C1 | C2 | C3 | C4 | C5 | C6 | Result |
    /// |---|---|---|---|---|---|---|---|---|
    /// | R1 | replace | T | T | T | T | T | T | revision 8 |
    /// | R2 | delete | T | T | T | T | T | T | revision 8 |
    /// | R3 | either | F | - | - | - | - | - | empty key |
    /// | R4 | either | T | F | - | - | - | - | empty hash |
    /// | R5 | either | T | T | F | - | - | - | empty id |
    /// | R6 | either | T | T | T | F | - | - | exhausted |
    /// | R7 | replace | T | T | T | T | F | - | replace mismatch |
    /// | R8 | delete | T | T | T | T | F | - | tombstone mismatch |
    /// | R9 | either | T | T | T | T | T | F | lifecycle mismatch |
    #[test]
    fn mutation_validation_tests_are_generated_from_the_decision_table() {
        let rules = [
            Rule {
                id: "R1",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Ok(SessionRevision(8)),
            },
            Rule {
                id: "R2",
                payload: PayloadKind::Delete,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Ok(SessionRevision(8)),
            },
            Rule {
                id: "R3",
                payload: PayloadKind::Replace,
                key_nonempty: false,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptyIdempotencyKey),
            },
            Rule {
                id: "R4",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: false,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptyPayloadHash),
            },
            Rule {
                id: "R5",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: false,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptySessionId),
            },
            Rule {
                id: "R6",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: false,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::RevisionExhausted),
            },
            Rule {
                id: "R7",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: false,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::ReplacementRevisionMismatch),
            },
            Rule {
                id: "R8",
                payload: PayloadKind::Delete,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: false,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::TombstoneRevisionMismatch),
            },
            Rule {
                id: "R9",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: false,
                expected: Err(SessionMutationValidationError::LifecycleSessionMismatch),
            },
        ];

        for rule in rules {
            let expected_revision = if rule.revision_available {
                SessionRevision(7)
            } else {
                SessionRevision(u64::MAX)
            };
            let session_id = if rule.session_id_nonempty {
                "session-1"
            } else {
                ""
            };
            let next = expected_revision.0.checked_add(1).unwrap_or_default();
            let payload = match rule.payload {
                PayloadKind::Replace => SessionMutationPayload::Replace(session(
                    session_id,
                    if rule.payload_revision_exact {
                        expected_revision
                    } else {
                        SessionRevision(expected_revision.0.saturating_sub(1))
                    },
                )),
                PayloadKind::Delete => SessionMutationPayload::Delete(SessionTombstone {
                    session_id: session_id.into(),
                    deleted_revision: if rule.payload_revision_exact {
                        SessionRevision(next)
                    } else {
                        expected_revision
                    },
                    deleted_at: "2026-07-25T00:00:00Z".into(),
                }),
            };
            let mutation = SessionMutation {
                expected_revision,
                idempotency: IdempotencyRecord {
                    key: if rule.key_nonempty { "request-1" } else { "" }.into(),
                    payload_hash: if rule.hash_nonempty {
                        "sha256:payload"
                    } else {
                        ""
                    }
                    .into(),
                },
                payload,
                lifecycle_facts: vec![SessionLifecycleFact {
                    id: "fact-1".into(),
                    session_id: if rule.lifecycle_session_exact {
                        session_id
                    } else {
                        "another-session"
                    }
                    .into(),
                    workspace_id: Some("workspace".into()),
                    event_type: "session.updated".into(),
                    timestamp: 1,
                }],
            };
            assert_eq!(
                mutation.validate(),
                rule.expected,
                "decision rule {}",
                rule.id
            );
        }
    }
}
