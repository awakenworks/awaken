// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Archives activation within the existing commit transaction and reads ordered PostgreSQL history.
use awaken_agent_contract::thread::read::replay::CommittedThreadPrefix;
use sqlx::PgConnection;

use super::{
    AuditKind, Error, EventRecord, Json, Message, RunId, RunState, StateCommand, ThreadCommit,
    ThreadId, decode_authority, encode_authority, encode_run_lifecycle_cursor, reject,
};

/// The Coordinator supplies this only from a locked, validated claim.
pub struct RunArchive {
    pub scope_id: String,
    pub session_id: String,
    pub data: serde_json::Value,
}

pub(super) async fn insert_archive(
    tx: &mut PgConnection,
    commit: &ThreadCommit,
    sequence: u64,
    archive: RunArchive,
) -> Result<(), Error> {
    sqlx::query("INSERT INTO runtime_run_activation (scope_id, session_id, thread_id, run_id, first_commit_sequence, data) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(archive.scope_id).bind(archive.session_id)
        .bind(&commit.thread_id.0).bind(&commit.run_id().0)
        .bind(encode_authority(sequence)?).bind(Json(archive.data))
        .execute(tx).await.map_err(reject)?;
    Ok(())
}

pub async fn replay_head(tx: &mut PgConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COALESCE(MAX(sequence), 0) FROM runtime_commit")
        .fetch_one(tx)
        .await
}

pub async fn read_run_archives(
    tx: &mut PgConnection,
    scope: &str,
    session: &str,
    through: i64,
) -> Result<Vec<(String, String, i64, Json<serde_json::Value>)>, sqlx::Error> {
    sqlx::query_as("SELECT thread_id, run_id, first_commit_sequence, data FROM runtime_run_activation WHERE scope_id = $1 AND session_id = $2 AND first_commit_sequence <= $3 ORDER BY first_commit_sequence")
        .bind(scope).bind(session).bind(through).fetch_all(tx).await
}

pub async fn replay_thread_prefix(
    tx: &mut PgConnection,
    thread: &ThreadId,
    through: i64,
) -> Result<CommittedThreadPrefix, Error> {
    replay_prefix(tx, thread, None, through).await
}

pub async fn replay_run_prefix(
    tx: &mut PgConnection,
    thread: &ThreadId,
    run: &RunId,
    through: i64,
) -> Result<CommittedThreadPrefix, Error> {
    replay_prefix(tx, thread, Some(run), through).await
}

async fn replay_prefix(
    tx: &mut PgConnection,
    thread: &ThreadId,
    run: Option<&RunId>,
    through: i64,
) -> Result<CommittedThreadPrefix, Error> {
    let run = run.map(|run| run.0.as_str());
    let runs: Vec<(i64, String, Json<RunState>)> = sqlx::query_as(
        "SELECT sequence, run_id, phase FROM runtime_commit
         WHERE thread_id = $1 AND sequence <= $2 AND ($3::TEXT IS NULL OR run_id = $3)
         ORDER BY sequence",
    )
    .bind(&thread.0)
    .bind(through)
    .bind(run)
    .fetch_all(&mut *tx)
    .await
    .map_err(reject)?;
    let messages: Vec<(i64, Json<Message>)> = sqlx::query_as(
        "SELECT m.commit_sequence, m.data FROM runtime_message m
         WHERE m.thread_id = $1 AND m.commit_sequence <= $2 AND ($3::TEXT IS NULL OR EXISTS (
            SELECT 1 FROM runtime_commit c WHERE c.sequence = m.commit_sequence AND c.run_id = $3
         )) ORDER BY m.id",
    )
    .bind(&thread.0)
    .bind(through)
    .bind(run)
    .fetch_all(&mut *tx)
    .await
    .map_err(reject)?;
    let state: Vec<(i64, Json<StateCommand>)> = sqlx::query_as(
        "SELECT m.commit_sequence, m.data FROM runtime_state_command m
         WHERE m.thread_id = $1 AND m.commit_sequence <= $2 AND ($3::TEXT IS NULL OR EXISTS (
            SELECT 1 FROM runtime_commit c WHERE c.sequence = m.commit_sequence AND c.run_id = $3
         )) ORDER BY m.id",
    )
    .bind(&thread.0)
    .bind(through)
    .bind(run)
    .fetch_all(&mut *tx)
    .await
    .map_err(reject)?;
    let head = decode_authority(through)?;
    let event_ceiling = encode_run_lifecycle_cursor(head + 1, 0)
        .ok()
        .and_then(|cursor| i64::try_from(cursor.0 - 1).ok())
        .unwrap_or(i64::MAX);
    let events: Vec<(i64, String, Json<AuditKind>, Json<serde_json::Value>)> = sqlx::query_as("SELECT sequence, run_id, kind, payload FROM runtime_event WHERE sequence <= $3 AND run_id IN (SELECT run_id FROM runtime_commit WHERE thread_id = $1 AND sequence <= $2 AND ($4::TEXT IS NULL OR run_id = $4)) ORDER BY sequence")
        .bind(&thread.0).bind(through).bind(event_ceiling).bind(run).fetch_all(tx).await.map_err(reject)?;
    Ok(CommittedThreadPrefix {
        thread_id: thread.clone(),
        runs: runs
            .into_iter()
            .map(|(seq, run, Json(state))| Ok((decode_authority(seq)?, RunId(run), state)))
            .collect::<Result<_, Error>>()?,
        messages: messages
            .into_iter()
            .map(decode_fact)
            .collect::<Result<_, _>>()?,
        state: state
            .into_iter()
            .map(decode_fact)
            .collect::<Result<_, _>>()?,
        events: events
            .into_iter()
            .map(|(seq, run, Json(kind), Json(payload))| {
                Ok(EventRecord {
                    sequence: decode_authority(seq)?,
                    run_id: RunId(run),
                    kind,
                    payload,
                })
            })
            .collect::<Result<_, Error>>()?,
    })
}

fn decode_fact<T>((sequence, Json(fact)): (i64, Json<T>)) -> Result<(u64, T), Error> {
    Ok((decode_authority(sequence)?, fact))
}

pub(super) async fn transcript_seek_page(
    pool: &sqlx::PgPool,
    request: &awaken_agent_contract::thread::read::transcript::TranscriptSeekRequest,
) -> Result<awaken_agent_contract::thread::read::transcript::TranscriptSeekPage, super::StoreError>
{
    use super::StoreError;
    use awaken_agent_contract::thread::read::transcript::{TranscriptSeek, TranscriptSeekPage};

    fn read(error: impl std::fmt::Display) -> StoreError {
        StoreError::Read(error.to_string())
    }
    let mut tx = pool.begin().await.map_err(read)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(read)?;
    let (through, before) = match request.seek {
        Some(seek) => {
            let through = i64::try_from(seek.through).map_err(read)?;
            let before = i64::try_from(seek.before).map_err(read)?;
            if before <= 0 || before > through {
                return Err(StoreError::Transcript(
                    awaken_agent_contract::thread::read::transcript::TranscriptError::SnapshotMismatch,
                ));
            }
            let anchors: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM runtime_message WHERE thread_id = $1 AND id IN ($2, $3)",
            )
            .bind(&request.thread_id.0)
            .bind(through)
            .bind(before)
            .fetch_one(&mut *tx)
            .await
            .map_err(read)?;
            if anchors != if through == before { 1 } else { 2 } {
                return Err(StoreError::Transcript(
                    awaken_agent_contract::thread::read::transcript::TranscriptError::SnapshotMismatch,
                ));
            }
            (through, Some(before))
        }
        None => {
            let head: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(id), 0) FROM runtime_message WHERE thread_id = $1",
            )
            .bind(&request.thread_id.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(read)?;
            (head, None)
        }
    };
    let limit = request.limit.get();
    let probe = i64::try_from(limit.saturating_add(1)).map_err(read)?;
    let mut rows: Vec<(i64, Json<Message>)> = sqlx::query_as(
        "SELECT id, data FROM runtime_message
         WHERE thread_id = $1 AND id <= $2 AND ($3::BIGINT IS NULL OR id < $3)
           AND data->>'role' IN ('User', 'Assistant')
         ORDER BY id DESC LIMIT $4",
    )
    .bind(&request.thread_id.0)
    .bind(through)
    .bind(before)
    .bind(probe)
    .fetch_all(&mut *tx)
    .await
    .map_err(read)?;
    tx.commit().await.map_err(read)?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = match rows.last().filter(|_| more) {
        Some((id, _)) => Some(TranscriptSeek {
            through: u64::try_from(through).map_err(read)?,
            before: u64::try_from(*id).map_err(read)?,
        }),
        None => None,
    };
    Ok(TranscriptSeekPage {
        messages: rows
            .into_iter()
            .rev()
            .map(|(_, Json(message))| message)
            .collect(),
        next,
    })
}
