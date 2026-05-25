//! Micro-benchmarks for the in-memory mailbox dispatch hot path.
//!
//! Measures the full success lifecycle (enqueue → claim → ack) and the error
//! lifecycle (enqueue → claim → dead_letter) that the LLM error-classification
//! fix exercises, giving a regression baseline for the per-dispatch cost.
//!
//! Run with `cargo bench -p awaken-stores`. Measurement only — there are no
//! pass/fail thresholds (timing assertions are flaky on shared CI runners).

use awaken_contract::contract::mailbox::{MailboxStore, RunDispatch, RunDispatchStatus};
use awaken_stores::InMemoryMailboxStore;
use criterion::{Criterion, criterion_group, criterion_main};

fn dispatch(id: &str, thread_id: &str) -> RunDispatch {
    RunDispatch {
        dispatch_id: id.to_string(),
        thread_id: thread_id.to_string(),
        run_id: format!("{id}-run"),
        priority: 128,
        dedupe_key: None,
        dispatch_epoch: 0,
        status: RunDispatchStatus::Queued,
        available_at: 0,
        attempt_count: 0,
        max_attempts: 3,
        last_error: None,
        claim_token: None,
        claimed_by: None,
        lease_until: None,
        dispatch_instance_id: None,
        run_status: None,
        termination: None,
        run_response: None,
        run_error: None,
        completed_at: None,
        created_at: 0,
        updated_at: 0,
    }
}

fn bench_mailbox_lifecycle(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Successful dispatch lifecycle: enqueue → claim → ack.
    c.bench_function("mailbox_enqueue_claim_ack", |b| {
        b.to_async(&rt).iter(|| async {
            let store = InMemoryMailboxStore::new();
            store.enqueue(&dispatch("d", "t")).await.unwrap();
            let claimed = store.claim("t", "consumer", 30_000, 1, 1).await.unwrap();
            let token = claimed[0].claim_token.clone().unwrap();
            store.ack("d", &token, 2).await.unwrap();
        });
    });

    // Error dispatch lifecycle: enqueue → claim → dead_letter (the permanent
    // LLM-error path the classification fix routes through).
    c.bench_function("mailbox_enqueue_claim_dead_letter", |b| {
        b.to_async(&rt).iter(|| async {
            let store = InMemoryMailboxStore::new();
            store.enqueue(&dispatch("d", "t")).await.unwrap();
            let claimed = store.claim("t", "consumer", 30_000, 1, 1).await.unwrap();
            let token = claimed[0].claim_token.clone().unwrap();
            store
                .dead_letter("d", &token, "permanent: 403 quota", 2)
                .await
                .unwrap();
        });
    });
}

criterion_group!(benches, bench_mailbox_lifecycle);
criterion_main!(benches);
