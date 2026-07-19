//! Reusable behavioural conformance for durable dispatch implementations.
//!
//! Backends and decorators call these functions from their own integration tests.
//! The suite intentionally depends only on the public contract: passing it proves a
//! wrapper preserves the worker-visible claim, fencing, and recovery semantics rather
//! than merely delegating a convenient subset of methods.

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_ingress_contract::RunDispatch;
use awaken_run_ingress_contract::dispatch::{
    DispatchOutcome, DispatchQueue, PendingInput, RunClaim, SettleOutcome,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

const LEASE_MS: u64 = 1_000;

/// Capabilities which are deliberately absent from a database-less worker transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConformanceCapabilities {
    /// The adapter can hold a backend-local commit epoch guard.
    pub local_commit_guard: bool,
    /// The adapter persists and returns opaque sandbox bindings.
    pub sandbox_binding: bool,
}

impl ConformanceCapabilities {
    /// Full store/decorator behaviour.
    pub const LOCAL_STORE: Self = Self {
        local_commit_guard: true,
        sandbox_binding: true,
    };

    /// Worker-visible HTTP behaviour. Atomic commit is tested through the separate
    /// claimed-commit service; a worker cannot acquire a server-local row guard.
    pub const WORKER_TRANSPORT: Self = Self {
        local_commit_guard: false,
        sandbox_binding: false,
    };
}

/// Run the shared dispatch suite against one fresh, isolated implementation.
///
/// `namespace` is incorporated into every id so a PostgreSQL schema can safely run
/// this alongside other suites. The caller must still provide a store with no live
/// rows using that namespace.
pub async fn assert_dispatch_conformance(
    store: &dyn DispatchQueue,
    namespace: &str,
    capabilities: ConformanceCapabilities,
) {
    exact_claim_recovery_and_fencing(store, namespace).await;
    parent_mediated_commands_are_atomic(store, namespace).await;
    current_claim_guard_is_exact(store, namespace, capabilities).await;
    sandbox_binding_survives_recovery(store, namespace, capabilities).await;
}

async fn exact_claim_recovery_and_fencing(store: &dyn DispatchQueue, ns: &str) {
    let target = run_id(ns, "target");
    let unrelated = run_id(ns, "unrelated");
    store
        .enqueue(dispatch(ns, "unrelated", "unrelated-thread"))
        .await
        .expect("enqueue unrelated conformance run");
    store
        .enqueue(dispatch(ns, "target", "target-thread"))
        .await
        .expect("enqueue exact-claim target");

    let first = store
        .claim_run(&target, "conformance-a", LEASE_MS, 0)
        .await
        .expect("exact claim succeeds")
        .expect("target is runnable");
    assert_eq!(first.request.run_id(), &target);
    assert_eq!(first.lease.owner, "conformance-a");
    assert!(first.lease.epoch > 0, "a claimed lease has a fence epoch");

    let other = store
        .claim("conformance-pool", LEASE_MS, 0)
        .await
        .expect("general claim succeeds")
        .expect("exact claim leaves unrelated work available");
    assert_eq!(other.request.run_id(), &unrelated);
    assert_eq!(
        store
            .settle(&unrelated, other.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("settle unrelated"),
        SettleOutcome::Applied
    );

    assert!(
        store
            .claim_run(&target, "conformance-b", LEASE_MS, LEASE_MS)
            .await
            .expect("live-boundary claim")
            .is_none(),
        "a lease remains live at its exact expiry boundary"
    );
    let recovered = store
        .claim_run(&target, "conformance-b", LEASE_MS, LEASE_MS + 1)
        .await
        .expect("recovery claim succeeds")
        .expect("expired target is recoverable");
    assert_eq!(recovered.lease.epoch, first.lease.epoch + 1);
    assert_eq!(
        store
            .settle(&target, first.lease.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("stale settle returns a verdict"),
        SettleOutcome::Fenced,
        "the old epoch cannot settle a recovered run"
    );
    assert_eq!(
        store
            .settle(&target, recovered.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("current settle"),
        SettleOutcome::Applied
    );
}

async fn parent_mediated_commands_are_atomic(store: &dyn DispatchQueue, ns: &str) {
    let child = run_id(ns, "child");
    let claimed = store
        .claim_new_run(
            dispatch(ns, "child", "child-thread"),
            "conformance-parent",
            LEASE_MS,
            10_000,
        )
        .await
        .expect("claim_new_run succeeds")
        .expect("new child is returned already claimed");
    assert_eq!(claimed.request.run_id(), &child);
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, 10_000)
            .await
            .expect("pool claim after atomic admission")
            .is_none(),
        "the pool cannot interleave between child enqueue and exact claim"
    );
    assert_eq!(
        store
            .settle(&child, claimed.lease.epoch, DispatchOutcome::Awaiting, &[],)
            .await
            .expect("child enters awaiting"),
        SettleOutcome::Applied
    );

    let message_id = format!("{ns}-child-answer");
    let resumed = store
        .deliver_and_claim(
            PendingInput {
                message_id: message_id.clone(),
                run_id: child.clone(),
                thread_id: thread_id(ns, "child-thread"),
                correlation_id: format!("{ns}-approval"),
                available_at_ms: None,
                result: ResumeResult::Input("approved".to_string()),
            },
            "conformance-parent",
            LEASE_MS,
            10_001,
        )
        .await
        .expect("deliver_and_claim succeeds")
        .expect("awaiting child is returned already claimed");
    assert_eq!(resumed.pending.len(), 1);
    assert_eq!(resumed.pending[0].message_id, message_id);
    assert!(
        store
            .claim("conformance-pool", LEASE_MS, 10_001)
            .await
            .expect("pool claim after atomic delivery")
            .is_none(),
        "the pool cannot interleave between input delivery and exact claim"
    );
    assert_eq!(
        store
            .settle(
                &child,
                resumed.lease.epoch,
                DispatchOutcome::Done,
                &[message_id],
            )
            .await
            .expect("finish child"),
        SettleOutcome::Applied
    );
}

async fn current_claim_guard_is_exact(
    store: &dyn DispatchQueue,
    ns: &str,
    capabilities: ConformanceCapabilities,
) {
    if !capabilities.local_commit_guard {
        return;
    }
    let run = run_id(ns, "guard");
    store
        .enqueue(dispatch(ns, "guard", "guard-thread"))
        .await
        .expect("enqueue guard run");
    let claimed = store
        .claim_run(&run, "conformance-guard", LEASE_MS, 20_000)
        .await
        .expect("claim guard run")
        .expect("guard run is runnable");
    let current = RunClaim::from(&claimed.lease);
    let stale = RunClaim {
        epoch: current.epoch.saturating_sub(1),
        ..current.clone()
    };
    assert!(
        store
            .lock_commit_epoch(&stale)
            .await
            .expect("stale guard lookup")
            .is_none(),
        "a stale epoch never acquires commit authority"
    );
    let guard = store
        .lock_commit_epoch(&current)
        .await
        .expect("current guard lookup")
        .expect("the exact current claim acquires authority");
    drop(guard);
    assert_eq!(
        store
            .settle(&run, current.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("settle guarded run"),
        SettleOutcome::Applied
    );
}

async fn sandbox_binding_survives_recovery(
    store: &dyn DispatchQueue,
    ns: &str,
    capabilities: ConformanceCapabilities,
) {
    if !capabilities.sandbox_binding {
        return;
    }
    let run = run_id(ns, "sandbox");
    store
        .enqueue(dispatch(ns, "sandbox", "sandbox-thread"))
        .await
        .expect("enqueue sandbox run");
    let first = store
        .claim_run(&run, "conformance-sandbox-a", LEASE_MS, 30_000)
        .await
        .expect("claim sandbox run")
        .expect("sandbox run is runnable");
    let sandbox_ref = format!("opaque:{ns}");
    store
        .bind_sandbox(&run, &sandbox_ref)
        .await
        .expect("bind sandbox");
    let recovered = store
        .claim_run(
            &run,
            "conformance-sandbox-b",
            LEASE_MS,
            first.lease.expires_ms + 1,
        )
        .await
        .expect("recover sandbox run")
        .expect("expired sandbox run is recoverable");
    assert_eq!(recovered.sandbox.as_deref(), Some(sandbox_ref.as_str()));
    assert_eq!(
        store
            .settle(&run, recovered.lease.epoch, DispatchOutcome::Done, &[],)
            .await
            .expect("settle sandbox run"),
        SettleOutcome::Applied
    );
}

fn dispatch(ns: &str, run: &str, thread: &str) -> RunDispatch {
    RunDispatch::new(RunActivation::new(
        run_id(ns, run),
        thread_id(ns, thread),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId(format!("{ns}-snapshot")),
            root_agent_id: AgentId(format!("{ns}-agent")),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint(format!("{ns}-fingerprint")),
                instructions: "conformance".to_string(),
                max_steps: 4,
                delegation_limits: Default::default(),
                model_binding: ModelBinding::new("provider", "model", "backend"),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint(format!("{ns}-fingerprint")),
        },
        vec![Message::text(
            MessageId(format!("{ns}-{run}-input")),
            Role::User,
            "run conformance",
        )],
    ))
}

fn run_id(ns: &str, suffix: &str) -> RunId {
    RunId(format!("{ns}-{suffix}"))
}

fn thread_id(ns: &str, suffix: &str) -> ThreadId {
    ThreadId(format!("{ns}-{suffix}"))
}
