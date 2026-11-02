// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Store adapter for durable pre-root creation intents; the claim and the
// @formal reason: root insert exclude each other in one store transaction (ADR-0102 D1).
//! Durable pre-root creation intents over the canonical SQLite Session store.
//! Every write is a `BEGIN IMMEDIATE` transaction, so a claim, a prepare and
//! the root insert that consumes the intent are serialized by the one writer.

use awaken_session_contract::{
    PreparedParticipantIntent, PreparedParticipantIntentClaim, PreparedParticipantIntentPhase,
    PreparedParticipantIntentRecord, PreparedParticipantIntents,
};

use super::{
    SessionRepositoryConflict, SessionRepositoryError, SqliteManagedSessionRepository, corrupt,
    storage,
};

use crate::creation_intents::{
    ABORTED, ABORTING, IntentRow, PREPARING, db_ms, decode_intent, decode_phase,
};

impl SqliteManagedSessionRepository {
    /// Consume the Session's creation intent inside the root-insert
    /// transaction. An `Aborting` intent means the identity is being retired.
    pub(super) async fn adopt_creation_intent(
        tx: &mut sqlx::SqliteConnection,
        session_id: &str,
    ) -> Result<(), SessionRepositoryError> {
        let phase = sqlx::query_scalar::<_, String>(
            "SELECT phase FROM managed_session_creation_intent WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        if let Some(phase) = phase.as_deref()
            && !decode_phase(phase)?.permits_adoption()
        {
            return Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::Tombstoned,
            ));
        }
        sqlx::query("DELETE FROM managed_session_creation_intent WHERE session_id = ?1")
            .bind(session_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        Ok(())
    }

    async fn intent_row(
        tx: &mut sqlx::SqliteConnection,
        session_id: &str,
    ) -> Result<Option<PreparedParticipantIntent>, SessionRepositoryError> {
        sqlx::query_as::<_, IntentRow>(
            "SELECT session_id, scope_id, phase, repository_ids_json, lease_expires_at_unix_ms \
             FROM managed_session_creation_intent WHERE session_id = ?1",
        )
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(decode_intent)
        .transpose()
    }

    async fn root_exists(
        tx: &mut sqlx::SqliteConnection,
        session_id: &str,
    ) -> Result<bool, SessionRepositoryError> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT 1 FROM managed_session WHERE session_id = ?1")
                .bind(session_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?
                .is_some(),
        )
    }
}

#[async_trait::async_trait]
impl PreparedParticipantIntents for SqliteManagedSessionRepository {
    async fn record_creation_intent(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        repository_id: &str,
        lease_expires_at_unix_ms: u64,
    ) -> Result<PreparedParticipantIntentRecord, SessionRepositoryError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        if Self::root_exists(&mut tx, session_id.as_str()).await? {
            return Ok(PreparedParticipantIntentRecord::RootExists);
        }
        let mut repository_ids = match Self::intent_row(&mut tx, session_id.as_str()).await? {
            Some(intent) if !intent.phase().permits_adoption() => {
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::Tombstoned,
                ));
            }
            Some(intent) if intent.owner_scope() != owner_scope => {
                return Err(SessionRepositoryError::InvalidMutation(
                    "creation intent belongs to another owner scope".into(),
                ));
            }
            Some(intent) => intent.into_repository_ids(),
            None => Vec::new(),
        };
        if !repository_ids.iter().any(|id| id == repository_id) {
            repository_ids.push(repository_id.to_owned());
        }
        sqlx::query(
            "INSERT INTO managed_session_creation_intent \
                (session_id, scope_id, phase, repository_ids_json, lease_expires_at_unix_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (session_id) DO UPDATE SET \
                repository_ids_json = excluded.repository_ids_json, \
                lease_expires_at_unix_ms = \
                    max(lease_expires_at_unix_ms, excluded.lease_expires_at_unix_ms)",
        )
        .bind(session_id.as_str())
        .bind(owner_scope)
        .bind(PREPARING)
        .bind(serde_json::to_string(&repository_ids).map_err(corrupt)?)
        .bind(db_ms(lease_expires_at_unix_ms)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(PreparedParticipantIntentRecord::Recorded)
    }

    async fn claim_creation_abort(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PreparedParticipantIntentClaim, SessionRepositoryError> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let Some(intent) = Self::intent_row(&mut tx, session_id.as_str()).await? else {
            return Ok(PreparedParticipantIntentClaim::Absent);
        };
        if Self::root_exists(&mut tx, session_id.as_str()).await? {
            // A prepare raced a committed root: the root's references decide.
            sqlx::query("DELETE FROM managed_session_creation_intent WHERE session_id = ?1")
                .bind(session_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            tx.commit().await.map_err(storage)?;
            return Ok(PreparedParticipantIntentClaim::Absent);
        }
        if intent.phase() == PreparedParticipantIntentPhase::Aborted {
            return Ok(PreparedParticipantIntentClaim::Claimed(intent));
        }
        sqlx::query("UPDATE managed_session_creation_intent SET phase = ?2 WHERE session_id = ?1")
            .bind(session_id.as_str())
            .bind(ABORTING)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(PreparedParticipantIntentClaim::Claimed(
            intent.begin_abort(),
        ))
    }

    async fn complete_creation_abort(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(), SessionRepositoryError> {
        sqlx::query(
            "UPDATE managed_session_creation_intent SET phase = ?3 WHERE session_id = ?1 AND phase = ?2",
        )
        .bind(session_id.as_str())
        .bind(ABORTING)
        .bind(ABORTED)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn recoverable_creation_intents(
        &self,
        now_unix_ms: u64,
        limit: usize,
    ) -> Result<Vec<PreparedParticipantIntent>, SessionRepositoryError> {
        sqlx::query_as::<_, IntentRow>(
            "SELECT session_id, scope_id, phase, repository_ids_json, lease_expires_at_unix_ms \
             FROM managed_session_creation_intent \
             WHERE phase = ?1 OR (phase = ?4 AND lease_expires_at_unix_ms <= ?2) \
             ORDER BY lease_expires_at_unix_ms, session_id LIMIT ?3",
        )
        .bind(ABORTING)
        .bind(db_ms(now_unix_ms)?)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .bind(PREPARING)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(decode_intent)
        .collect()
    }
}
