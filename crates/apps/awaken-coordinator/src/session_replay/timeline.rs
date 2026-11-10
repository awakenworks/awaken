// @formal surface requirement=analytics.observability.projection_only
// @formal reason: Reads scope-bound committed evidence at frozen coordinates and projects safe diagnostic fields.
//! Read-side diagnostic cut and keyset pagination over the original facts.

use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::{RunId, ThreadId};
use awaken_dispatch_contract::run_dispatch::ArchivedRunActivation;
use awaken_runtime_contract::resolved::Backend;
use awaken_session_application::replay::{
    EvidenceRef, EvidenceSource, ReplayAgentMessage, ReplayRunActivation, RunActivationEvidence,
    RunBackendEvidence, RunEvidence, SessionReplayError, TimelineCursor, TimelineEntry,
    TimelinePage, TimelineQuery, TimelineSnapshot,
};
use awaken_session_types::{SessionId, SessionRevision};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::{PgConnection, types::Json};

use super::{PostgresSessionReplay, causality, history, positive, unavailable};

const MAX_PAGE: usize = 100;
const MAX_RELATIONS: usize = 32;
const MAX_TOKEN_BYTES: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cut {
    scope: String,
    session: String,
    revision: i64,
    runtime: i64,
    dispatch: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Position(i64, i16, i64);

#[derive(Serialize, Deserialize)]
struct PageToken {
    cut: Cut,
    query: TimelineQuery,
    after: Position,
}

type EvidenceRow = (
    i16,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Json<serde_json::Value>,
);

impl Cut {
    fn require_access(&self, scope: &str, session: &SessionId) -> Result<(), SessionReplayError> {
        if self.scope != scope || self.session != session.as_str() {
            return Err(SessionReplayError::NotFound);
        }
        Ok(())
    }
}

fn seal<T: Serialize>(
    key: &blake3::Hash,
    domain: &[u8],
    value: &T,
) -> Result<String, SessionReplayError> {
    let data = serde_json::to_vec(value).map_err(history)?;
    let mut hasher = blake3::Hasher::new_keyed(key.as_bytes());
    hasher.update(domain);
    hasher.update(&data);
    let mac = hasher.finalize();
    Ok(format!("{}.{}", URL_SAFE_NO_PAD.encode(data), mac.to_hex()))
}

fn open<T: DeserializeOwned>(
    key: &blake3::Hash,
    domain: &[u8],
    token: &str,
) -> Result<T, SessionReplayError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(SessionReplayError::InvalidQuery);
    }
    let (data, mac) = token
        .split_once('.')
        .ok_or(SessionReplayError::InvalidQuery)?;
    let data = URL_SAFE_NO_PAD
        .decode(data)
        .map_err(|_| SessionReplayError::InvalidQuery)?;
    let mac = blake3::Hash::from_hex(mac).map_err(|_| SessionReplayError::InvalidQuery)?;
    let mut hasher = blake3::Hasher::new_keyed(key.as_bytes());
    hasher.update(domain);
    hasher.update(&data);
    if hasher.finalize() != mac {
        return Err(SessionReplayError::InvalidQuery);
    }
    serde_json::from_slice(&data).map_err(|_| SessionReplayError::InvalidQuery)
}

async fn key(tx: &mut PgConnection) -> Result<blake3::Hash, SessionReplayError> {
    let hex: String =
        sqlx::query_scalar("SELECT key_hex FROM managed_session_diagnostic_key WHERE singleton")
            .fetch_one(tx)
            .await
            .map_err(unavailable)?;
    blake3::Hash::from_hex(&hex).map_err(history)
}

async fn current_revision(
    tx: &mut PgConnection,
    scope: &str,
    session: &SessionId,
) -> Result<i64, SessionReplayError> {
    sqlx::query_scalar(
        "SELECT revision FROM managed_session WHERE scope_id = $1 AND session_id = $2",
    )
    .bind(scope)
    .bind(session.as_str())
    .fetch_optional(tx)
    .await
    .map_err(unavailable)?
    .ok_or(SessionReplayError::NotFound)
}

async fn check_cut(
    tx: &mut PgConnection,
    scope: &str,
    session: &SessionId,
    cut: &Cut,
) -> Result<(), SessionReplayError> {
    cut.require_access(scope, session)?;
    current_revision(tx, scope, session).await?;
    let retained: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM managed_session_revision
           WHERE scope_id = $1 AND session_id = $2 AND revision = $3)",
    )
    .bind(scope)
    .bind(session.as_str())
    .bind(cut.revision)
    .fetch_one(tx)
    .await
    .map_err(unavailable)?;
    if retained {
        Ok(())
    } else {
        Err(history("the frozen Session revision is unavailable"))
    }
}

pub(crate) async fn timeline(
    service: &PostgresSessionReplay,
    scope: &str,
    session: &SessionId,
    query: &TimelineQuery,
    cursor: Option<&TimelineCursor>,
) -> Result<TimelinePage, SessionReplayError> {
    if !(1..=MAX_PAGE).contains(&query.limit)
        || query.from_ms.is_some_and(|from| from < 0)
        || query.to_ms.is_some_and(|to| to < 0)
        || query
            .from_ms
            .zip(query.to_ms)
            .is_some_and(|(from, to)| from >= to)
        || query
            .operation_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 512)
        || query
            .thread_id
            .as_ref()
            .is_some_and(|id| id.0.is_empty() || id.0.len() > 256)
        || query
            .run_id
            .as_ref()
            .is_some_and(|id| id.0.is_empty() || id.0.len() > 256)
    {
        return Err(SessionReplayError::InvalidQuery);
    }

    let mut tx = service.begin().await?;
    let (key, cut, after) = if let Some(cursor) = cursor {
        let key = key(&mut tx).await?;
        let token: PageToken = open(&key, b"awaken.timeline.cursor.v1", &cursor.0)?;
        if token.query != *query {
            return Err(SessionReplayError::InvalidQuery);
        }
        check_cut(&mut tx, scope, session, &token.cut).await?;
        (key, token.cut, token.after)
    } else {
        // Capture the Dispatch complete-prefix barrier before establishing the
        // transaction snapshot, including concurrently allocated admissions.
        sqlx::query("LOCK TABLE runtime_dispatch_operation IN SHARE MODE")
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        let revision = current_revision(&mut tx, scope, session).await?;
        let runtime = awaken_store_postgres::replay_head(&mut tx)
            .await
            .map_err(unavailable)?;
        let dispatch = sqlx::query_scalar(
            "SELECT GREATEST(
                COALESCE((SELECT MAX(sequence) FROM runtime_dispatch_operation), 0),
                COALESCE((SELECT MAX(sequence) FROM runtime_dispatch_session_membership), 0)
             )",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(unavailable)?;
        let cut = Cut {
            scope: scope.into(),
            session: session.as_str().into(),
            revision,
            runtime,
            dispatch,
        };
        check_cut(&mut tx, scope, session, &cut).await?;
        let key = key(&mut tx).await?;
        tx.commit().await.map_err(unavailable)?;
        tx = service.begin().await?;
        check_cut(&mut tx, scope, session, &cut).await?;
        (key, cut, Position(i64::MAX, i16::MAX, i64::MAX))
    };
    let mut rows: Vec<EvidenceRow> = sqlx::query_as(TIMELINE_SQL)
        .bind(scope)
        .bind(session.as_str())
        .bind(cut.revision)
        .bind(cut.runtime)
        .bind(cut.dispatch)
        .bind(query.from_ms)
        .bind(query.to_ms)
        .bind(query.thread_id.as_ref().map(|id| &id.0))
        .bind(query.run_id.as_ref().map(|id| &id.0))
        .bind(&query.operation_id)
        .bind(query.source.map(source_rank))
        .bind(after.0)
        .bind(after.1)
        .bind(after.2)
        .bind(i64::try_from(query.limit + 1).map_err(history)?)
        .fetch_all(&mut *tx)
        .await
        .map_err(unavailable)?;
    tx.commit().await.map_err(unavailable)?;

    let more = rows.len() > query.limit;
    rows.truncate(query.limit);
    let snapshot = TimelineSnapshot(seal(&key, b"awaken.timeline.snapshot.v1", &cut)?);
    let next_cursor = rows
        .last()
        .filter(|_| more)
        .map(|row| {
            let after = Position(row.2.unwrap_or(-1), row.0, row.1);
            seal(
                &key,
                b"awaken.timeline.cursor.v1",
                &PageToken {
                    cut,
                    query: query.clone(),
                    after,
                },
            )
            .map(TimelineCursor)
        })
        .transpose()?;
    Ok(TimelinePage {
        entries: rows.into_iter().map(entry).collect::<Result<_, _>>()?,
        snapshot,
        next_cursor,
    })
}

fn source_rank(source: EvidenceSource) -> i16 {
    match source {
        EvidenceSource::Session => 0,
        EvidenceSource::Dispatch => 1,
        EvidenceSource::Runtime => 2,
    }
}

fn entry(row: EvidenceRow) -> Result<TimelineEntry, SessionReplayError> {
    let (source, position, recorded_at_ms, thread, run, operation_id, kind, Json(mut detail)) = row;
    let source = match source {
        0 => EvidenceSource::Session,
        1 => EvidenceSource::Dispatch,
        2 => EvidenceSource::Runtime,
        _ => return Err(history("unknown diagnostic source")),
    };
    if kind == "runtime_commit" {
        let phase: RunState = serde_json::from_value(detail["phase"].take()).map_err(history)?;
        let (phase, end_cause) = match phase {
            RunState::Running => ("running", None),
            RunState::Awaiting => ("awaiting", None),
            RunState::Ended(cause) => (
                "ended",
                Some(match cause {
                    EndCause::NaturalEnd => "natural_end",
                    EndCause::MaxSteps => "max_steps",
                    EndCause::Cancelled => "cancelled",
                    EndCause::Stopped(_) => "stopped",
                    EndCause::Error(_) => "error",
                    EndCause::Indeterminate => "indeterminate",
                }),
            ),
        };
        detail = serde_json::json!({ "phase": phase });
        if let Some(cause) = end_cause {
            detail["end_cause"] = cause.into();
        }
    }
    Ok(TimelineEntry {
        evidence: EvidenceRef { source, position },
        recorded_at_ms,
        thread_id: thread.map(ThreadId),
        run_id: run.map(RunId),
        operation_id,
        kind,
        detail,
    })
}

pub(crate) async fn run_evidence_at(
    service: &PostgresSessionReplay,
    scope: &str,
    session: &SessionId,
    run: &RunId,
    snapshot: &TimelineSnapshot,
) -> Result<RunEvidence, SessionReplayError> {
    let mut tx = service.begin().await?;
    let cut: Cut = open(
        &key(&mut tx).await?,
        b"awaken.timeline.snapshot.v1",
        &snapshot.0,
    )?;
    check_cut(&mut tx, scope, session, &cut).await?;
    let admission: Option<(String, i64, i64, String, Option<String>)> = sqlx::query_as(
        "SELECT thread_id, sequence, recorded_at_ms, initial_status, parent_run_id
         FROM runtime_dispatch_session_membership
         WHERE scope_id = $1 AND session_id = $2 AND run_id = $3 AND sequence <= $4",
    )
    .bind(scope)
    .bind(session.as_str())
    .bind(&run.0)
    .bind(cut.dispatch)
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;
    let archive = archive_for_run(&mut tx, &cut, run).await?;
    let thread = match (&admission, &archive) {
        (Some((thread, ..)), Some((archived, _, _))) if thread != archived => {
            return Err(history(
                "admission and activation disagree about the Thread",
            ));
        }
        (Some((thread, ..)), _) | (_, Some((thread, _, _))) => ThreadId(thread.clone()),
        _ => {
            let root: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM runtime_commit
                 WHERE thread_id = $1 AND run_id = $2 AND sequence <= $3)",
            )
            .bind(session.as_str())
            .bind(&run.0)
            .bind(cut.runtime)
            .fetch_one(&mut *tx)
            .await
            .map_err(unavailable)?;
            if !root {
                return Err(SessionReplayError::NotFound);
            }
            ThreadId(session.to_string())
        }
    };

    let runtime: Option<EvidenceRow> = sqlx::query_as(
        "SELECT 2::SMALLINT, c.sequence,
                FLOOR(EXTRACT(EPOCH FROM c.committed_at) * 1000)::BIGINT,
                c.thread_id, c.run_id,
                CASE WHEN r.operation_ordinal IS NOT NULL THEN
                    c.run_id || ':' || r.operation_ordinal::TEXT END,
                'runtime_commit'::TEXT, jsonb_build_object('phase', c.phase)
         FROM runtime_commit c LEFT JOIN runtime_commit_receipt r
           ON r.operation_run_id = c.run_id AND r.commit_sequence = c.sequence
         WHERE c.thread_id = $1 AND c.run_id = $2 AND c.sequence <= $3
         ORDER BY c.sequence DESC LIMIT 1",
    )
    .bind(&thread.0)
    .bind(&run.0)
    .bind(cut.runtime)
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;
    let dispatch: Option<EvidenceRow> = sqlx::query_as(
        "SELECT 1::SMALLINT, sequence, recorded_at_ms, $3::TEXT, run_id, NULL::TEXT,
                operation->>'type', jsonb_strip_nulls(jsonb_build_object(
                    'claim', COALESCE(operation->'claim', operation->'attempt'->'claim'),
                    'previous', operation->'previous', 'reason', operation->'reason',
                    'outcome', operation->'outcome', 'attempt_count', operation->'attempt_count'
                ))
         FROM runtime_dispatch_operation WHERE run_id = $1 AND sequence <= $2
          AND operation->>'type' IN
              ('attempt_admitted', 'claimed', 'lease_lost', 'reclaimed', 'settled', 'dead_lettered')
         ORDER BY sequence DESC LIMIT 1",
    )
    .bind(&run.0)
    .bind(cut.dispatch)
    .bind(&thread.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;
    let parent = archive
        .as_ref()
        .and_then(|(_, _, data)| {
            data.activation
                .delegation_origin
                .as_ref()
                .map(|origin| origin.parent_run_id.clone())
        })
        .or_else(|| admission.as_ref()?.4.clone().map(RunId));
    let (messages, more_messages) =
        related_messages(&mut tx, &cut, session, &thread, run, parent.as_ref()).await?;
    tx.commit().await.map_err(unavailable)?;

    Ok(RunEvidence {
        run_id: run.clone(),
        thread_id: thread.clone(),
        parent_run_id: parent,
        snapshot: snapshot.clone(),
        session_revision: SessionRevision(positive(cut.revision)?),
        admission: admission.map(|(_, position, time, status, _)| TimelineEntry {
            evidence: EvidenceRef {
                source: EvidenceSource::Dispatch,
                position,
            },
            recorded_at_ms: Some(time),
            thread_id: Some(thread),
            run_id: Some(run.clone()),
            operation_id: None,
            kind: "dispatch_admission".into(),
            detail: serde_json::json!({ "status": status }),
        }),
        activation: archive.map(activation_evidence).transpose()?,
        runtime: runtime.map(entry).transpose()?,
        dispatch: dispatch.map(entry).transpose()?,
        messages,
        more_messages,
    })
}

type Archive = (String, i64, ArchivedRunActivation);

fn activation_evidence(
    (_, first_commit, archive): Archive,
) -> Result<RunActivationEvidence, SessionReplayError> {
    let snapshot = &archive.activation.snapshot;
    let spec = &snapshot.resolved_spec;
    let candidate = match archive.activation.model_ref_override.as_deref() {
        Some(model) if !model.is_empty() => spec
            .candidate_for_model(model)
            .ok_or_else(|| history("activation model is missing from the archived publication"))?,
        _ => &spec.model_binding,
    };
    let binding = candidate.binding();
    let backend = match binding.backend() {
        Backend::Native => RunBackendEvidence::Native,
        Backend::Acp(backend) => RunBackendEvidence::Acp {
            cli: backend.cli().into(),
        },
        Backend::Remote(_) => RunBackendEvidence::A2a,
        Backend::Invalid(_) => return Err(history("invalid archived execution backend")),
    };
    Ok(RunActivationEvidence {
        agent_id: snapshot.root_agent_id.0.clone(),
        agent_revision: (!snapshot.metadata.is_legacy_default())
            .then_some(snapshot.metadata.source.revision),
        publication_fingerprint: snapshot.fingerprint.0.clone(),
        selected_model: binding.identity().1.to_owned(),
        backend,
        resource_revision: archive.session_resources.map(|r| r.resource_revision),
        first_commit,
    })
}

async fn archive_for_run(
    tx: &mut PgConnection,
    cut: &Cut,
    run: &RunId,
) -> Result<Option<Archive>, SessionReplayError> {
    let row: Option<(String, i64, Json<ArchivedRunActivation>)> = sqlx::query_as(
        "SELECT thread_id, first_commit_sequence, data FROM runtime_run_activation
         WHERE scope_id = $1 AND session_id = $2 AND run_id = $3 AND first_commit_sequence <= $4",
    )
    .bind(&cut.scope)
    .bind(&cut.session)
    .bind(&run.0)
    .bind(cut.runtime)
    .fetch_optional(tx)
    .await
    .map_err(unavailable)?;
    row.map(|(thread, sequence, Json(archive))| {
        if archive.activation.thread_id.0 != thread || archive.activation.run_id != *run {
            return Err(history("activation archive differs from its Run key"));
        }
        Ok((thread, sequence, archive))
    })
    .transpose()
}

async fn related_messages(
    tx: &mut PgConnection,
    cut: &Cut,
    session: &SessionId,
    thread: &ThreadId,
    run: &RunId,
    parent: Option<&RunId>,
) -> Result<(Vec<ReplayAgentMessage>, bool), SessionReplayError> {
    let root_thread = ThreadId(session.to_string());
    let source = match parent {
        Some(parent) => parent,
        None if thread == &root_thread => run,
        None => return Ok((Vec::new(), false)),
    };
    let mut root = awaken_store_postgres::replay_run_prefix(tx, &root_thread, source, cut.runtime)
        .await
        .map_err(unavailable)?;
    let rows: Vec<(i64, Json<ArchivedRunActivation>)> = sqlx::query_as(
        "SELECT first_commit_sequence, data FROM runtime_run_activation
         WHERE scope_id = $1 AND session_id = $2 AND first_commit_sequence <= $3
           AND data->'activation'->'delegation_origin'->>'parent_run_id' = $4
           AND ($5::TEXT IS NULL OR run_id = $5)",
    )
    .bind(&cut.scope)
    .bind(&cut.session)
    .bind(cut.runtime)
    .bind(&source.0)
    .bind(parent.map(|_| &run.0))
    .fetch_all(&mut *tx)
    .await
    .map_err(unavailable)?;
    let mut archives: Vec<_> = rows
        .into_iter()
        .map(|(sequence, Json(configuration))| {
            Ok(ReplayRunActivation {
                first_commit: positive(sequence)?,
                configuration,
            })
        })
        .collect::<Result<_, SessionReplayError>>()?;
    let (_, mut sends) = causality::project_sends(session, &root, &archives)?;
    if parent.is_some() {
        sends.retain(|edge| &edge.target_run == run && &edge.target_thread == thread);
    }
    let truncated = sends.len() > MAX_RELATIONS;
    sends.truncate(MAX_RELATIONS);
    let mut children =
        Vec::<awaken_agent_contract::thread::read::replay::CommittedThreadPrefix>::new();
    for edge in &sends {
        let child = awaken_store_postgres::replay_run_prefix(
            tx,
            &edge.target_thread,
            &edge.target_run,
            cut.runtime,
        )
        .await
        .map_err(unavailable)?;
        let report_run =
            awaken_session_execution_contract::agent_report_run_id(session, &edge.target_run);
        let report =
            awaken_store_postgres::replay_run_prefix(tx, &root_thread, &report_run, cut.runtime)
                .await
                .map_err(unavailable)?;
        if let Some((_, sequence, configuration)) = archive_for_run(tx, cut, &report_run).await? {
            archives.push(ReplayRunActivation {
                first_commit: positive(sequence)?,
                configuration,
            });
        }
        root.runs.extend(report.runs);
        root.messages.extend(report.messages);
        if let Some(prefix) = children
            .iter_mut()
            .find(|prefix| prefix.thread_id == child.thread_id)
        {
            prefix.runs.extend(child.runs);
            prefix.messages.extend(child.messages);
            prefix.runs.sort_by_key(|(sequence, _, _)| *sequence);
            prefix.messages.sort_by_key(|(sequence, _)| *sequence);
        } else {
            children.push(child);
        }
    }
    let mut threads = vec![root];
    threads.extend(children);
    causality::complete_messages(session, &threads, &archives, &mut sends)?;
    Ok((sends, truncated))
}

const TIMELINE_SQL: &str = r"
WITH members AS (
    SELECT run_id, thread_id FROM runtime_dispatch_session_membership
    WHERE scope_id = $1 AND session_id = $2 AND sequence <= $5
    UNION
    SELECT run_id, thread_id FROM runtime_run_activation
    WHERE scope_id = $1 AND session_id = $2 AND first_commit_sequence <= $4
), scoped_commits AS (
    SELECT c.* FROM runtime_commit c
    WHERE c.thread_id = $2 AND c.sequence <= $4
    UNION ALL
    SELECT c.* FROM members m JOIN runtime_commit c
        ON c.run_id = m.run_id AND c.thread_id = m.thread_id
    WHERE m.thread_id <> $2 AND c.sequence <= $4
), records AS (
    SELECT 0::SMALLINT AS source, r.revision AS position, r.recorded_at_ms,
           NULL::TEXT AS thread_id, NULL::TEXT AS run_id, NULL::TEXT AS operation_id,
           CASE WHEN r.configuration_changes IS NULL THEN 'session_revision'
                ELSE 'configuration_changed' END AS kind,
           COALESCE(r.configuration_changes, '{}'::JSONB) AS detail
    FROM managed_session_revision r
    WHERE r.scope_id = $1 AND r.session_id = $2 AND r.revision <= $3

    UNION ALL

    SELECT 1::SMALLINT, m.sequence, m.recorded_at_ms, m.thread_id, m.run_id, NULL::TEXT,
           'dispatch_admission', jsonb_build_object('status', m.initial_status)
    FROM runtime_dispatch_session_membership m
    WHERE m.scope_id = $1 AND m.session_id = $2 AND m.sequence <= $5

    UNION ALL

    SELECT 1::SMALLINT, d.sequence, d.recorded_at_ms, m.thread_id, d.run_id, NULL::TEXT,
           d.operation->>'type',
           jsonb_strip_nulls(jsonb_build_object(
               'claim', COALESCE(d.operation->'claim', d.operation->'attempt'->'claim'),
               'previous', d.operation->'previous', 'reason', d.operation->'reason',
               'outcome', d.operation->'outcome', 'attempt_count', d.operation->'attempt_count'
           ))
    FROM members m JOIN runtime_dispatch_operation d USING (run_id)
    WHERE d.sequence <= $5 AND d.operation->>'type' IN
          ('attempt_admitted', 'claimed', 'lease_lost', 'reclaimed', 'settled', 'dead_lettered')

    UNION ALL

    SELECT 2::SMALLINT, c.sequence,
           FLOOR(EXTRACT(EPOCH FROM c.committed_at) * 1000)::BIGINT,
           c.thread_id, c.run_id,
           CASE WHEN r.operation_ordinal IS NOT NULL THEN
               c.run_id || ':' || r.operation_ordinal::TEXT END,
           'runtime_commit', jsonb_build_object('phase', c.phase)
    FROM scoped_commits c
    LEFT JOIN runtime_commit_receipt r ON r.operation_run_id = c.run_id
        AND r.commit_sequence = c.sequence
)
SELECT source, position, recorded_at_ms, thread_id, run_id, operation_id, kind, detail
FROM records r
WHERE ($6::BIGINT IS NULL OR recorded_at_ms >= $6)
  AND ($7::BIGINT IS NULL OR recorded_at_ms < $7)
  AND ($8::TEXT IS NULL OR thread_id = $8)
  AND ($9::TEXT IS NULL OR run_id = $9)
  AND ($10::TEXT IS NULL OR operation_id = $10 OR (
      source = 2 AND EXISTS (
          SELECT 1 FROM runtime_state_command s
          CROSS JOIN LATERAL jsonb_array_elements(
              COALESCE(s.data->'action'->'Set'->'calls', '[]'::JSONB)
          ) call
          WHERE s.thread_id = r.thread_id AND s.commit_sequence = r.position
            AND s.data->>'key' = 'runtime.active_tool_batch.v1'
            AND (s.data->'action'->'Set'->>'id') || ':' || (call->'call'->>'call_id') = $10
      )
  ))
  AND ($11::SMALLINT IS NULL OR source = $11)
  AND (COALESCE(recorded_at_ms, -1), source, position) < ($12, $13, $14)
ORDER BY COALESCE(recorded_at_ms, -1) DESC, source DESC, position DESC
LIMIT $15
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_cannot_be_modified_or_used_as_a_snapshot() {
        let key = blake3::Hash::from_bytes([7; 32]);
        let token = seal(&key, b"cursor", &42).unwrap();
        assert_eq!(open::<i64>(&key, b"cursor", &token).unwrap(), 42);
        assert!(open::<i64>(&key, b"snapshot", &token).is_err());
        let corrupt = format!("A{token}");
        assert!(open::<i64>(&key, b"cursor", &corrupt).is_err());
    }

    #[test]
    fn runtime_evidence_classifies_failures_without_provider_error_text() {
        let phase = RunState::Ended(EndCause::Error(
            awaken_agent_contract::agent::run::Failure::Inference {
                code: "provider_error".into(),
                message: "private upstream text".into(),
            },
        ));
        let row = (
            2,
            1,
            None,
            None,
            None,
            None,
            "runtime_commit".into(),
            Json(serde_json::json!({ "phase": phase })),
        );
        assert_eq!(
            entry(row).unwrap().detail,
            serde_json::json!({ "phase": "ended", "end_cause": "error" }),
        );
    }
}
