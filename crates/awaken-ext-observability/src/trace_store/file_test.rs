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
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
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
