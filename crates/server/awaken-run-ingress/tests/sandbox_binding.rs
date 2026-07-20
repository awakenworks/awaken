//! B-P3 (ADR-0021 §6): the run↔sandbox binding is durable across a claim, so crash
//! recovery CAN re-adopt the same sandbox. Verifies the memory backend directly and,
//! gated on `AWAKEN_TEST_DATABASE_URL`, the real Postgres backend (which also proves
//! the V0011 `sandbox` column migration applies).
//!
//! SCOPE — this is the STORE-layer half. The production pool now forwards the whole
//! `Claimed` value to its host adapter; the runtime host persists a first placement
//! before execution and validates/adopts this handle on recovery. Host tests cover
//! that adapter and prove a replacement host over the same durable Workdir root sees
//! the same files. This test stays focused on backend conformance.
//!
//! A local provider still cannot reattach an OS child owned by a dead process; its
//! `process(pid)` remains explicitly unsupported. Shared container/k8s providers plug
//! into the same neutral handle/adopt/process/lease ports and may provide true process
//! reattachment without changing the dispatch aggregate.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::{AnyDispatchStore, MemoryDispatchStore};
use awaken_run_ingress_contract::RunDispatch;
use awaken_run_ingress_contract::dispatch::{
    DispatchOutcome, DispatchQueue, RunClaim, SettleOutcome,
};
use awaken_runtime_contract::activation::RunActivation;

fn activation(run: &str, thread: &str) -> RunActivation {
    use awaken_runtime_contract::resolved::{
        CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
    };
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    let fp = CatalogFingerprint("fp".into());
    RunActivation {
        run_id: RunId(run.into()),
        thread_id: ThreadId(thread.into()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fp.clone(),
                instructions: String::new(),
                max_steps: 8,
                delegation_limits: Default::default(),
                model_binding: ModelBinding {
                    provider_identity_ref: "p".into(),
                    model_ref: "m".into(),
                    backend_ref: "b".into(),
                },
                tool_descriptors: vec![],
                plugin_ids: vec![],
                plugin_config: Default::default(),
                context_policy: ContextPolicy::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: fp,
        },
        input: vec![],
        delegation_origin: None,
        model_ref_override: None,
    }
}

fn req(run: &str, thread: &str) -> RunDispatch {
    RunDispatch::new(activation(run, thread))
}

/// enqueue → claim (unbound) → bind_sandbox → the lease expires and a recovery
/// re-claim returns the SAME sandbox binding.
async fn binding_survives_a_recovery_claim(store: &dyn DispatchQueue) {
    let run = RunId("run-1".into());
    // Settle any leftover run-1 from a prior run on a shared schema (idempotent), so
    // single-writer-per-thread (ADR-0022) does not see a stale in-flight run here. A
    // leftover un-reclaimed row is at epoch 1 (claimed once by the crashed prior
    // run); a clean schema has no row and the settle is a benign fenced no-op.
    let _ = store.settle(&run, 1, DispatchOutcome::Done, &[]).await;
    store.enqueue(req("run-1", "thread-1")).await.unwrap();

    // First claim: no sandbox yet.
    let first = store.claim("worker-a", 1_000, 0).await.unwrap().unwrap();
    assert_eq!(first.lease.run_id, run);
    assert_eq!(first.sandbox, None, "unbound before placement");

    // The fleet places the run on a sandbox and records the opaque handle.
    assert_eq!(
        store
            .bind_sandbox(&RunClaim::from(&first.lease), "docker:abc123")
            .await
            .unwrap(),
        SettleOutcome::Applied
    );

    // The lease expires (worker-a crashed); a recovery claim re-adopts the SAME
    // sandbox — the binding is durable, so no sandbox is leaked.
    let recovered = store
        .claim("worker-b", 1_000, 5_000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.lease.run_id, run);
    assert_eq!(
        recovered.sandbox.as_deref(),
        Some("docker:abc123"),
        "the sandbox binding survives crash recovery"
    );
    assert_eq!(
        store
            .bind_sandbox(&RunClaim::from(&first.lease), "docker:stale")
            .await
            .unwrap(),
        SettleOutcome::Fenced,
        "the expired claim cannot overwrite the replacement's binding"
    );
    // Clean up so a re-run on a shared schema starts fresh. The recovery re-claim
    // holds the current fence epoch, so settle under it.
    let _ = store
        .settle(&run, recovered.lease.epoch, DispatchOutcome::Done, &[])
        .await;
}

#[tokio::test]
async fn memory_backend_binds_and_recovers() {
    binding_survives_a_recovery_claim(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn runtime_selected_store_delegates_the_sandbox_binding() {
    let store = AnyDispatchStore::open_sqlite_in_memory().expect("sqlite adapter");
    binding_survives_a_recovery_claim(&store).await;
}

#[tokio::test]
async fn postgres_backend_binds_and_recovers() {
    // The standard test-DB var every other pg suite uses (`AWAKEN_TEST_DATABASE_URL`).
    // It previously read a bespoke `AWAKEN_TEST_PG_URL`, so it self-skipped even under
    // the pg CI harness — a false green for the durable-binding guarantee.
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        eprintln!("skip: AWAKEN_TEST_DATABASE_URL unset");
        return;
    };
    let store = awaken_run_ingress::PostgresDispatchStore::connect(&url)
        .await
        .expect("connect + migrate (incl. V0011 sandbox column)");
    binding_survives_a_recovery_claim(&store).await;
}
