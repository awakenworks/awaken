//! Private-transcript corpus importers.
//!
//! Import is deliberately one-way and lossy: provider/session identifiers,
//! evaluator rationales and home paths are not copied into the Judge dataset.
//! Claude's `goal_status.reason` remains oracle audit material in the source
//! transcript and is never shown to the model under evaluation.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use awaken_ext_goal::outcome::GradeDecision;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::outcome_judge::{JudgeCase, JudgeDataset, SCHEMA_VERSION, SourceKind};

const MAX_DELIVERABLE_CHARS: usize = 16_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeImportStats {
    pub files_scanned: usize,
    pub goal_status_rows: usize,
    pub sentinel_rows_skipped: usize,
    pub rows_without_deliverable: usize,
    pub duplicates_skipped: usize,
    pub satisfied_available: usize,
    pub needs_revision_available: usize,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexImportStats {
    pub completed_goals: usize,
    pub matched_transcripts: usize,
    pub completion_calls: usize,
    pub rows_without_deliverable: usize,
    pub selected: usize,
}

/// Import Codex goals that the agent explicitly concluded with
/// `update_goal({status:"complete"})`. These are positive-only, self-judged weak
/// labels; they measure cross-Judge agreement, not objective accuracy.
pub fn import_codex_goals(
    sessions_root: &Path,
    goals_db: &Path,
    dataset_name: impl Into<String>,
    limit: usize,
) -> io::Result<(JudgeDataset, CodexImportStats)> {
    let connection =
        Connection::open_with_flags(goals_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sqlite_error)?;
    let mut statement = connection
        .prepare(
            "SELECT thread_id, objective FROM thread_goals \
             WHERE status = 'complete' ORDER BY thread_id",
        )
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let mut stats = CodexImportStats {
        completed_goals: rows.len(),
        matched_transcripts: 0,
        completion_calls: 0,
        rows_without_deliverable: 0,
        selected: 0,
    };
    let mut paths = Vec::new();
    collect_jsonl(sessions_root, &mut paths)?;
    paths.sort();
    let by_thread: std::collections::BTreeMap<String, PathBuf> = paths
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let thread_id = find_uuid_suffix(name)?;
            Some((thread_id, path))
        })
        .collect();
    let mut cases = Vec::new();
    for (thread_id, objective) in rows {
        let Some(path) = by_thread.get(&thread_id) else {
            continue;
        };
        stats.matched_transcripts += 1;
        let (completion_calls, deliverable) = codex_completion_deliverable(path)?;
        stats.completion_calls += completion_calls;
        let Some(deliverable) = deliverable else {
            stats.rows_without_deliverable += 1;
            continue;
        };
        let objective = sanitize(&objective);
        let deliverable = truncate_tail(&sanitize(&deliverable), MAX_DELIVERABLE_CHARS);
        let fingerprint = stable_hash(&format!("{thread_id}\0{objective}\0{deliverable}"));
        cases.push(JudgeCase {
            id: format!("codex-goal-{fingerprint:016x}"),
            source: SourceKind::CodexTranscriptDerived,
            tags: vec![
                "private_transcript".into(),
                "weak_oracle".into(),
                "self_judged".into(),
                "complete".into(),
            ],
            description: objective.clone(),
            rubric: format!(
                "Return satisfied only when the deliverable establishes this goal as complete: {objective}"
            ),
            deliverable,
            worker_state: serde_json::json!({ "source": "codex_update_goal_complete" }),
            evidence: Vec::new(),
            expected: GradeDecision::Satisfied,
            required_reason_terms: Vec::new(),
        });
    }
    cases.sort_by(|a, b| a.id.cmp(&b.id));
    if limit > 0 {
        cases.truncate(limit);
    }
    stats.selected = cases.len();
    let dataset = JudgeDataset {
        schema_version: SCHEMA_VERSION,
        name: dataset_name.into(),
        cases,
    };
    dataset
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((dataset, stats))
}

fn codex_completion_deliverable(path: &Path) -> io::Result<(usize, Option<String>)> {
    let file = fs::File::open(path)?;
    let mut assistant_turns = std::collections::VecDeque::new();
    let mut completion_calls = 0;
    let mut deliverable = None;
    let mut awaiting_post_completion_summary = false;
    for line in io::BufReader::new(file).lines() {
        let line = line?;
        let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(text) = codex_assistant_text(&row) {
            if awaiting_post_completion_summary {
                deliverable = Some(text.clone());
                awaiting_post_completion_summary = false;
            }
            assistant_turns.push_back(text);
            while assistant_turns.len() > 6 {
                assistant_turns.pop_front();
            }
        }
        let payload = row.get("payload").unwrap_or(&serde_json::Value::Null);
        let is_completion = payload.get("type").and_then(|value| value.as_str())
            == Some("custom_tool_call")
            && payload
                .get("input")
                .and_then(|value| value.as_str())
                .is_some_and(|input| {
                    input.contains("tools.update_goal")
                        && input.contains("status")
                        && input.contains("complete")
                });
        if is_completion {
            completion_calls += 1;
            let text = assistant_turns
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n\n");
            if !text.trim().is_empty() {
                deliverable = Some(text);
            }
            awaiting_post_completion_summary = true;
        }
    }
    Ok((completion_calls, deliverable))
}

fn codex_assistant_text(row: &serde_json::Value) -> Option<String> {
    let payload = row.get("payload")?;
    if payload.get("type").and_then(|value| value.as_str()) != Some("message")
        || payload.get("role").and_then(|value| value.as_str()) != Some("assistant")
    {
        return None;
    }
    if payload
        .get("phase")
        .and_then(|value| value.as_str())
        .is_some_and(|phase| phase != "final_answer")
    {
        return None;
    }
    let text = payload
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|block| block.get("text").and_then(|value| value.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn find_uuid_suffix(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".jsonl")?;
    // Rollout filenames end in a UUID; byte slicing is safe because this suffix
    // and separator are ASCII even when a parent directory contains Unicode.
    (stem.len() >= 36).then(|| stem[stem.len() - 36..].to_string())
}

fn sqlite_error(error: rusqlite::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// Import a deterministic, label-balanced sample from Claude Code `/goal`
/// transcripts. `limit == 0` selects every usable, de-duplicated record.
pub fn import_claude_goals(
    root: &Path,
    dataset_name: impl Into<String>,
    limit: usize,
) -> io::Result<(JudgeDataset, ClaudeImportStats)> {
    let mut paths = Vec::new();
    collect_jsonl(root, &mut paths)?;
    paths.sort();

    let mut stats = ClaudeImportStats {
        files_scanned: paths.len(),
        goal_status_rows: 0,
        sentinel_rows_skipped: 0,
        rows_without_deliverable: 0,
        duplicates_skipped: 0,
        satisfied_available: 0,
        needs_revision_available: 0,
        selected: 0,
    };
    let mut seen = BTreeSet::new();
    let mut satisfied = Vec::new();
    let mut needs_revision = Vec::new();
    for path in paths {
        import_claude_file(
            &path,
            &mut seen,
            &mut satisfied,
            &mut needs_revision,
            &mut stats,
        )?;
    }
    satisfied.sort_by(|a, b| a.id.cmp(&b.id));
    needs_revision.sort_by(|a, b| a.id.cmp(&b.id));
    stats.satisfied_available = satisfied.len();
    stats.needs_revision_available = needs_revision.len();

    let mut cases = if limit == 0 {
        satisfied.into_iter().chain(needs_revision).collect()
    } else {
        let satisfied_quota = limit.div_ceil(2);
        let needs_revision_quota = limit / 2;
        let mut selected: Vec<JudgeCase> = satisfied
            .drain(..satisfied_quota.min(satisfied.len()))
            .collect();
        selected.extend(needs_revision.drain(..needs_revision_quota.min(needs_revision.len())));
        if selected.len() < limit {
            selected.extend(satisfied.into_iter().take(limit - selected.len()));
        }
        if selected.len() < limit {
            selected.extend(needs_revision.into_iter().take(limit - selected.len()));
        }
        selected
    };
    cases.sort_by(|a, b| a.id.cmp(&b.id));
    stats.selected = cases.len();
    let dataset = JudgeDataset {
        schema_version: SCHEMA_VERSION,
        name: dataset_name.into(),
        cases,
    };
    dataset
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((dataset, stats))
}

fn import_claude_file(
    path: &Path,
    seen: &mut BTreeSet<u64>,
    satisfied: &mut Vec<JudgeCase>,
    needs_revision: &mut Vec<JudgeCase>,
    stats: &mut ClaudeImportStats,
) -> io::Result<()> {
    let file = fs::File::open(path)?;
    let mut last_assistant = None;
    for line in io::BufReader::new(file).lines() {
        let line = line?;
        let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(text) = assistant_text(&row) {
            last_assistant = Some(text);
        }
        let Some(attachment) = row.get("attachment") else {
            continue;
        };
        if attachment.get("type").and_then(|value| value.as_str()) != Some("goal_status") {
            continue;
        }
        stats.goal_status_rows += 1;
        if attachment
            .get("sentinel")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            stats.sentinel_rows_skipped += 1;
            continue;
        }
        let Some(met) = attachment.get("met").and_then(|value| value.as_bool()) else {
            continue;
        };
        let Some(condition) = attachment
            .get("condition")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        let Some(deliverable) = last_assistant
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            stats.rows_without_deliverable += 1;
            continue;
        };
        let condition = sanitize(condition);
        let deliverable = truncate_tail(&sanitize(deliverable), MAX_DELIVERABLE_CHARS);
        let fingerprint = stable_hash(&format!("{met}\0{condition}\0{deliverable}"));
        if !seen.insert(fingerprint) {
            stats.duplicates_skipped += 1;
            continue;
        }
        let expected = if met {
            GradeDecision::Satisfied
        } else {
            GradeDecision::NeedsRevision
        };
        let case = JudgeCase {
            id: format!("claude-goal-{fingerprint:016x}"),
            source: SourceKind::ClaudeTranscriptDerived,
            tags: vec![
                "private_transcript".into(),
                "weak_oracle".into(),
                if met { "met" } else { "unmet" }.into(),
            ],
            description: condition.clone(),
            rubric: format!(
                "Return satisfied only when the deliverable establishes this completion condition: {condition}"
            ),
            deliverable,
            worker_state: serde_json::json!({ "source": "claude_goal_status" }),
            evidence: Vec::new(),
            expected,
            required_reason_terms: Vec::new(),
        };
        if met {
            satisfied.push(case);
        } else {
            needs_revision.push(case);
        }
    }
    Ok(())
}

fn assistant_text(row: &serde_json::Value) -> Option<String> {
    let message = row.get("message")?;
    if message.get("role").and_then(|value| value.as_str()) != Some("assistant") {
        return None;
    }
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let text = content
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(|value| value.as_str()) == Some("text"))
        .filter_map(|block| block.get("text").and_then(|value| value.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn collect_jsonl(root: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    if root.is_file() {
        if root.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            paths.push(root.to_path_buf());
        }
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_jsonl(&path, paths)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
    Ok(())
}

fn sanitize(text: &str) -> String {
    text.replace("/home/chaizhenhua", "<home>")
}

fn truncate_tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let tail = text.chars().skip(count - max_chars).collect::<String>();
    format!("[earlier deliverable text omitted]\n{tail}")
}

fn stable_hash(text: &str) -> u64 {
    // FNV-1a is sufficient for deterministic corpus ids; this is not a security
    // or anonymisation primitive, and no source text is recoverable from the id.
    text.as_bytes()
        .iter()
        .fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn imports_balanced_goal_status_without_oracle_reason_leakage() {
        let dir =
            std::env::temp_dir().join(format!("awaken-claude-goal-import-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        for (n, met) in [false, true, false].into_iter().enumerate() {
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "assistant",
                    "message": { "role": "assistant", "content": [{
                        "type": "text", "text": format!("deliverable {n} /home/chaizhenhua/project")
                    }] }
                })
            )
            .unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "attachment",
                    "attachment": {
                        "type": "goal_status",
                        "met": met,
                        "condition": format!("condition {n}"),
                        "reason": format!("SECRET ORACLE {n}")
                    }
                })
            )
            .unwrap();
        }
        drop(file);

        let (dataset, stats) = import_claude_goals(&dir, "private", 2).unwrap();
        assert_eq!(stats.goal_status_rows, 3);
        assert_eq!(stats.selected, 2);
        assert_eq!(
            dataset
                .cases
                .iter()
                .filter(|case| case.expected == GradeDecision::Satisfied)
                .count(),
            1
        );
        assert!(
            dataset
                .cases
                .iter()
                .all(|case| !case.deliverable.contains("SECRET ORACLE"))
        );
        assert!(
            dataset
                .cases
                .iter()
                .all(|case| !case.deliverable.contains("/home/chaizhenhua"))
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sentinel_and_duplicate_rows_do_not_become_cases() {
        let dir =
            std::env::temp_dir().join(format!("awaken-claude-goal-dedupe-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let assistant = serde_json::json!({
            "message": { "role": "assistant", "content": "done" }
        });
        let status = serde_json::json!({
            "attachment": { "type": "goal_status", "met": true, "condition": "ship" }
        });
        let sentinel = serde_json::json!({
            "attachment": {
                "type": "goal_status", "met": false, "sentinel": true, "condition": "ship"
            }
        });
        let mut file = fs::File::create(&path).unwrap();
        for row in [&assistant, &sentinel, &status, &status] {
            writeln!(file, "{row}").unwrap();
        }
        drop(file);
        let (dataset, stats) = import_claude_goals(&dir, "private", 0).unwrap();
        assert_eq!(dataset.cases.len(), 1);
        assert_eq!(stats.sentinel_rows_skipped, 1);
        assert_eq!(stats.duplicates_skipped, 1);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn codex_import_prefers_the_summary_after_complete_tool_call() {
        let dir =
            std::env::temp_dir().join(format!("awaken-codex-goal-import-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let thread_id = "01234567-89ab-cdef-0123-456789abcdef";
        let transcript = dir.join(format!("rollout-2026-01-01T00-00-00-{thread_id}.jsonl"));
        let rows = [
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message", "role": "assistant",
                    "content": [{"type":"output_text", "text":"still running"}]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call", "name": "exec",
                    "input": "const r = await tools.update_goal({status:\"complete\"});"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message", "role": "assistant",
                    "content": [{"type":"output_text", "text":"FINAL: tests pass"}]
                }
            }),
        ];
        let mut file = fs::File::create(&transcript).unwrap();
        for row in rows {
            writeln!(file, "{row}").unwrap();
        }
        drop(file);
        let db = dir.join("goals.sqlite");
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE thread_goals (
                    thread_id TEXT PRIMARY KEY, goal_id TEXT, objective TEXT,
                    status TEXT, token_budget INTEGER, tokens_used INTEGER,
                    time_used_seconds INTEGER, created_at_ms INTEGER, updated_at_ms INTEGER
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO thread_goals VALUES (?1, 'g', 'ship', 'complete', NULL, 0, 0, 0, 0)",
                [thread_id],
            )
            .unwrap();
        drop(connection);

        let (dataset, stats) = import_codex_goals(&dir, &db, "codex", 0).unwrap();
        assert_eq!(stats.selected, 1);
        assert_eq!(dataset.cases[0].deliverable, "FINAL: tests pass");
        fs::remove_dir_all(dir).ok();
    }
}
