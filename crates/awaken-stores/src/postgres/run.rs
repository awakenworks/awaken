use async_trait::async_trait;
use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore,
    checkpoint_parent_thread_id, message_append,
};
use awaken_server_contract::thread::Thread;
use sqlx::Row;
use sqlx::postgres::PgRow;

use super::PostgresStore;

const RUN_COLUMNS: &str = concat!(
    "run_id, thread_id, agent_id, parent_run_id, registry_manifest, activation, request, ",
    "run_input, run_output, status, termination_reason, final_output, error_payload, ",
    "dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, ",
    "started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state"
);

// ── RunStore ────────────────────────────────────────────────────────

#[async_trait]
impl RunStore for PostgresStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        self.ensure_schema().await?;
        let state_json = record
            .state
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok());
        let termination_reason_json = record
            .termination_reason
            .as_ref()
            .and_then(|reason| serde_json::to_value(reason).ok());
        let activation_json = record
            .activation
            .as_ref()
            .and_then(|activation| serde_json::to_value(activation).ok());
        let request_json = record
            .request
            .as_ref()
            .and_then(|request| serde_json::to_value(request).ok());
        let input_json = record
            .input
            .as_ref()
            .and_then(|input| serde_json::to_value(input).ok());
        let output_json = record
            .output
            .as_ref()
            .and_then(|output| serde_json::to_value(output).ok());
        let waiting_json = record
            .waiting
            .as_ref()
            .and_then(|waiting| serde_json::to_value(waiting).ok());
        let outcome_json = record
            .outcome
            .as_ref()
            .and_then(|outcome| serde_json::to_value(outcome).ok());
        let registry_manifest_json = record
            .registry_manifest
            .as_ref()
            .and_then(|manifest| serde_json::to_value(manifest).ok());
        let sql = format!(
            "INSERT INTO {} (run_id, thread_id, agent_id, parent_run_id, registry_manifest, activation, request, run_input, run_output, status, termination_reason, final_output, error_payload, dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26)",
            self.runs_table
        );
        sqlx::query(&sql)
            .bind(&record.run_id)
            .bind(&record.thread_id)
            .bind(&record.agent_id)
            .bind(&record.parent_run_id)
            .bind(&registry_manifest_json)
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

        Ok(row.map(run_record_from_pg_row))
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

        Ok(row.map(run_record_from_pg_row))
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        self.ensure_schema().await?;

        // Build count query
        let mut conditions = Vec::new();
        if query.thread_id.is_some() {
            conditions.push("thread_id = $1".to_string());
        }
        if query.status.is_some() {
            let idx = if query.thread_id.is_some() { 2 } else { 1 };
            conditions.push(format!("status = ${idx}"));
        }

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
            q.fetch_all(&self.pool)
                .await
                .map_err(|e| StorageError::Io(e.to_string()))?
        };

        let items: Vec<RunRecord> = rows.into_iter().map(run_record_from_pg_row).collect();

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
        // Upsert run record
        let state_json = run
            .state
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok());
        let termination_reason_json = run
            .termination_reason
            .as_ref()
            .and_then(|reason| serde_json::to_value(reason).ok());
        let activation_json = run
            .activation
            .as_ref()
            .and_then(|activation| serde_json::to_value(activation).ok());
        let request_json = run
            .request
            .as_ref()
            .and_then(|request| serde_json::to_value(request).ok());
        let input_json = run
            .input
            .as_ref()
            .and_then(|input| serde_json::to_value(input).ok());
        let output_json = run
            .output
            .as_ref()
            .and_then(|output| serde_json::to_value(output).ok());
        let waiting_json = run
            .waiting
            .as_ref()
            .and_then(|waiting| serde_json::to_value(waiting).ok());
        let outcome_json = run
            .outcome
            .as_ref()
            .and_then(|outcome| serde_json::to_value(outcome).ok());
        let registry_manifest_json = run
            .registry_manifest
            .as_ref()
            .and_then(|manifest| serde_json::to_value(manifest).ok());
        let run_sql = format!(
            "INSERT INTO {} (run_id, thread_id, agent_id, parent_run_id, registry_manifest, activation, request, run_input, run_output, status, termination_reason, final_output, error_payload, dispatch_id, session_id, transport_request_id, waiting, outcome, created_at, started_at, finished_at, updated_at, steps, input_tokens, output_tokens, state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26)
             ON CONFLICT (run_id) DO UPDATE SET
                registry_manifest = $5, activation = $6, request = $7, run_input = $8, run_output = $9,
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
            .bind(&registry_manifest_json)
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
}

fn run_record_from_pg_row(row: PgRow) -> RunRecord {
    let status: String = row.get("status");
    let state: Option<serde_json::Value> = row.get("state");
    let registry_manifest: Option<serde_json::Value> = row.get("registry_manifest");
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

    RunRecord {
        run_id: row.get("run_id"),
        thread_id: row.get("thread_id"),
        agent_id: row.get("agent_id"),
        parent_run_id: row.get("parent_run_id"),
        registry_manifest: registry_manifest.and_then(|value| serde_json::from_value(value).ok()),
        activation: activation.and_then(|value| serde_json::from_value(value).ok()),
        request: request.and_then(|value| serde_json::from_value(value).ok()),
        input: input.and_then(|value| serde_json::from_value(value).ok()),
        output: output.and_then(|value| serde_json::from_value(value).ok()),
        status: parse_run_status(&status),
        termination_reason: termination_reason.and_then(|value| serde_json::from_value(value).ok()),
        final_output: row.get("final_output"),
        error_payload: row.get("error_payload"),
        dispatch_id: row.get("dispatch_id"),
        session_id: row.get("session_id"),
        transport_request_id: row.get("transport_request_id"),
        waiting: waiting.and_then(|value| serde_json::from_value(value).ok()),
        outcome: outcome.and_then(|value| serde_json::from_value(value).ok()),
        created_at: created_at as u64,
        started_at: started_at.map(|value| value as u64),
        finished_at: finished_at.map(|value| value as u64),
        updated_at: updated_at as u64,
        steps: steps as usize,
        input_tokens: input_tokens as u64,
        output_tokens: output_tokens as u64,
        state: state.and_then(|value| serde_json::from_value(value).ok()),
    }
}

pub(super) fn parse_run_status(s: &str) -> awaken_server_contract::contract::lifecycle::RunStatus {
    use awaken_server_contract::contract::lifecycle::RunStatus;
    match s {
        "created" => RunStatus::Created,
        "running" => RunStatus::Running,
        "waiting" => RunStatus::Waiting,
        "done" => RunStatus::Done,
        _ => RunStatus::Running,
    }
}
