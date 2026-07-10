//! B-P3 (ADR-0021 §6): the run↔sandbox binding is durable across a claim, so crash
//! recovery re-adopts the same sandbox. Verifies the memory backend directly and,
//! gated on `AWAKEN_TEST_PG_URL`, the real Postgres backend (which also proves the
//! V0011 `sandbox` column migration applies).

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress::MemoryDispatchStore;
use awaken_run_ingress_contract::dispatch::DispatchQueue;
use awaken_run_ingress_contract::request::RunExecutionRequest;
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
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fp.clone(),
                instructions: String::new(),
                max_steps: 8,
                model_binding: ModelBinding {
                    provider_instance_ref: "p".into(),
                    model_ref: "m".into(),
                    backend_ref: "b".into(),
                },
                tool_descriptors: vec![],
                plugin_ids: vec![],
                plugin_config: Default::default(),
                context_policy: ContextPolicy::default(),
            },
            fingerprint: fp,
        },
        input: vec![],
        trace: Default::default(),
    }
}

fn req(run: &str, thread: &str) -> RunExecutionRequest {
    RunExecutionRequest::new(activation(run, thread))
}

/// enqueue → claim (unbound) → bind_sandbox → the lease expires and a recovery
/// re-claim returns the SAME sandbox binding.
async fn binding_survives_a_recovery_claim(store: &dyn DispatchQueue) {
    let run = RunId("run-1".into());
    store.enqueue(req("run-1", "thread-1")).await.unwrap();

    // First claim: no sandbox yet.
    let first = store.claim("worker-a", 1_000, 0).await.unwrap().unwrap();
    assert_eq!(first.lease.run_id, run);
    assert_eq!(first.sandbox, None, "unbound before placement");

    // The fleet places the run on a sandbox and records the opaque handle.
    store.bind_sandbox(&run, "docker:abc123").await.unwrap();

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
}

#[tokio::test]
async fn memory_backend_binds_and_recovers() {
    binding_survives_a_recovery_claim(&MemoryDispatchStore::new()).await;
}

#[tokio::test]
async fn postgres_backend_binds_and_recovers() {
    let Ok(url) = std::env::var("AWAKEN_TEST_PG_URL") else {
        eprintln!("skip: AWAKEN_TEST_PG_URL unset");
        return;
    };
    let store = awaken_run_ingress::PostgresDispatchStore::connect(&url)
        .await
        .expect("connect + migrate (incl. V0011 sandbox column)");
    binding_survives_a_recovery_claim(&store).await;
}
