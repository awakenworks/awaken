//! Filesystem-backed `TraceStore` implementation.
//!
//! Layout:
//!   `{root}/{yyyy-mm}/{run_id}.ndjson`     — one event per line
//!   `{root}/{yyyy-mm}/{run_id}.idx.json`   — RunSummary, rewritten on RunEnd
//!   `{root}/{yyyy-mm}/{run_id}.ref`        — sentinel; one byte per
//!                                            ReferenceKind discriminant
//!
//! ## Concurrency contract
//!
//! `FileTraceStore` is designed for **single-process access** under the
//! awaken-server. Within a process, `write_lock` serialises every write
//! path (`append`, `write_index_for_run`, `mark_referenced`) and the
//! `prune` scan, so reads never observe a half-written line and a
//! reference that lands during a prune cycle is honoured.
//!
//! Cross-process safety relies only on POSIX `O_APPEND` semantics for the
//! NDJSON shard, which guarantees atomic ordering for writes ≤ `PIPE_BUF`
//! (typically 4 KiB). A serialised `MetricsEvent` larger than `PIPE_BUF`
//! can be split across `write` calls — multiple awaken-server processes
//! writing to the same shard could interleave such records. Multi-process
//! deployments must point each process at its own root, or wait for a
//! future revision that adds an explicit `flock` cross-process guard.
//! Tracked as a follow-up on ADR-0030 D4.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{ReferenceKind, RunSummary, TraceFilter, TraceStore, TraceStoreError};
use crate::metrics::MetricsEvent;

/// Reject a `run_id` that would be unsafe to use as a path component.
///
/// Centralised so `read` / `append` / `write_index_for_run` /
/// `mark_referenced` / `locate_run` enforce the same rules — a previous
/// version only validated on the write paths, which left a directory-
/// traversal hole on `read("../escape")`.
fn validate_run_id(run_id: &str) -> Result<(), TraceStoreError> {
    if run_id.is_empty() || run_id.contains(['/', '\\', '\0']) || run_id == "." || run_id == ".." {
        return Err(TraceStoreError::InvalidRunId(run_id.into()));
    }
    Ok(())
}

pub struct FileTraceStore {
    root: PathBuf,
    // In-process serialisation for every mutating path (`append`,
    // `write_index_for_run`, `mark_referenced`) AND for `prune` scans —
    // holding it across prune closes the read-modify-delete TOCTOU window
    // where a `mark_referenced` could land between the directory listing
    // and the unlink. See module-level doc for the cross-process story.
    write_lock: Mutex<()>,
    /// First-touch cache from `run_id` to its committed shard directory.
    /// Pins the directory chosen at the very first `append` (or recovered
    /// from disk via `locate_run`) so subsequent writes — events,
    /// `write_index_for_run`, `mark_referenced` — always land in the
    /// same directory even if the wall-clock crosses a month boundary
    /// mid-run, and even if `summary.started_at` is well before `now()`.
    /// Entries are evicted by `prune` when their run is removed.
    run_dirs: Mutex<HashMap<String, PathBuf>>,
}

impl FileTraceStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, TraceStoreError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            write_lock: Mutex::new(()),
            run_dirs: Mutex::new(HashMap::new()),
        })
    }

    fn shard_dir_for_time(&self, t: SystemTime) -> PathBuf {
        let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        let (year, month) = year_month_utc(secs);
        self.root.join(format!("{year:04}-{month:02}"))
    }

    /// Resolve (and cache) the shard directory for `run_id`.
    ///
    /// Decision order:
    /// 1. Already cached → return that.
    /// 2. An existing `.ndjson` is on disk → adopt its directory.
    /// 3. Fall back to `hint` (e.g. `now()` for `append`,
    ///    `summary.started_at` for `write_index_for_run`).
    ///
    /// The chosen directory is cached so all subsequent file ops on the
    /// run target the same place.
    fn resolve_shard_dir(&self, run_id: &str, hint: SystemTime) -> PathBuf {
        {
            let cache = self.run_dirs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(dir) = cache.get(run_id) {
                return dir.clone();
            }
        }
        let dir = self
            .scan_for_existing_dir(run_id)
            .unwrap_or_else(|| self.shard_dir_for_time(hint));
        let mut cache = self.run_dirs.lock().unwrap_or_else(|e| e.into_inner());
        cache.entry(run_id.to_string()).or_insert(dir.clone());
        dir
    }

    fn scan_for_existing_dir(&self, run_id: &str) -> Option<PathBuf> {
        if !self.root.exists() {
            return None;
        }
        let entries = fs::read_dir(&self.root).ok()?;
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let candidate = dir.join(format!("{run_id}.ndjson"));
            if candidate.exists() {
                return Some(dir);
            }
        }
        None
    }

    pub fn locate_run(&self, run_id: &str) -> Option<PathBuf> {
        validate_run_id(run_id).ok()?;
        {
            let cache = self.run_dirs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(dir) = cache.get(run_id) {
                let candidate = dir.join(format!("{run_id}.ndjson"));
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
        // Cache miss or stale entry — fall back to disk scan and re-cache.
        let dir = self.scan_for_existing_dir(run_id)?;
        let candidate = dir.join(format!("{run_id}.ndjson"));
        let mut cache = self.run_dirs.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(run_id.to_string(), dir);
        Some(candidate)
    }
}

/// Return whether `path` ends with a `\n` byte. Empty files return
/// `false`. Used by `read` to distinguish a partial trailing record
/// (writer crash mid-write) from a fully-terminated corrupt last line
/// (real corruption that must not be tolerated).
fn file_ends_with_newline(path: &std::path::Path) -> Result<bool, TraceStoreError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

fn year_month_utc(epoch_secs: i64) -> (i32, u32) {
    // Days from 1970-01-01 (Unix epoch).  Algorithm adapted from
    // Howard Hinnant's date math primitives (public domain).
    let days = (epoch_secs.div_euclid(86_400)) + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = days - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m)
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    run_id: String,
    agent_id: String,
    started_at_secs: u64,
    ended_at_secs: Option<u64>,
    prompt_ids: Vec<String>,
    experiment_id: Option<String>,
    variant_name: Option<String>,
    final_status: Option<String>,
    judge_score: Option<f32>,
}

impl IndexFile {
    fn from(s: &RunSummary) -> Self {
        Self {
            run_id: s.run_id.clone(),
            agent_id: s.agent_id.clone(),
            started_at_secs: s
                .started_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            ended_at_secs: s
                .ended_at
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            prompt_ids: s.prompt_ids.clone(),
            experiment_id: s.experiment_id.clone(),
            variant_name: s.variant_name.clone(),
            final_status: s.final_status.clone(),
            judge_score: s.judge_score,
        }
    }
    fn to_summary(&self) -> RunSummary {
        RunSummary {
            run_id: self.run_id.clone(),
            agent_id: self.agent_id.clone(),
            started_at: UNIX_EPOCH + std::time::Duration::from_secs(self.started_at_secs),
            ended_at: self
                .ended_at_secs
                .map(|s| UNIX_EPOCH + std::time::Duration::from_secs(s)),
            prompt_ids: self.prompt_ids.clone(),
            experiment_id: self.experiment_id.clone(),
            variant_name: self.variant_name.clone(),
            final_status: self.final_status.clone(),
            judge_score: self.judge_score,
        }
    }
}

impl TraceStore for FileTraceStore {
    fn append(&self, run_id: &str, event: &MetricsEvent) -> Result<(), TraceStoreError> {
        validate_run_id(run_id)?;
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.resolve_shard_dir(run_id, SystemTime::now());
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{run_id}.ndjson"));
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    fn read(&self, run_id: &str) -> Result<Vec<MetricsEvent>, TraceStoreError> {
        validate_run_id(run_id)?;
        let path = self
            .locate_run(run_id)
            .ok_or_else(|| TraceStoreError::NotFound {
                run_id: run_id.into(),
            })?;
        // Inspect the very last byte before parsing. A well-terminated
        // NDJSON file ends with `\n`; an interrupted append leaves the
        // last record without one. We tolerate a parse error on the
        // tail only in the second case — a newline-terminated corrupt
        // last line is real corruption, not a partial write, and must
        // surface as `TraceStoreError::Serde`.
        let trailing_newline = file_ends_with_newline(&path)?;
        let file = File::open(&path)?;
        let reader = BufReader::new(file);
        // Read with strict I/O semantics: a read error mid-file is a real
        // failure, not silent EOF (the previous `map_while(Result::ok)`
        // hid these).
        let mut lines: Vec<String> = Vec::new();
        for line in reader.lines() {
            let line = line?; // I/O error → propagate
            if !line.is_empty() {
                lines.push(line);
            }
        }
        let mut out = Vec::with_capacity(lines.len());
        for (idx, line) in lines.iter().enumerate() {
            match serde_json::from_str::<MetricsEvent>(line) {
                Ok(ev) => out.push(ev),
                Err(e) => {
                    let is_last = idx + 1 == lines.len();
                    if is_last && !trailing_newline {
                        // Genuine partial trailing record (writer crashed
                        // mid-write and never emitted the closing '\n').
                        // Tolerate.
                        break;
                    }
                    return Err(TraceStoreError::Serde(e));
                }
            }
        }
        Ok(out)
    }

    fn list(&self, filter: &TraceFilter) -> Result<Vec<RunSummary>, TraceStoreError> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for month_dir in fs::read_dir(&self.root)?.flatten() {
            let dir_path = month_dir.path();
            if !dir_path.is_dir() {
                continue;
            }
            for shard in fs::read_dir(&dir_path)?.flatten() {
                let p = shard.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if !p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with(".idx.json"))
                    .unwrap_or(false)
                {
                    continue;
                }
                let bytes = match fs::read(&p) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let idx: IndexFile = match serde_json::from_slice(&bytes) {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                let s = idx.to_summary();
                if let Some(a) = &filter.agent_id
                    && &s.agent_id != a
                {
                    continue;
                }
                if let Some(p) = &filter.prompt_id
                    && !s.prompt_ids.iter().any(|id| id == p)
                {
                    continue;
                }
                if let Some(e) = &filter.experiment_id
                    && s.experiment_id.as_deref() != Some(e.as_str())
                {
                    continue;
                }
                if let Some(v) = &filter.variant_name
                    && s.variant_name.as_deref() != Some(v.as_str())
                {
                    continue;
                }
                if let Some(since) = filter.since
                    && s.started_at < since
                {
                    continue;
                }
                out.push(s);
            }
        }
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    fn mark_referenced(&self, run_id: &str, by: ReferenceKind) -> Result<(), TraceStoreError> {
        validate_run_id(run_id)?;
        // Acquire `write_lock` before resolving the path so a concurrent
        // `prune` cannot delete the shard between locate and sentinel write.
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let p = self
            .locate_run(run_id)
            .ok_or_else(|| TraceStoreError::NotFound {
                run_id: run_id.into(),
            })?;
        let sentinel = p.with_extension("ref");
        let kind_byte: u8 = match by {
            ReferenceKind::Dataset => b'D',
            ReferenceKind::ExperimentEvidence => b'E',
            ReferenceKind::OperatorPin => b'P',
        };
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(sentinel)?;
        f.write_all(&[kind_byte, b'\n'])?;
        Ok(())
    }

    fn prune(
        &self,
        older_than: SystemTime,
        except_referenced: &HashSet<String>,
    ) -> Result<u64, TraceStoreError> {
        // Hold `write_lock` for the whole scan + delete cycle. Without it a
        // concurrent `mark_referenced` could land its sentinel between this
        // function's directory listing and the unlink, leaving a referenced
        // run silently deleted.
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut removed = 0u64;
        if !self.root.exists() {
            return Ok(0);
        }
        for month_dir in fs::read_dir(&self.root)?.flatten() {
            let dir_path = month_dir.path();
            if !dir_path.is_dir() {
                continue;
            }
            for shard in fs::read_dir(&dir_path)?.flatten() {
                let p = shard.path();
                if p.extension().and_then(|s| s.to_str()) != Some("ndjson") {
                    continue;
                }
                let run_id = match p.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                if except_referenced.contains(&run_id) {
                    continue;
                }
                // Also keep if a .ref sentinel exists.
                if p.with_extension("ref").exists() {
                    continue;
                }
                // Compare started_at from the index, falling back to file
                // mtime when the index is missing OR corrupt. Using
                // UNIX_EPOCH on a parse failure (the previous behaviour)
                // would silently mark every malformed index as
                // always-deletable — operator data loss on the happiest
                // schema-evolution path. Mtime is at least bounded by the
                // shard's actual write history.
                let idx = p.with_extension("idx.json");
                let mtime_fallback = || {
                    p.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH)
                };
                let started_at = match fs::read(&idx) {
                    Ok(bytes) => match serde_json::from_slice::<IndexFile>(&bytes) {
                        Ok(i) => UNIX_EPOCH + std::time::Duration::from_secs(i.started_at_secs),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                index = %idx.display(),
                                "TraceStore index parse failed; falling back to file mtime"
                            );
                            mtime_fallback()
                        }
                    },
                    Err(_) => mtime_fallback(),
                };
                if started_at < older_than {
                    let _ = fs::remove_file(&p);
                    let _ = fs::remove_file(&idx);
                    let _ = fs::remove_file(p.with_extension("ref"));
                    // Evict the cached run_id → dir mapping so future
                    // `append`s for a recycled id resolve afresh.
                    self.run_dirs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&run_id);
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    fn write_index_for_run(
        &self,
        run_id: &str,
        summary: &RunSummary,
    ) -> Result<(), TraceStoreError> {
        validate_run_id(run_id)?;
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Same shard directory the run's `.ndjson` already uses — keeps
        // index and events colocated even when `started_at` is from a
        // different month than the events were appended in (see
        // `resolve_shard_dir`'s cache-first logic).
        let dir = self.resolve_shard_dir(run_id, summary.started_at);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{run_id}.idx.json"));
        let json = serde_json::to_vec_pretty(&IndexFile::from(summary))?;
        fs::write(path, json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{GenAISpan, MetricsEvent, SpanContext};

    fn temp_root(name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("awaken-trace-{name}-{now}"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn span() -> GenAISpan {
        GenAISpan {
            context: SpanContext {
                run_id: "01HXTEST".into(),
                ..Default::default()
            },
            step_index: Some(0),
            model: "m".into(),
            provider: "p".into(),
            operation: "chat".into(),
            response_model: None,
            response_id: None,
            finish_reasons: vec![],
            error_type: None,
            error_class: None,
            input_tokens: Some(1),
            output_tokens: Some(2),
            total_tokens: Some(3),
            thinking_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: vec![],
            duration_ms: 0,
            started_at_ms: 0,
            ended_at_ms: 0,
        }
    }

    #[test]
    fn append_then_read_roundtrip() {
        let root = temp_root("rt");
        let store = FileTraceStore::new(&root).unwrap();
        store
            .append("run-1", &MetricsEvent::Inference(span()))
            .unwrap();
        store
            .append("run-1", &MetricsEvent::Inference(span()))
            .unwrap();
        let events = store.read("run-1").unwrap();
        assert_eq!(events.len(), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_unknown_run_returns_not_found() {
        let root = temp_root("nf");
        let store = FileTraceStore::new(&root).unwrap();
        let err = store.read("nope").unwrap_err();
        assert!(matches!(err, TraceStoreError::NotFound { .. }));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn append_rejects_traversal_run_id() {
        let root = temp_root("tx");
        let store = FileTraceStore::new(&root).unwrap();
        let err = store
            .append("../escape", &MetricsEvent::Inference(span()))
            .unwrap_err();
        assert!(matches!(err, TraceStoreError::InvalidRunId(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_tolerates_partial_trailing_line() {
        // A genuine partial trailing record is an interrupted append: the
        // writer crashed before emitting the closing `\n`. We simulate
        // that by appending a half-record WITHOUT a trailing newline.
        // F12 changed the semantics so that newline-terminated corrupt
        // lines surface as errors (see `read_surfaces_corrupt_terminated_last_line`)
        // — only the no-trailing-newline case is tolerated.
        let root = temp_root("partial");
        let store = FileTraceStore::new(&root).unwrap();
        store
            .append("rp", &MetricsEvent::Inference(span()))
            .unwrap();
        let p = store.locate_run("rp").unwrap();
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        // NOTE: no trailing '\n' — this is the crash-mid-write shape.
        f.write_all(b"{not-valid-json").unwrap();
        drop(f);
        let events = store.read("rp").unwrap();
        assert_eq!(events.len(), 1, "partial record must be dropped silently");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_surfaces_mid_file_corruption_as_error() {
        // Trailing-only tolerance: a parse error on a NON-last line must
        // surface, otherwise mid-file corruption is silently lost.
        let root = temp_root("mid-corrupt");
        let store = FileTraceStore::new(&root).unwrap();
        // First record (valid).
        store
            .append("rc", &MetricsEvent::Inference(span()))
            .unwrap();
        // Corrupt mid-file line (terminated with newline, so it is NOT the
        // trailing record).
        let p = store.locate_run("rc").unwrap();
        {
            let mut f = OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(b"{this-line-is-corrupt}\n").unwrap();
        }
        // Trailing record (valid).
        store
            .append("rc", &MetricsEvent::Inference(span()))
            .unwrap();

        let err = store.read("rc").unwrap_err();
        assert!(
            matches!(err, TraceStoreError::Serde(_)),
            "expected Serde error for mid-file corruption, got: {err:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn year_month_utc_known_dates() {
        // 2026-01-01 00:00:00 UTC = 1767225600
        assert_eq!(year_month_utc(1_767_225_600), (2026, 1));
        // 2024-02-29 00:00:00 UTC (leap day) = 1709164800
        assert_eq!(year_month_utc(1_709_164_800), (2024, 2));
    }

    #[test]
    fn list_returns_empty_on_empty_root() {
        let root = temp_root("list-empty");
        let store = FileTraceStore::new(&root).unwrap();
        let summaries = store.list(&TraceFilter::default()).unwrap();
        assert!(summaries.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_returns_one_per_run_after_index_written() {
        let root = temp_root("list-runs");
        let store = FileTraceStore::new(&root).unwrap();
        store.append("a", &MetricsEvent::Inference(span())).unwrap();
        store.append("b", &MetricsEvent::Inference(span())).unwrap();
        // Indexes are produced by `write_index_for_run`; call it directly here
        // (a private helper exposed to tests) to seed two summaries.
        store
            .write_index_for_run(
                "a",
                &RunSummary {
                    run_id: "a".into(),
                    agent_id: "weather".into(),
                    started_at: SystemTime::UNIX_EPOCH,
                    ended_at: None,
                    prompt_ids: vec![],
                    experiment_id: None,
                    variant_name: None,
                    final_status: None,
                    judge_score: None,
                },
            )
            .unwrap();
        store
            .write_index_for_run(
                "b",
                &RunSummary {
                    run_id: "b".into(),
                    agent_id: "other".into(),
                    started_at: SystemTime::UNIX_EPOCH,
                    ended_at: None,
                    prompt_ids: vec![],
                    experiment_id: None,
                    variant_name: None,
                    final_status: None,
                    judge_score: None,
                },
            )
            .unwrap();

        let all = store.list(&TraceFilter::default()).unwrap();
        assert_eq!(all.len(), 2);

        let filtered = store
            .list(&TraceFilter {
                agent_id: Some("weather".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].run_id, "a");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mark_referenced_creates_sentinel() {
        let root = temp_root("mr");
        let store = FileTraceStore::new(&root).unwrap();
        store
            .append("run-x", &MetricsEvent::Inference(span()))
            .unwrap();
        store
            .mark_referenced("run-x", ReferenceKind::Dataset)
            .unwrap();
        let p = store.locate_run("run-x").unwrap();
        let sentinel = p.with_extension("ref");
        assert!(sentinel.exists(), "ref sentinel should exist");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_skips_referenced_runs() {
        let root = temp_root("prune");
        let store = FileTraceStore::new(&root).unwrap();
        // Two old runs, only one referenced.
        store
            .append("keep", &MetricsEvent::Inference(span()))
            .unwrap();
        store
            .append("drop", &MetricsEvent::Inference(span()))
            .unwrap();
        store
            .write_index_for_run(
                "keep",
                &RunSummary {
                    run_id: "keep".into(),
                    agent_id: "a".into(),
                    started_at: SystemTime::UNIX_EPOCH,
                    ended_at: None,
                    prompt_ids: vec![],
                    experiment_id: None,
                    variant_name: None,
                    final_status: None,
                    judge_score: None,
                },
            )
            .unwrap();
        store
            .write_index_for_run(
                "drop",
                &RunSummary {
                    run_id: "drop".into(),
                    agent_id: "a".into(),
                    started_at: SystemTime::UNIX_EPOCH,
                    ended_at: None,
                    prompt_ids: vec![],
                    experiment_id: None,
                    variant_name: None,
                    final_status: None,
                    judge_score: None,
                },
            )
            .unwrap();

        let mut referenced = HashSet::new();
        referenced.insert("keep".to_string());
        let removed = store.prune(SystemTime::now(), &referenced).unwrap();
        assert_eq!(removed, 1);
        assert!(store.locate_run("keep").is_some());
        assert!(store.locate_run("drop").is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_surfaces_corrupt_terminated_last_line() {
        // F12: a newline-terminated corrupt last line is real corruption,
        // not a partial trailing record. Must surface as `Serde`.
        let root = temp_root("term-last");
        let store = FileTraceStore::new(&root).unwrap();
        store
            .append("rt", &MetricsEvent::Inference(span()))
            .unwrap();
        // Append a malformed line WITH trailing newline. This is what a
        // healthy writer would emit if it serialised a bad event — i.e.
        // not a crash artifact.
        let p = store.locate_run("rt").unwrap();
        {
            let mut f = OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(b"{not-valid-json}\n").unwrap();
        }
        let err = store.read("rt").unwrap_err();
        assert!(
            matches!(err, TraceStoreError::Serde(_)),
            "newline-terminated corrupt last line must error, got {err:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_falls_back_to_mtime_on_corrupt_index() {
        // F15: a corrupt `.idx.json` must NOT be treated as
        // started_at = UNIX_EPOCH — that would make every malformed
        // index always-deletable. Use file mtime instead so a recent
        // shard with a bad index survives a tight TTL.
        let root = temp_root("prune-bad-idx");
        let store = FileTraceStore::new(&root).unwrap();
        store
            .append("recent-bad-idx", &MetricsEvent::Inference(span()))
            .unwrap();
        // Locate the shard and drop a corrupt index next to it.
        let ndjson = store.locate_run("recent-bad-idx").unwrap();
        let idx = ndjson.with_extension("idx.json");
        std::fs::write(&idx, b"{not json").unwrap();

        // Aggressive cutoff: any UNIX_EPOCH fallback would delete this run.
        // mtime fallback (now) survives this cutoff.
        let cutoff = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        let removed = store.prune(cutoff, &HashSet::new()).unwrap();
        assert_eq!(
            removed, 0,
            "recent shard with corrupt index must NOT be deleted (mtime saves it)"
        );
        assert!(store.locate_run("recent-bad-idx").is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_rejects_traversal_run_id() {
        // Regression: prior `read` did not validate `run_id`, so a path-like
        // id could escape the trace root via `locate_run`'s scan.
        let root = temp_root("read-traversal");
        let store = FileTraceStore::new(&root).unwrap();
        let err = store.read("../escape").unwrap_err();
        assert!(matches!(err, TraceStoreError::InvalidRunId(_)));
        let err2 = store.read("..").unwrap_err();
        assert!(matches!(err2, TraceStoreError::InvalidRunId(_)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shard_dir_pinned_across_started_at_mismatch() {
        // Regression: `append` used `now()` for the shard, but
        // `write_index_for_run` used `summary.started_at`. A summary whose
        // `started_at` falls in a different month from the append would
        // land the index in a separate directory from the events. After
        // pinning, both must end up in the same shard.
        let root = temp_root("shard-pin");
        let store = FileTraceStore::new(&root).unwrap();
        store
            .append("01HXPINRUN", &MetricsEvent::Inference(span()))
            .unwrap();

        // Summary with a started_at from 2010 — different year than now.
        let stale_started = UNIX_EPOCH + std::time::Duration::from_secs(1_262_304_000);
        store
            .write_index_for_run(
                "01HXPINRUN",
                &RunSummary {
                    run_id: "01HXPINRUN".into(),
                    agent_id: "a".into(),
                    started_at: stale_started,
                    ended_at: None,
                    prompt_ids: vec![],
                    experiment_id: None,
                    variant_name: None,
                    final_status: None,
                    judge_score: None,
                },
            )
            .unwrap();

        // Locate the ndjson; the idx.json must sit beside it.
        let ndjson = store.locate_run("01HXPINRUN").unwrap();
        let idx = ndjson.with_extension("idx.json");
        assert!(
            idx.exists(),
            "index file must colocate with ndjson, even when summary.started_at \
             pre-dates the run's actual append time"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
