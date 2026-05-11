//! ADR-0030 D6: retention loop deterministic test.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_ext_observability::trace_store::file::FileTraceStore;
use awaken_ext_observability::trace_store::{ReferenceKind, RunSummary, TraceStore};
use awaken_ext_observability::{GenAISpan, MetricsEvent, SpanContext};
use awaken_server::services::trace_retention::{RetentionConfig, spawn_retention_loop};

// ── Helpers ───────────────────────────────────────────────────────────────

fn temp_trace_dir() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("awaken-retention-test-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn sample_event(run_id: &str) -> MetricsEvent {
    MetricsEvent::Inference(GenAISpan {
        context: SpanContext {
            run_id: run_id.to_string(),
            agent_id: "test-agent".to_string(),
            ..SpanContext::default()
        },
        step_index: None,
        model: "test-model".to_string(),
        provider: "test-provider".to_string(),
        operation: "chat".to_string(),
        response_model: None,
        response_id: None,
        finish_reasons: vec![],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: vec![],
        duration_ms: 100,
        started_at_ms: 0,
        ended_at_ms: 0,
    })
}

/// Seed a run in the past so `prune(ttl=0)` removes it immediately.
fn seed_old_run(store: &FileTraceStore, run_id: &str) {
    // Append the event (uses SystemTime::now() for the shard dir).
    store.append(run_id, &sample_event(run_id)).unwrap();

    // Write an index with a started_at far in the past so that even a
    // ttl=0 cutoff (= SystemTime::now()) finds it eligible.
    let old_time = UNIX_EPOCH + Duration::from_secs(1_000_000);
    let summary = RunSummary {
        run_id: run_id.to_string(),
        agent_id: "test-agent".to_string(),
        started_at: old_time,
        ended_at: Some(old_time + Duration::from_secs(1)),
        prompt_ids: vec![],
        experiment_id: None,
        variant_name: None,
        final_status: None,
        judge_score: None,
    };
    store.write_index_for_run(run_id, &summary).unwrap();
}

// ── Tests ─────────────────────────────────────────────────────────────────

/// Verify that the loop calls prune on a tick:
/// - unreferenced old run is removed
/// - referenced old run survives (via .ref sentinel from mark_referenced)
#[tokio::test]
async fn retention_loop_prunes_unreferenced_and_keeps_referenced() {
    let dir = temp_trace_dir();
    let store = Arc::new(FileTraceStore::new(&dir).expect("FileTraceStore::new"));

    let unreferenced_id = "run-unreferenced-0001";
    let referenced_id = "run-referenced-0002";

    seed_old_run(&store, unreferenced_id);
    seed_old_run(&store, referenced_id);

    // Mark the second run as referenced via the sentinel file mechanism.
    store
        .mark_referenced(referenced_id, ReferenceKind::OperatorPin)
        .unwrap();

    // Spawn the retention loop with ttl=0 so everything old is eligible.
    let config = RetentionConfig {
        ttl: Duration::from_secs(0),
        // Long interval — we drive it manually via tick_now.
        interval: Duration::from_secs(3600),
    };
    let handle = spawn_retention_loop(store.clone() as Arc<dyn TraceStore>, config);

    // Trigger a prune tick and block until the loop signals completion.
    // Avoids the scheduling-race fragility of an arbitrary `sleep`.
    handle.tick_now_and_wait().await;

    // Unreferenced run must be gone.
    let err = store.read(unreferenced_id);
    assert!(
        err.is_err(),
        "unreferenced old run must be pruned; read returned Ok"
    );

    // Referenced run must survive.
    let events = store
        .read(referenced_id)
        .expect("referenced run must survive prune");
    assert!(!events.is_empty(), "referenced run must still have events");
}

/// Direct prune-logic test (no loop): validates that prune(now, empty set)
/// removes the unreferenced run and that mark_referenced protects the other.
#[test]
fn prune_logic_directly_removes_unreferenced_keeps_referenced() {
    let dir = temp_trace_dir();
    let store = FileTraceStore::new(&dir).expect("FileTraceStore::new");

    let unreferenced_id = "run-direct-unref";
    let referenced_id = "run-direct-ref";

    seed_old_run(&store, unreferenced_id);
    seed_old_run(&store, referenced_id);

    store
        .mark_referenced(referenced_id, ReferenceKind::OperatorPin)
        .unwrap();

    let cutoff = SystemTime::now();
    let removed = store.prune(cutoff, &HashSet::new()).unwrap();

    assert_eq!(removed, 1, "exactly one shard should have been pruned");

    assert!(
        store.read(unreferenced_id).is_err(),
        "unreferenced run must be gone after prune"
    );
    assert!(
        store.read(referenced_id).is_ok(),
        "referenced run must survive prune"
    );
}
