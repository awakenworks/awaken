// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Store adapter for durable pre-root creation intents; the claim and the
// @formal reason: root insert exclude each other in one store transaction (ADR-0102 D1).
//! Durable pre-root creation intents over the PostgreSQL Session store. The
//! intent row lock orders a claim against the root insert that consumes it:
//! whichever commits first decides, and the other observes its result.

use awaken_session_contract::{
    PreparedParticipantIntent, PreparedParticipantIntentClaim, PreparedParticipantIntentPhase,
    PreparedParticipantIntentRecord, PreparedParticipantIntents,
};

use super::{
    PostgresManagedSessionRepository, SessionRepositoryConflict, SessionRepositoryError, corrupt,
    storage,
};

const PREPARING: &str = "preparing";
const ABORTING: &str = "aborting";

type IntentRow = (String, String, String, String, i64);
type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

fn decode_intent(row: IntentRow) -> Result<PreparedParticipantIntent, SessionRepositoryError> {
    let (session_id, owner_scope, phase, repository_ids, lease) = row;
    let phase = match phase.as_str() {
        PREPARING => PreparedParticipantIntentPhase::Preparing,
        ABORTING => PreparedParticipantIntentPhase::Aborting,
        other => return Err(corrupt(format!("unknown creation intent phase `{other}`"))),
    };
    Ok(PreparedParticipantIntent::new(
        session_id.into(),
        owner_scope,
        serde_json::from_str(&repository_ids).map_err(corrupt)?,
        phase,
        u64::try_from(lease).map_err(corrupt)?,
    ))
}

fn db_ms(value: u64) -> Result<i64, SessionRepositoryError> {
    i64::try_from(value).map_err(|_| {
        SessionRepositoryError::InvalidMutation("creation intent lease is out of range".into())
    })
}

impl PostgresManagedSessionRepository {
    /// Consume the Session's creation intent inside the root-insert
    /// transaction. `FOR UPDATE` waits for a concurrent claim and then sees
    /// its committed phase.
    pub(super) async fn adopt_creation_intent(
        tx: &mut Tx<'_>,
        session_id: &str,
    ) -> Result<(), SessionRepositoryError> {
        let phase = sqlx::query_scalar::<_, String>(
            "SELECT phase FROM managed_session_creation_intent WHERE session_id = $1 FOR UPDATE",
        )
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage)?;
        if phase.as_deref() == Some(ABORTING) {
            return Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::Tombstoned,
            ));
        }
        sqlx::query("DELETE FROM managed_session_creation_intent WHERE session_id = $1")
            .bind(session_id)
            .execute(&mut **tx)
            .await
            .map_err(storage)?;
        Ok(())
    }

    async fn locked_intent(
        tx: &mut Tx<'_>,
        session_id: &str,
    ) -> Result<Option<PreparedParticipantIntent>, SessionRepositoryError> {
        sqlx::query_as::<_, IntentRow>(
            "SELECT session_id, scope_id, phase, repository_ids_json, lease_expires_at_unix_ms \
             FROM managed_session_creation_intent WHERE session_id = $1 FOR UPDATE",
        )
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage)?
        .map(decode_intent)
        .transpose()
    }

    async fn root_exists(
        tx: &mut Tx<'_>,
        session_id: &str,
    ) -> Result<bool, SessionRepositoryError> {
        Ok(
            sqlx::query_scalar::<_, i32>("SELECT 1 FROM managed_session WHERE session_id = $1")
                .bind(session_id)
                .fetch_optional(&mut **tx)
                .await
                .map_err(storage)?
                .is_some(),
        )
    }
}

#[async_trait::async_trait]
impl PreparedParticipantIntents for PostgresManagedSessionRepository {
    async fn record_creation_intent(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        repository_id: &str,
        lease_expires_at_unix_ms: u64,
    ) -> Result<PreparedParticipantIntentRecord, SessionRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // Serialize first prepares of one identity before reading the row.
        sqlx::query(
            "INSERT INTO managed_session_creation_intent \
                (session_id, scope_id, phase, repository_ids_json, lease_expires_at_unix_ms) \
             VALUES ($1, $2, $3, '[]', $4) ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id.as_str())
        .bind(owner_scope)
        .bind(PREPARING)
        .bind(db_ms(lease_expires_at_unix_ms)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let intent = Self::locked_intent(&mut tx, session_id.as_str())
            .await?
            .ok_or_else(|| storage("creation intent vanished under its row lock"))?;
        if Self::root_exists(&mut tx, session_id.as_str()).await? {
            // Dropping the transaction rolls back the placeholder row.
            return Ok(PreparedParticipantIntentRecord::RootExists);
        }
        if intent.phase() == PreparedParticipantIntentPhase::Aborting {
            return Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::Tombstoned,
            ));
        }
        if intent.owner_scope() != owner_scope {
            return Err(SessionRepositoryError::InvalidMutation(
                "creation intent belongs to another owner scope".into(),
            ));
        }
        let mut repository_ids = intent.into_repository_ids();
        if !repository_ids.iter().any(|id| id == repository_id) {
            repository_ids.push(repository_id.to_owned());
        }
        sqlx::query(
            "UPDATE managed_session_creation_intent SET repository_ids_json = $2, \
                lease_expires_at_unix_ms = GREATEST(lease_expires_at_unix_ms, $3) \
             WHERE session_id = $1",
        )
        .bind(session_id.as_str())
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
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let Some(intent) = Self::locked_intent(&mut tx, session_id.as_str()).await? else {
            return Ok(PreparedParticipantIntentClaim::Absent);
        };
        if Self::root_exists(&mut tx, session_id.as_str()).await? {
            // A prepare raced a committed root: the root's references decide.
            sqlx::query("DELETE FROM managed_session_creation_intent WHERE session_id = $1")
                .bind(session_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            tx.commit().await.map_err(storage)?;
            return Ok(PreparedParticipantIntentClaim::Absent);
        }
        sqlx::query("UPDATE managed_session_creation_intent SET phase = $2 WHERE session_id = $1")
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
            "DELETE FROM managed_session_creation_intent WHERE session_id = $1 AND phase = $2",
        )
        .bind(session_id.as_str())
        .bind(ABORTING)
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
             WHERE phase = $1 OR lease_expires_at_unix_ms <= $2 \
             ORDER BY lease_expires_at_unix_ms, session_id LIMIT $3",
        )
        .bind(ABORTING)
        .bind(db_ms(now_unix_ms)?)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(decode_intent)
        .collect()
    }
}
