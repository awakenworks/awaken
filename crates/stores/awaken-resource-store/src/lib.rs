//! Durable resource-lifecycle state adapters.
//!
//! Purge intents and reverse references are resource-plane operational data. They
//! deliberately live outside IAM storage and contain no principal, role, API key
//! or policy. The same ports support the in-process local composition and a
//! replaceable durable adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_resource_contract::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourceKind, ResourcePurgeError,
    ResourcePurgeIntent, ResourcePurgeRepository, ResourceReclamationFence, ResourceReference,
    ResourceReferenceIndex, ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
#[cfg(feature = "sqlite")]
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(feature = "postgres")]
mod schema;

#[cfg(feature = "postgres")]
pub use postgres::PostgresResourceStore;

#[derive(Default)]
struct ResourceConsistencyState {
    references: BTreeSet<ResourceReferenceRecord>,
    reclamation_fences: BTreeMap<(ResourceKind, String), String>,
}

/// Ephemeral reference adapter used by embedded tests and no-storage mode.
#[derive(Default)]
pub struct InMemoryResourceStore {
    intents: Mutex<BTreeMap<String, ResourcePurgeIntent>>,
    consistency: Mutex<ResourceConsistencyState>,
}

#[async_trait]
impl ResourceReclamationFence for InMemoryResourceStore {
    async fn acquire_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<AcquireResourceReclamationOutcome, ResourcePurgeError> {
        validate_fence_request(intent_id, target)?;
        let mut state = self
            .consistency
            .lock()
            .map_err(|error| storage(error.to_string()))?;
        let key = physical_key(target);
        if let Some(owner) = state.reclamation_fences.get(&key) {
            return Ok(if owner == intent_id {
                AcquireResourceReclamationOutcome::AlreadyOwned
            } else {
                AcquireResourceReclamationOutcome::Contended
            });
        }
        let blockers = references_for_identity(&state.references, target.kind, &target.resource_id);
        if !blockers.is_empty() {
            return Ok(AcquireResourceReclamationOutcome::Blocked(blockers));
        }
        state.reclamation_fences.insert(key, intent_id.into());
        Ok(AcquireResourceReclamationOutcome::Acquired)
    }

    async fn release_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<bool, ResourcePurgeError> {
        validate_fence_request(intent_id, target)?;
        let mut state = self
            .consistency
            .lock()
            .map_err(|error| storage(error.to_string()))?;
        let key = physical_key(target);
        match state.reclamation_fences.get(&key) {
            Some(owner) if owner == intent_id => {
                state.reclamation_fences.remove(&key);
                Ok(true)
            }
            Some(_) => Err(ResourcePurgeError::StaleReclamationFence),
            None => Ok(false),
        }
    }
}

impl InMemoryResourceStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ResourcePurgeRepository for InMemoryResourceStore {
    async fn put(
        &self,
        intent: ResourcePurgeIntent,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        intent.validate()?;
        let mut rows = self
            .intents
            .lock()
            .map_err(|error| storage(error.to_string()))?;
        if let Some(existing) = rows.get(&intent.intent_id).or_else(|| {
            rows.values()
                .find(|row| row.idempotency_key == intent.idempotency_key)
        }) {
            return if existing.same_request(&intent) {
                Ok(PutResourcePurgeOutcome::Existing)
            } else {
                Err(ResourcePurgeError::IdempotencyConflict(
                    intent.idempotency_key,
                ))
            };
        }
        rows.insert(intent.intent_id.clone(), intent);
        Ok(PutResourcePurgeOutcome::Inserted)
    }

    async fn get(
        &self,
        intent_id: &str,
    ) -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError> {
        Ok(self
            .intents
            .lock()
            .map_err(|error| storage(error.to_string()))?
            .get(intent_id)
            .cloned())
    }

    async fn recoverable(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError> {
        let mut rows: Vec<_> = self
            .intents
            .lock()
            .map_err(|error| storage(error.to_string()))?
            .values()
            .filter(|intent| recoverable(intent, now_unix_ms))
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            a.requested_at_unix_ms
                .cmp(&b.requested_at_unix_ms)
                .then_with(|| a.intent_id.cmp(&b.intent_id))
        });
        rows.truncate(limit);
        Ok(rows)
    }

    async fn save(
        &self,
        expected_revision: u64,
        intent: ResourcePurgeIntent,
    ) -> Result<(), ResourcePurgeError> {
        intent.validate()?;
        let mut rows = self
            .intents
            .lock()
            .map_err(|error| storage(error.to_string()))?;
        let current = rows
            .get(&intent.intent_id)
            .ok_or_else(|| ResourcePurgeError::NotFound(intent.intent_id.clone()))?;
        if current.revision != expected_revision {
            return Err(ResourcePurgeError::RevisionConflict(intent.intent_id));
        }
        rows.insert(intent.intent_id.clone(), intent);
        Ok(())
    }
}

#[async_trait]
impl ResourceReferenceIndex for InMemoryResourceStore {
    async fn add_reference(
        &self,
        record: ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        validate_reference(&record)?;
        let mut state = self
            .consistency
            .lock()
            .map_err(|error| storage(error.to_string()))?;
        ensure_unfenced(&state.reclamation_fences, &record.target)?;
        Ok(state.references.insert(record))
    }

    async fn remove_reference(
        &self,
        record: &ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        validate_reference(record)?;
        Ok(self
            .consistency
            .lock()
            .map_err(|error| storage(error.to_string()))?
            .references
            .remove(record))
    }

    async fn replace_references(
        &self,
        kind: ResourceReferenceKind,
        reference_id: &str,
        records: Vec<ResourceReferenceRecord>,
    ) -> Result<(), ResourcePurgeError> {
        validate_replacement(kind, reference_id, &records)?;
        let mut state = self
            .consistency
            .lock()
            .map_err(|error| storage(error.to_string()))?;
        for record in &records {
            ensure_unfenced(&state.reclamation_fences, &record.target)?;
        }
        state.references.retain(|record| {
            record.reference.kind != kind || record.reference.reference_id != reference_id
        });
        state.references.extend(records);
        Ok(())
    }

    async fn references(
        &self,
        target: &ResourceTarget,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        let rows = self
            .consistency
            .lock()
            .map_err(|error| storage(error.to_string()))?;
        Ok(rows
            .references
            .iter()
            .filter(|record| &record.target == target)
            .map(|record| record.reference.clone())
            .collect())
    }

    async fn references_for_resource(
        &self,
        kind: ResourceKind,
        resource_id: &str,
    ) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
        Ok(self
            .consistency
            .lock()
            .map_err(|error| storage(error.to_string()))?
            .references
            .iter()
            .filter(|record| record.target.kind == kind && record.target.resource_id == resource_id)
            .cloned()
            .collect())
    }
}

/// SQLite adapter used by the durable single-machine composition.
#[cfg(feature = "sqlite")]
pub struct SqliteResourceStore {
    connection: Mutex<Connection>,
}

#[cfg(feature = "sqlite")]
impl SqliteResourceStore {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, ResourcePurgeError> {
        let connection = Connection::open(path).map_err(|error| storage(error.to_string()))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS resource_purge_intents (
                   intent_id TEXT PRIMARY KEY,
                   idempotency_key TEXT NOT NULL UNIQUE,
                   revision INTEGER NOT NULL,
                   status TEXT NOT NULL,
                   requested_at_unix_ms INTEGER NOT NULL,
                   not_before_unix_ms INTEGER NOT NULL,
                   lease_expires_at_unix_ms INTEGER,
                   data TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS resource_purge_recoverable
                   ON resource_purge_intents(status, not_before_unix_ms, lease_expires_at_unix_ms);
                 CREATE TABLE IF NOT EXISTS resource_references (
                   workspace_id TEXT NOT NULL,
                   resource_kind TEXT NOT NULL,
                   resource_id TEXT NOT NULL,
                   reference_kind TEXT NOT NULL,
                   reference_id TEXT NOT NULL,
                   PRIMARY KEY(workspace_id, resource_kind, resource_id, reference_kind, reference_id)
                 );
                 CREATE INDEX IF NOT EXISTS resource_references_reverse
                   ON resource_references(resource_kind, resource_id);
                 CREATE TABLE IF NOT EXISTS resource_reclamation_fences (
                   resource_kind TEXT NOT NULL,
                   resource_id TEXT NOT NULL,
                   intent_id TEXT NOT NULL,
                   PRIMARY KEY(resource_kind, resource_id)
                 );",
            )
            .map_err(|error| storage(error.to_string()))?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ResourcePurgeError> {
        self.connection
            .lock()
            .map_err(|error| storage(error.to_string()))
    }
}

#[async_trait]
#[cfg(feature = "sqlite")]
impl ResourceReclamationFence for SqliteResourceStore {
    async fn acquire_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<AcquireResourceReclamationOutcome, ResourcePurgeError> {
        validate_fence_request(intent_id, target)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| storage(error.to_string()))?;
        let existing = transaction
            .query_row(
                "SELECT intent_id FROM resource_reclamation_fences
                 WHERE resource_kind = ?1 AND resource_id = ?2",
                params![kind_name(target.kind), target.resource_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage(error.to_string()))?;
        if let Some(owner) = existing {
            return Ok(if owner == intent_id {
                AcquireResourceReclamationOutcome::AlreadyOwned
            } else {
                AcquireResourceReclamationOutcome::Contended
            });
        }
        let blockers =
            sqlite_references_for_identity(&transaction, target.kind, &target.resource_id)?;
        if !blockers.is_empty() {
            return Ok(AcquireResourceReclamationOutcome::Blocked(blockers));
        }
        transaction
            .execute(
                "INSERT INTO resource_reclamation_fences
                 (resource_kind, resource_id, intent_id) VALUES (?1, ?2, ?3)",
                params![kind_name(target.kind), target.resource_id, intent_id],
            )
            .map_err(|error| storage(error.to_string()))?;
        transaction
            .commit()
            .map_err(|error| storage(error.to_string()))?;
        Ok(AcquireResourceReclamationOutcome::Acquired)
    }

    async fn release_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<bool, ResourcePurgeError> {
        validate_fence_request(intent_id, target)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| storage(error.to_string()))?;
        let existing = transaction
            .query_row(
                "SELECT intent_id FROM resource_reclamation_fences
                 WHERE resource_kind = ?1 AND resource_id = ?2",
                params![kind_name(target.kind), target.resource_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage(error.to_string()))?;
        let changed = match existing {
            Some(owner) if owner == intent_id => {
                transaction
                    .execute(
                        "DELETE FROM resource_reclamation_fences
                     WHERE resource_kind = ?1 AND resource_id = ?2 AND intent_id = ?3",
                        params![kind_name(target.kind), target.resource_id, intent_id],
                    )
                    .map_err(|error| storage(error.to_string()))?
                    == 1
            }
            Some(_) => return Err(ResourcePurgeError::StaleReclamationFence),
            None => false,
        };
        transaction
            .commit()
            .map_err(|error| storage(error.to_string()))?;
        Ok(changed)
    }
}

#[async_trait]
#[cfg(feature = "sqlite")]
impl ResourcePurgeRepository for SqliteResourceStore {
    async fn put(
        &self,
        intent: ResourcePurgeIntent,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        intent.validate()?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage(error.to_string()))?;
        let existing = transaction
            .query_row(
                "SELECT data FROM resource_purge_intents
                 WHERE intent_id = ?1 OR idempotency_key = ?2 LIMIT 1",
                params![intent.intent_id, intent.idempotency_key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage(error.to_string()))?;
        if let Some(data) = existing {
            let existing = decode_intent(&data)?;
            return if existing.same_request(&intent) {
                Ok(PutResourcePurgeOutcome::Existing)
            } else {
                Err(ResourcePurgeError::IdempotencyConflict(
                    intent.idempotency_key,
                ))
            };
        }
        let data = encode_intent(&intent)?;
        transaction
            .execute(
                "INSERT INTO resource_purge_intents
                 (intent_id, idempotency_key, revision, status, requested_at_unix_ms,
                  not_before_unix_ms, lease_expires_at_unix_ms, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    intent.intent_id,
                    intent.idempotency_key,
                    to_i64(intent.revision)?,
                    status_name(intent.status),
                    to_i64(intent.requested_at_unix_ms)?,
                    to_i64(intent.not_before_unix_ms)?,
                    intent.lease_expires_at_unix_ms.map(to_i64).transpose()?,
                    data,
                ],
            )
            .map_err(|error| storage(error.to_string()))?;
        transaction
            .commit()
            .map_err(|error| storage(error.to_string()))?;
        Ok(PutResourcePurgeOutcome::Inserted)
    }

    async fn get(
        &self,
        intent_id: &str,
    ) -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError> {
        self.connection()?
            .query_row(
                "SELECT data FROM resource_purge_intents WHERE intent_id = ?1",
                params![intent_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage(error.to_string()))?
            .map(|data| decode_intent(&data))
            .transpose()
    }

    async fn recoverable(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT data FROM resource_purge_intents
                 WHERE status NOT IN ('completed', 'terminal_failed')
                   AND not_before_unix_ms <= ?1
                   AND (lease_expires_at_unix_ms IS NULL OR lease_expires_at_unix_ms <= ?1)
                 ORDER BY requested_at_unix_ms, intent_id LIMIT ?2",
            )
            .map_err(|error| storage(error.to_string()))?;
        let rows = statement
            .query_map(
                params![to_i64(now_unix_ms)?, to_i64(limit as u64)?],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| storage(error.to_string()))?;
        rows.map(|row| {
            let data = row.map_err(|error| storage(error.to_string()))?;
            decode_intent(&data)
        })
        .collect()
    }

    async fn save(
        &self,
        expected_revision: u64,
        intent: ResourcePurgeIntent,
    ) -> Result<(), ResourcePurgeError> {
        intent.validate()?;
        let data = encode_intent(&intent)?;
        let changed = self
            .connection()?
            .execute(
                "UPDATE resource_purge_intents
                 SET revision = ?3, status = ?4, lease_expires_at_unix_ms = ?5, data = ?6
                 WHERE intent_id = ?1 AND revision = ?2",
                params![
                    intent.intent_id,
                    to_i64(expected_revision)?,
                    to_i64(intent.revision)?,
                    status_name(intent.status),
                    intent.lease_expires_at_unix_ms.map(to_i64).transpose()?,
                    data,
                ],
            )
            .map_err(|error| storage(error.to_string()))?;
        if changed == 1 {
            Ok(())
        } else if self.get(&intent.intent_id).await?.is_some() {
            Err(ResourcePurgeError::RevisionConflict(intent.intent_id))
        } else {
            Err(ResourcePurgeError::NotFound(intent.intent_id))
        }
    }
}

#[async_trait]
#[cfg(feature = "sqlite")]
impl ResourceReferenceIndex for SqliteResourceStore {
    async fn add_reference(
        &self,
        record: ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        validate_reference(&record)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| storage(error.to_string()))?;
        sqlite_ensure_unfenced(&transaction, &record.target)?;
        let changed = transaction
            .execute(
                "INSERT OR IGNORE INTO resource_references
                 (workspace_id, resource_kind, resource_id, reference_kind, reference_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                reference_params(&record),
            )
            .map_err(|error| storage(error.to_string()))?
            == 1;
        transaction
            .commit()
            .map_err(|error| storage(error.to_string()))?;
        Ok(changed)
    }

    async fn remove_reference(
        &self,
        record: &ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        validate_reference(record)?;
        Ok(self
            .connection()?
            .execute(
                "DELETE FROM resource_references
                 WHERE workspace_id = ?1 AND resource_kind = ?2 AND resource_id = ?3
                   AND reference_kind = ?4 AND reference_id = ?5",
                reference_params(record),
            )
            .map_err(|error| storage(error.to_string()))?
            == 1)
    }

    async fn replace_references(
        &self,
        kind: ResourceReferenceKind,
        reference_id: &str,
        records: Vec<ResourceReferenceRecord>,
    ) -> Result<(), ResourcePurgeError> {
        validate_replacement(kind, reference_id, &records)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| storage(error.to_string()))?;
        for record in &records {
            sqlite_ensure_unfenced(&transaction, &record.target)?;
        }
        transaction
            .execute(
                "DELETE FROM resource_references WHERE reference_kind = ?1 AND reference_id = ?2",
                params![reference_kind_name(kind), reference_id],
            )
            .map_err(|error| storage(error.to_string()))?;
        for record in records {
            transaction
                .execute(
                    "INSERT INTO resource_references
                     (workspace_id, resource_kind, resource_id, reference_kind, reference_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    reference_params(&record),
                )
                .map_err(|error| storage(error.to_string()))?;
        }
        transaction
            .commit()
            .map_err(|error| storage(error.to_string()))
    }

    async fn references(
        &self,
        target: &ResourceTarget,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT reference_kind, reference_id FROM resource_references
                 WHERE workspace_id = ?1 AND resource_kind = ?2 AND resource_id = ?3
                 ORDER BY reference_kind, reference_id",
            )
            .map_err(|error| storage(error.to_string()))?;
        let rows = statement
            .query_map(
                params![
                    target.workspace_id,
                    kind_name(target.kind),
                    target.resource_id
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|error| storage(error.to_string()))?;
        rows.map(|row| {
            let (kind, reference_id) = row.map_err(|error| storage(error.to_string()))?;
            Ok(ResourceReference {
                kind: parse_reference_kind(&kind)?,
                reference_id,
            })
        })
        .collect()
    }

    async fn references_for_resource(
        &self,
        kind: ResourceKind,
        resource_id: &str,
    ) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT workspace_id, reference_kind, reference_id FROM resource_references
                 WHERE resource_kind = ?1 AND resource_id = ?2
                 ORDER BY workspace_id, reference_kind, reference_id",
            )
            .map_err(|error| storage(error.to_string()))?;
        let rows = statement
            .query_map(params![kind_name(kind), resource_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| storage(error.to_string()))?;
        rows.map(|row| {
            let (workspace_id, reference_kind, reference_id) =
                row.map_err(|error| storage(error.to_string()))?;
            Ok(ResourceReferenceRecord {
                target: ResourceTarget::new(workspace_id, kind, resource_id),
                reference: ResourceReference {
                    kind: parse_reference_kind(&reference_kind)?,
                    reference_id,
                },
            })
        })
        .collect()
    }
}

fn physical_key(target: &ResourceTarget) -> (ResourceKind, String) {
    (target.kind, target.resource_id.clone())
}

fn references_for_identity(
    references: &BTreeSet<ResourceReferenceRecord>,
    kind: ResourceKind,
    resource_id: &str,
) -> Vec<ResourceReferenceRecord> {
    references
        .iter()
        .filter(|record| record.target.kind == kind && record.target.resource_id == resource_id)
        .cloned()
        .collect()
}

fn ensure_unfenced(
    fences: &BTreeMap<(ResourceKind, String), String>,
    target: &ResourceTarget,
) -> Result<(), ResourcePurgeError> {
    if fences.contains_key(&physical_key(target)) {
        Err(ResourcePurgeError::ReclamationFenced {
            kind: target.kind,
            resource_id: target.resource_id.clone(),
        })
    } else {
        Ok(())
    }
}

fn validate_fence_request(
    intent_id: &str,
    target: &ResourceTarget,
) -> Result<(), ResourcePurgeError> {
    if intent_id.trim().is_empty()
        || target.workspace_id.trim().is_empty()
        || target.resource_id.trim().is_empty()
    {
        Err(ResourcePurgeError::Invalid(
            "reclamation fence fields must not be empty".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_ensure_unfenced(
    connection: &Connection,
    target: &ResourceTarget,
) -> Result<(), ResourcePurgeError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM resource_reclamation_fences
             WHERE resource_kind = ?1 AND resource_id = ?2",
            params![kind_name(target.kind), target.resource_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(|error| storage(error.to_string()))?
        .is_some();
    if exists {
        Err(ResourcePurgeError::ReclamationFenced {
            kind: target.kind,
            resource_id: target.resource_id.clone(),
        })
    } else {
        Ok(())
    }
}

#[cfg(feature = "sqlite")]
fn sqlite_references_for_identity(
    connection: &Connection,
    kind: ResourceKind,
    resource_id: &str,
) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
    let mut statement = connection
        .prepare(
            "SELECT workspace_id, reference_kind, reference_id FROM resource_references
             WHERE resource_kind = ?1 AND resource_id = ?2
             ORDER BY workspace_id, reference_kind, reference_id",
        )
        .map_err(|error| storage(error.to_string()))?;
    let rows = statement
        .query_map(params![kind_name(kind), resource_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| storage(error.to_string()))?;
    rows.map(|row| {
        let (workspace_id, reference_kind, reference_id) =
            row.map_err(|error| storage(error.to_string()))?;
        Ok(ResourceReferenceRecord {
            target: ResourceTarget::new(workspace_id, kind, resource_id),
            reference: ResourceReference {
                kind: parse_reference_kind(&reference_kind)?,
                reference_id,
            },
        })
    })
    .collect()
}

fn recoverable(intent: &ResourcePurgeIntent, now_unix_ms: u64) -> bool {
    !intent.status.is_terminal()
        && intent.not_before_unix_ms <= now_unix_ms
        && intent
            .lease_expires_at_unix_ms
            .is_none_or(|expires| expires <= now_unix_ms)
}

pub(crate) fn validate_reference(
    record: &ResourceReferenceRecord,
) -> Result<(), ResourcePurgeError> {
    if record.target.workspace_id.trim().is_empty()
        || record.target.resource_id.trim().is_empty()
        || record.reference.reference_id.trim().is_empty()
    {
        return Err(ResourcePurgeError::Invalid(
            "resource reference fields must not be empty".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_replacement(
    kind: ResourceReferenceKind,
    reference_id: &str,
    records: &[ResourceReferenceRecord],
) -> Result<(), ResourcePurgeError> {
    if reference_id.trim().is_empty() {
        return Err(ResourcePurgeError::Invalid(
            "replacement reference_id must not be empty".into(),
        ));
    }
    for record in records {
        validate_reference(record)?;
        if record.reference.kind != kind || record.reference.reference_id != reference_id {
            return Err(ResourcePurgeError::Invalid(
                "replacement rows must belong to the requested holder".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn encode_intent(intent: &ResourcePurgeIntent) -> Result<String, ResourcePurgeError> {
    serde_json::to_string(intent).map_err(|error| storage(error.to_string()))
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn decode_intent(data: &str) -> Result<ResourcePurgeIntent, ResourcePurgeError> {
    let intent: ResourcePurgeIntent =
        serde_json::from_str(data).map_err(|error| storage(error.to_string()))?;
    intent.validate()?;
    Ok(intent)
}

pub(crate) fn storage(error: impl ToString) -> ResourcePurgeError {
    ResourcePurgeError::Storage(error.to_string())
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn to_i64(value: u64) -> Result<i64, ResourcePurgeError> {
    i64::try_from(value).map_err(|_| ResourcePurgeError::Invalid("integer exceeds i64".into()))
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn kind_name(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::File => "file",
        ResourceKind::MemoryStore => "memory_store",
        ResourceKind::Repository => "repository",
        ResourceKind::Skill => "skill",
    }
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn status_name(status: awaken_resource_contract::ResourcePurgeStatus) -> &'static str {
    use awaken_resource_contract::ResourcePurgeStatus;
    match status {
        ResourcePurgeStatus::Pending => "pending",
        ResourcePurgeStatus::Claimed => "claimed",
        ResourcePurgeStatus::Completed => "completed",
    }
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn reference_kind_name(kind: ResourceReferenceKind) -> &'static str {
    match kind {
        ResourceReferenceKind::LogicalLifecycle => "logical_lifecycle",
        ResourceReferenceKind::WorkspaceOwnership => "workspace_ownership",
        ResourceReferenceKind::AgentBinding => "agent_binding",
        ResourceReferenceKind::SessionBinding => "session_binding",
        ResourceReferenceKind::Artifact => "artifact",
        ResourceReferenceKind::RuntimeHandle => "runtime_handle",
        ResourceReferenceKind::ExtractionIntent => "extraction_intent",
        ResourceReferenceKind::RetentionHold => "retention_hold",
    }
}

#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub(crate) fn parse_reference_kind(
    value: &str,
) -> Result<ResourceReferenceKind, ResourcePurgeError> {
    match value {
        "logical_lifecycle" => Ok(ResourceReferenceKind::LogicalLifecycle),
        // Read the pre-separation spelling so an upgrade cannot make an existing
        // File ownership edge invisible to reclamation safety checks.
        "workspace_ownership" | "workspace_grant" => Ok(ResourceReferenceKind::WorkspaceOwnership),
        "agent_binding" => Ok(ResourceReferenceKind::AgentBinding),
        "session_binding" => Ok(ResourceReferenceKind::SessionBinding),
        "artifact" => Ok(ResourceReferenceKind::Artifact),
        "runtime_handle" => Ok(ResourceReferenceKind::RuntimeHandle),
        "extraction_intent" => Ok(ResourceReferenceKind::ExtractionIntent),
        "retention_hold" => Ok(ResourceReferenceKind::RetentionHold),
        other => Err(storage(format!(
            "unknown resource reference kind `{other}`"
        ))),
    }
}

#[cfg(feature = "sqlite")]
fn reference_params(record: &ResourceReferenceRecord) -> [String; 5] {
    [
        record.target.workspace_id.clone(),
        kind_name(record.target.kind).into(),
        record.target.resource_id.clone(),
        reference_kind_name(record.reference.kind).into(),
        record.reference.reference_id.clone(),
    ]
}

#[cfg(test)]
mod tests {
    use awaken_resource_contract::{ResourcePurgeStatus, ResourceReferenceKind};
    use proptest::prelude::*;

    use super::*;

    fn intent(id: &str) -> ResourcePurgeIntent {
        ResourcePurgeIntent::new(
            id,
            format!("delete:{id}"),
            ResourceTarget::new("workspace-a", ResourceKind::MemoryStore, "memory-1"),
            Some(3),
            10,
            20,
        )
        .unwrap()
    }

    fn reference(workspace: &str, holder: &str) -> ResourceReferenceRecord {
        ResourceReferenceRecord {
            target: ResourceTarget::new(workspace, ResourceKind::File, "hash-1"),
            reference: ResourceReference {
                kind: ResourceReferenceKind::WorkspaceOwnership,
                reference_id: holder.into(),
            },
        }
    }

    async fn repository_spec(store: &(impl ResourcePurgeRepository + ResourceReferenceIndex)) {
        assert_eq!(
            store.put(intent("purge-1")).await.unwrap(),
            PutResourcePurgeOutcome::Inserted
        );
        assert_eq!(
            store.put(intent("purge-1")).await.unwrap(),
            PutResourcePurgeOutcome::Existing
        );
        assert!(store.recoverable(19, 10).await.unwrap().is_empty());
        let mut value = store.recoverable(20, 10).await.unwrap().remove(0);
        let revision = value.revision;
        value.claim("worker", 20, 10).unwrap();
        store.save(revision, value.clone()).await.unwrap();
        assert!(store.recoverable(29, 10).await.unwrap().is_empty());
        assert_eq!(store.recoverable(30, 10).await.unwrap().len(), 1);
        assert!(matches!(
            store.save(revision, value).await,
            Err(ResourcePurgeError::RevisionConflict(_))
        ));

        let a = reference("workspace-a", "ownership-a");
        let b = reference("workspace-b", "ownership-b");
        assert!(store.add_reference(a.clone()).await.unwrap());
        assert!(!store.add_reference(a.clone()).await.unwrap());
        assert!(store.add_reference(b.clone()).await.unwrap());
        assert_eq!(
            store.references(&a.target).await.unwrap(),
            vec![a.reference.clone()]
        );
        assert_eq!(
            store
                .references_for_resource(ResourceKind::File, "hash-1")
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(store.remove_reference(&b).await.unwrap());
        assert!(!store.remove_reference(&b).await.unwrap());
        assert_eq!(
            store
                .references_for_resource(ResourceKind::File, "hash-1")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(matches!(
            store.acquire_reclamation("intent-a", &a.target).await.unwrap(),
            AcquireResourceReclamationOutcome::Blocked(rows) if rows == vec![a.clone()]
        ));
        assert!(store.remove_reference(&a).await.unwrap());
        assert_eq!(
            store
                .acquire_reclamation("intent-a", &a.target)
                .await
                .unwrap(),
            AcquireResourceReclamationOutcome::Acquired
        );
        assert_eq!(
            store
                .acquire_reclamation("intent-a", &a.target)
                .await
                .unwrap(),
            AcquireResourceReclamationOutcome::AlreadyOwned
        );
        assert_eq!(
            store
                .acquire_reclamation("intent-b", &b.target)
                .await
                .unwrap(),
            AcquireResourceReclamationOutcome::Contended
        );
        assert!(matches!(
            store.add_reference(b.clone()).await,
            Err(ResourcePurgeError::ReclamationFenced {
                kind: ResourceKind::File,
                resource_id
            }) if resource_id == "hash-1"
        ));
        assert_eq!(
            store.release_reclamation("intent-b", &b.target).await,
            Err(ResourcePurgeError::StaleReclamationFence)
        );
        assert!(
            store
                .release_reclamation("intent-a", &a.target)
                .await
                .unwrap()
        );
        assert!(
            !store
                .release_reclamation("intent-a", &a.target)
                .await
                .unwrap()
        );
        assert!(store.add_reference(b.clone()).await.unwrap());
        let replacement = ResourceReferenceRecord {
            target: ResourceTarget::new("workspace-c", ResourceKind::File, "hash-2"),
            reference: ResourceReference {
                kind: ResourceReferenceKind::SessionBinding,
                reference_id: "session-1".into(),
            },
        };
        store
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                "session-1",
                vec![replacement.clone()],
            )
            .await
            .unwrap();
        store
            .replace_references(
                ResourceReferenceKind::SessionBinding,
                "session-1",
                Vec::new(),
            )
            .await
            .unwrap();
        assert!(
            store
                .references(&replacement.target)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.get("purge-1").await.unwrap().unwrap().status,
            ResourcePurgeStatus::Claimed
        );
    }

    #[tokio::test]
    async fn in_memory_conforms() {
        repository_spec(&InMemoryResourceStore::new()).await;
    }

    proptest! {
        #[test]
        fn in_memory_reference_and_fence_protocol_matches_the_small_model(
            actions in proptest::collection::vec(0u8..6, 0..128)
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let store = InMemoryResourceStore::new();
                let row = reference("workspace-a", "ownership-a");
                let mut referenced = false;
                let mut fence: Option<&str> = None;
                for action in actions {
                    match action {
                        0 => {
                            let actual = store.add_reference(row.clone()).await;
                            let expected = if fence.is_some() {
                                Err(ResourcePurgeError::ReclamationFenced {
                                    kind: ResourceKind::File,
                                    resource_id: "hash-1".into(),
                                })
                            } else {
                                let inserted = !referenced;
                                referenced = true;
                                Ok(inserted)
                            };
                            prop_assert_eq!(actual, expected);
                        }
                        1 => {
                            let actual = store.remove_reference(&row).await.unwrap();
                            prop_assert_eq!(actual, referenced);
                            referenced = false;
                        }
                        2 | 3 => {
                            let owner = if action == 2 { "intent-a" } else { "intent-b" };
                            let actual = store
                                .acquire_reclamation(owner, &row.target)
                                .await
                                .unwrap();
                            let expected = match fence {
                                Some(current) if current == owner => {
                                    AcquireResourceReclamationOutcome::AlreadyOwned
                                }
                                Some(_) => AcquireResourceReclamationOutcome::Contended,
                                None if referenced => {
                                    AcquireResourceReclamationOutcome::Blocked(vec![row.clone()])
                                }
                                None => {
                                    fence = Some(owner);
                                    AcquireResourceReclamationOutcome::Acquired
                                }
                            };
                            prop_assert_eq!(actual, expected);
                        }
                        4 | 5 => {
                            let owner = if action == 4 { "intent-a" } else { "intent-b" };
                            let actual = store.release_reclamation(owner, &row.target).await;
                            let expected = match fence {
                                Some(current) if current == owner => {
                                    fence = None;
                                    Ok(true)
                                }
                                Some(_) => Err(ResourcePurgeError::StaleReclamationFence),
                                None => Ok(false),
                            };
                            prop_assert_eq!(actual, expected);
                        }
                        _ => unreachable!(),
                    }
                    prop_assert!(!(referenced && fence.is_some()));
                }
                Ok(())
            })?;
        }
    }

    #[test]
    fn legacy_workspace_grant_rows_remain_ownership_edges() {
        assert_eq!(
            parse_reference_kind("workspace_grant").unwrap(),
            ResourceReferenceKind::WorkspaceOwnership
        );
        assert_eq!(
            reference_kind_name(ResourceReferenceKind::WorkspaceOwnership),
            "workspace_ownership"
        );
    }

    #[tokio::test]
    async fn sqlite_is_durable_and_conforms() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resources.db");
        let store = SqliteResourceStore::open(&path).unwrap();
        repository_spec(&store).await;
        drop(store);
        let reopened = SqliteResourceStore::open(path).unwrap();
        assert!(reopened.get("purge-1").await.unwrap().is_some());
        assert_eq!(
            reopened
                .references_for_resource(ResourceKind::File, "hash-1")
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
