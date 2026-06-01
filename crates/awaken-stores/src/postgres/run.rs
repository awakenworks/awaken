use async_trait::async_trait;
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::storage::{
    CheckpointSnapshot, RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore,
    checkpoint_parent_thread_id, message_append,
};
use awaken_server_contract::thread::Thread;
use serde::de::DeserializeOwned;
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::PostgresStore;

const RUN_COLUMNS: &str = concat!(
    "run_id, thread_id, agent_id, parent_run_id, resolution_id, activation, request, ",
    "run_input, run_output, status, termination_reason, final_output, error_payload, ",
    "dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, ",
    "started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state"
);

fn optional_json<T: serde::Serialize>(
    field: &str,
    value: Option<&T>,
) -> Result<Option<serde_json::Value>, StorageError> {
    value
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| StorageError::Serialization(format!("serialize run {field}: {error}")))
}

// ── RunStore ────────────────────────────────────────────────────────

#[async_trait]
impl RunStore for PostgresStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        record.validate_for_persist()?;
        self.ensure_schema().await?;
        let state_json = optional_json("state", record.state.as_ref())?;
        let termination_reason_json =
            optional_json("termination_reason", record.termination_reason.as_ref())?;
        let activation_json = optional_json("activation", record.activation.as_ref())?;
        let request_json = optional_json("request", record.request.as_ref())?;
        let input_json = optional_json("input", record.input.as_ref())?;
        let output_json = optional_json("output", record.output.as_ref())?;
        let waiting_json = optional_json("waiting", record.waiting.as_ref())?;
        let outcome_json = optional_json("outcome", record.outcome.as_ref())?;
        let resolution_id_json = optional_json("resolution_id", record.resolution_id.as_ref())?;
        let sql = format!(
            "INSERT INTO {} (run_id, thread_id, agent_id, parent_run_id, resolution_id, activation, request, run_input, run_output, status, termination_reason, final_output, error_payload, dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26)",
            self.runs_table
        );
        sqlx::query(&sql)
            .bind(&record.run_id)
            .bind(&record.thread_id)
            .bind(&record.agent_id)
            .bind(&record.parent_run_id)
            .bind(&resolution_id_json)
            .bind(&activation_json)
            .bind(&request_json)
            .bind(&input_json)
            .bind(&output_json)
            .bind(format!("{:?}", record.status).to_lowercase())
            .bind(&termination_reason_json)
            .bind(&record.final_output)
            .bind(&record.error_payload)
            .bind(&record.dispatch_id)
            .bind(&record.session_id)
            .bind(&record.transport_request_id)
            .bind(&waiting_json)
            .bind(&outcome_json)
            .bind(record.created_at as i64)
            .bind(record.started_at.map(|value| value as i64))
            .bind(record.finished_at.map(|value| value as i64))
            .bind(record.updated_at as i64)
            .bind(record.steps as i32)
            .bind(record.input_tokens as i64)
            .bind(record.output_tokens as i64)
            .bind(&state_json)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                if e.to_string().contains("duplicate key")
                    || e.to_string().contains("unique constraint")
                {
                    StorageError::AlreadyExists(record.run_id.clone())
                } else {
                    StorageError::Io(e.to_string())
                }
            })?;
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM {} WHERE run_id = $1",
            self.runs_table
        );
        let row = sqlx::query(&sql)
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        row.map(try_run_record_from_pg_row).transpose()
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        self.ensure_schema().await?;
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM {} WHERE thread_id = $1 ORDER BY updated_at DESC LIMIT 1",
            self.runs_table
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        row.map(try_run_record_from_pg_row).transpose()
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        self.ensure_schema().await?;

        // Build WHERE conditions with positional binds in a stable order:
        // thread_id, status, id_prefix. The scope prefix is pushed down as a
        // `LIKE 'prefix%'` filter so a scoped listing never returns other scopes.
        let mut conditions = Vec::new();
        let mut idx = 1;
        if query.thread_id.is_some() {
            conditions.push(format!("thread_id = ${idx}"));
            idx += 1;
        }
        if query.status.is_some() {
            conditions.push(format!("status = ${idx}"));
            idx += 1;
        }
        if query.id_prefix.is_some() {
            conditions.push(format!("thread_id LIKE ${idx} ESCAPE '\\'"));
            idx += 1;
        }
        let _ = idx;

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) FROM {}{}", self.runs_table, where_clause);
        let list_sql = format!(
            "SELECT {RUN_COLUMNS} FROM {}{} ORDER BY created_at ASC LIMIT {} OFFSET {}",
            self.runs_table,
            where_clause,
            query.limit.clamp(1, 200),
            query.offset
        );

        let like_pattern = query.id_prefix.as_deref().map(like_prefix_pattern);

        // This is simplified — in production you'd use a proper query builder.
        // For the feature-gated postgres backend, we use raw string queries.
        let (total,): (i64,) = {
            let mut q = sqlx::query_as(&count_sql);
            if let Some(ref tid) = query.thread_id {
                q = q.bind(tid);
            }
            if let Some(status) = query.status {
                q = q.bind(format!("{status:?}").to_lowercase());
            }
            if let Some(ref pattern) = like_pattern {
                q = q.bind(pattern);
            }
            q.fetch_one(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?
        };

        let rows = {
            let mut q = sqlx::query(&list_sql);
            if let Some(ref tid) = query.thread_id {
                q = q.bind(tid);
            }
            if let Some(status) = query.status {
                q = q.bind(format!("{status:?}").to_lowercase());
            }
            if let Some(ref pattern) = like_pattern {
                q = q.bind(pattern);
            }
            q.fetch_all(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?
        };

        let items: Vec<RunRecord> = rows
            .into_iter()
            .map(try_run_record_from_pg_row)
            .collect::<Result<_, _>>()?;

        let has_more = (query.offset + items.len()) < total as usize;
        Ok(RunPage {
            items,
            total: total as usize,
            has_more,
        })
    }
}

// ── ThreadRunStore ──────────────────────────────────────────────────

impl PostgresStore {
    pub(super) async fn lock_thread_messages_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        thread_id: &str,
    ) -> Result<(), StorageError> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1), 0)")
            .bind(thread_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    /// Version-guarded committed append within an open transaction. Locks the
    /// thread row `FOR UPDATE` so concurrent appends serialize across
    /// connections/instances (ADR-0042 D5), reads the current committed
    /// messages, rejects a stale `expected_version`, then delegates the merged
    /// write + run upsert to [`Self::checkpoint_in_tx`]. Returns the new
    /// committed message count.
    pub(crate) async fn checkpoint_append_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        thread_id: &str,
        messages: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<u64, StorageError> {
        // Acquire a per-thread transaction lock before any row exists, then
        // lock the thread row when present. This keeps new-thread appends
        // serial across connections as well as existing-thread appends.
        self.lock_thread_messages_tx(tx, thread_id).await?;
        let _ = self.load_thread_tx(tx, thread_id, "FOR UPDATE").await?;
        let existing_records = self
            .load_committed_message_records_tx(tx, thread_id)
            .await?;
        let existing = existing_records
            .iter()
            .map(|record| record.message.clone())
            .collect::<Vec<_>>();
        let actual = existing.len() as u64;
        if let Some(expected) = expected_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }
        let mut merged = existing.clone();
        message_append::merge_checkpoint_append_messages(&mut merged, messages)?;
        let existing_by_id = existing_records
            .iter()
            .filter_map(|record| {
                record
                    .message
                    .id
                    .as_ref()
                    .map(|id| (id.clone(), record.seq))
            })
            .collect::<std::collections::HashMap<_, _>>();
        let mut next_seq = actual + 1;
        for message in messages {
            if message
                .id
                .as_ref()
                .and_then(|id| existing_by_id.get(id))
                .is_some()
            {
                continue;
            } else {
                self.insert_committed_message_tx(tx, thread_id, next_seq, message)
                    .await?;
                next_seq += 1;
            }
        }
        let new_version = merged.len() as u64;
        self.upsert_thread_and_run_in_tx(tx, thread_id, run).await?;
        Ok(new_version)
    }

    pub(super) async fn upsert_thread_and_run_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        thread_id: &str,
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_millis() as u64;
        let mut thread = self
            .load_thread_tx(tx, thread_id, "")
            .await?
            .unwrap_or_else(|| Thread::with_id(thread_id));
        self.validate_thread_hierarchy_tx(
            tx,
            thread_id,
            checkpoint_parent_thread_id(Some(&thread), run),
        )
        .await?;
        thread.touch(now);
        thread.apply_run_projection(run);
        thread.normalize_lineage();
        self.save_thread_tx(tx, &thread).await?;
        self.upsert_run_in_tx(tx, run).await
    }

    async fn upsert_run_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        run.validate_for_persist()?;
        // Upsert run record
        let state_json = optional_json("state", run.state.as_ref())?;
        let termination_reason_json =
            optional_json("termination_reason", run.termination_reason.as_ref())?;
        let activation_json = optional_json("activation", run.activation.as_ref())?;
        let request_json = optional_json("request", run.request.as_ref())?;
        let input_json = optional_json("input", run.input.as_ref())?;
        let output_json = optional_json("output", run.output.as_ref())?;
        let waiting_json = optional_json("waiting", run.waiting.as_ref())?;
        let outcome_json = optional_json("outcome", run.outcome.as_ref())?;
        let resolution_id_json = optional_json("resolution_id", run.resolution_id.as_ref())?;
        let run_sql = format!(
            "INSERT INTO {} (run_id, thread_id, agent_id, parent_run_id, resolution_id, activation, request, run_input, run_output, status, termination_reason, final_output, error_payload, dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26)
             ON CONFLICT (run_id) DO UPDATE SET
                resolution_id = $5, activation = $6, request = $7, run_input = $8, run_output = $9,
                status = $10, termination_reason = $11, final_output = $12,
                error_payload = $13, dispatch_id = $14, session_id = $15,
                transport_request_id = $16, waiting = $17, outcome = $18,
                started_at = $20, finished_at = $21, updated_at = $22,
                steps = $23, input_tokens = $24, output_tokens = $25, state = $26",
            self.runs_table
        );
        sqlx::query(&run_sql)
            .bind(&run.run_id)
            .bind(&run.thread_id)
            .bind(&run.agent_id)
            .bind(&run.parent_run_id)
            .bind(&resolution_id_json)
            .bind(&activation_json)
            .bind(&request_json)
            .bind(&input_json)
            .bind(&output_json)
            .bind(format!("{:?}", run.status).to_lowercase())
            .bind(&termination_reason_json)
            .bind(&run.final_output)
            .bind(&run.error_payload)
            .bind(&run.dispatch_id)
            .bind(&run.session_id)
            .bind(&run.transport_request_id)
            .bind(&waiting_json)
            .bind(&outcome_json)
            .bind(run.created_at as i64)
            .bind(run.started_at.map(|value| value as i64))
            .bind(run.finished_at.map(|value| value as i64))
            .bind(run.updated_at as i64)
            .bind(run.steps as i32)
            .bind(run.input_tokens as i64)
            .bind(run.output_tokens as i64)
            .bind(&state_json)
            .execute(&mut **tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        Ok(())
    }

    pub(crate) async fn checkpoint_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        self.lock_thread_messages_tx(tx, thread_id).await?;
        self.replace_committed_messages_tx(tx, thread_id, messages)
            .await?;
        self.upsert_thread_and_run_in_tx(tx, thread_id, run).await
    }
}

#[async_trait]
impl ThreadRunStore for PostgresStore {
    fn thread_run_storage_identity(&self) -> Option<String> {
        Some(self.thread_run_storage_identity_descriptor())
    }

    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        self.checkpoint_in_tx(&mut tx, thread_id, messages, run)
            .await?;
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(())
    }

    async fn checkpoint_append(
        &self,
        thread_id: &str,
        messages: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<u64, StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let new_version = self
            .checkpoint_append_in_tx(&mut tx, thread_id, messages, expected_version, run)
            .await?;
        tx.commit()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(new_version)
    }

    /// Consistent resume read in one transaction (ADR-0038 C5): committed
    /// messages, latest run, and thread state are read together so a
    /// concurrent commit cannot tear the snapshot.
    async fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Result<Option<CheckpointSnapshot>, StorageError> {
        self.ensure_schema().await?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?;

        let records = self
            .load_committed_message_records_tx(&mut tx, thread_id)
            .await?;

        let latest_run_sql = format!(
            "SELECT {RUN_COLUMNS} FROM {} WHERE thread_id = $1 ORDER BY updated_at DESC LIMIT 1",
            self.runs_table
        );
        let latest_run = sqlx::query(&latest_run_sql)
            .bind(thread_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?
            .map(try_run_record_from_pg_row)
            .transpose()?;

        if records.is_empty() && latest_run.is_none() {
            return Ok(None);
        }

        let message_version = records.len() as u64;
        // Records carry compaction marks (projected from message metadata), so
        // fold via the effective view before applying the unpaired filter.
        let messages = awaken_server_contract::contract::message::effective_committed_view(
            records.into_iter().map(|record| record.message).collect(),
            thread_id,
        );

        let thread_state_sql = format!(
            "SELECT data FROM {} WHERE thread_id = $1",
            self.thread_states_table()
        );
        let thread_state = sqlx::query_as::<_, (serde_json::Value,)>(&thread_state_sql)
            .bind(thread_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| StorageError::Io(e.to_string()))?
            .map(|(data,)| serde_json::from_value(data))
            .transpose()
            .map_err(|e| StorageError::Serialization(e.to_string()))?;

        // Read-only transaction; rollback (drop) is fine — nothing was written.
        Ok(Some(CheckpointSnapshot {
            messages,
            message_version,
            latest_run,
            thread_state,
        }))
    }
}

/// Build a SQL `LIKE` pattern matching ids that start with `prefix`. The LIKE
/// wildcards `%`, `_` and the escape char `\` are escaped so a scope id with
/// those characters cannot widen the match (used with `ESCAPE '\'`).
pub(super) fn like_prefix_pattern(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len() + 1);
    for ch in prefix.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    pattern
}

fn optional_row_json<T: DeserializeOwned>(
    field: &str,
    value: Option<serde_json::Value>,
) -> Result<Option<T>, StorageError> {
    value
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| StorageError::Serialization(format!("decode run {field}: {error}")))
}

fn try_run_record_from_pg_row(row: PgRow) -> Result<RunRecord, StorageError> {
    let status: String = row.get("status");
    let state: Option<serde_json::Value> = row.get("state");
    let resolution_id: Option<serde_json::Value> = row.get("resolution_id");
    let activation: Option<serde_json::Value> = row.get("activation");
    let request: Option<serde_json::Value> = row.get("request");
    let input: Option<serde_json::Value> = row.get("run_input");
    let output: Option<serde_json::Value> = row.get("run_output");
    let termination_reason: Option<serde_json::Value> = row.get("termination_reason");
    let waiting: Option<serde_json::Value> = row.get("waiting");
    let outcome: Option<serde_json::Value> = row.get("outcome");
    let created_at: i64 = row.get("created_at");
    let started_at: Option<i64> = row.get("started_at");
    let finished_at: Option<i64> = row.get("finished_at");
    let updated_at: i64 = row.get("updated_at");
    let steps: i32 = row.get("steps");
    let input_tokens: i64 = row.get("input_tokens");
    let output_tokens: i64 = row.get("output_tokens");

    let record = RunRecord {
        run_id: row.get("run_id"),
        thread_id: row.get("thread_id"),
        agent_id: row.get("agent_id"),
        parent_run_id: row.get("parent_run_id"),
        resolution_id: optional_row_json("resolution_id", resolution_id)?,
        activation: optional_row_json("activation", activation)?,
        request: optional_row_json("request", request)?,
        input: optional_row_json("run_input", input)?,
        output: optional_row_json("run_output", output)?,
        status: parse_run_status(&status)?,
        termination_reason: optional_row_json("termination_reason", termination_reason)?,
        final_output: row.get("final_output"),
        error_payload: row.get("error_payload"),
        dispatch_id: row.get("dispatch_id"),
        session_id: row.get("session_id"),
        transport_request_id: row.get("transport_request_id"),
        waiting: optional_row_json("waiting", waiting)?,
        outcome: optional_row_json("outcome", outcome)?,
        created_at: created_at as u64,
        started_at: started_at.map(|value| value as u64),
        finished_at: finished_at.map(|value| value as u64),
        updated_at: updated_at as u64,
        steps: steps as usize,
        input_tokens: input_tokens as u64,
        output_tokens: output_tokens as u64,
        state: optional_row_json("state", state)?,
    };
    validate_decoded_run_record(record)
}

fn validate_decoded_run_record(record: RunRecord) -> Result<RunRecord, StorageError> {
    record.validate_for_persist()?;
    Ok(record)
}

pub(super) fn parse_run_status(
    s: &str,
) -> Result<awaken_server_contract::contract::lifecycle::RunStatus, StorageError> {
    use awaken_server_contract::contract::lifecycle::RunStatus;
    match s {
        "created" => Ok(RunStatus::Created),
        "running" => Ok(RunStatus::Running),
        "waiting" => Ok(RunStatus::Waiting),
        "done" => Ok(RunStatus::Done),
        other => Err(StorageError::Validation(format!(
            "unknown run status '{other}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::ser::Error as _;

    struct FailingSerialize;

    impl serde::Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(S::Error::custom("injected serialization failure"))
        }
    }

    #[test]
    fn optional_json_propagates_serialization_errors() {
        let error = optional_json("state", Some(&FailingSerialize)).unwrap_err();

        assert!(
            matches!(error, StorageError::Serialization(ref message)
                if message.contains("serialize run state")
                    && message.contains("injected serialization failure")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn decoded_run_record_validation_rejects_corrupt_activation() {
        let record = RunRecord {
            run_id: "run-1".to_string(),
            thread_id: "thread-1".to_string(),
            agent_id: "agent-1".to_string(),
            status: awaken_server_contract::contract::lifecycle::RunStatus::Running,
            created_at: 1,
            updated_at: 1,
            activation: Some(
                awaken_server_contract::contract::run::RunActivationSnapshot {
                    intent: awaken_server_contract::contract::run::RunIntent::new("other-thread"),
                    input: awaken_server_contract::contract::run::RunInputSnapshot {
                        thread_id: "other-thread".to_string(),
                        ..Default::default()
                    },
                    options: awaken_server_contract::contract::run::RunOptions::default(),
                    trace: awaken_server_contract::contract::run::RunTraceContext::default(),
                    seeded_decisions: Vec::new(),
                    resolution_id: None,
                },
            ),
            ..Default::default()
        };

        let error = validate_decoded_run_record(record).unwrap_err();

        assert!(
            matches!(error, StorageError::Validation(ref message)
                if message.contains("activation thread_id")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn optional_row_json_propagates_decode_errors() {
        let error: StorageError =
            optional_row_json::<String>("resolution_id", Some(serde_json::json!({}))).unwrap_err();

        assert!(
            matches!(error, StorageError::Serialization(ref message)
                if message.contains("decode run resolution_id")),
            "unexpected error: {error}"
        );
    }
}
