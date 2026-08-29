//! [`HostWorkerResolver`]: routes a claimed dispatch to the worker that owns
//! its thread, opening (or reusing) the session through the host.

use super::*;
mod claimed_dispatch;
mod claimed_session;
#[cfg(test)]
mod dispatched_mcp_tests;
mod resolver;
mod session_realization;
#[cfg(test)]
pub(super) mod test_support;
use claimed_session::install_claimed_session_projection;
pub(crate) use resolver::HostWorkerResolver;

#[cfg(test)]
mod tests {
    use super::claimed_dispatch::adopt_bound_sandbox;
    use super::test_support::{
        AdoptionModel, ToggleBindingSink, claim, deferred_environment, prepare_deferred_session,
        test_activation,
    };
    use super::*;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshotId};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test]
    async fn session_run_reservation_repair_skips_execution_realization() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Cause/effect graph: C1 a complete self-affine Session Run reservation
        // expires; C2 the queue grants its dedicated recovery claim; C3 no
        // Session Environment has been realized. Effects: E1 the claim is marked
        // admission-only; E2 Host resolves the existing environment-free boundary
        // Worker; E3 no Session Environment or Sandbox side effect is created.
        // Decision rule R1=C1+C2+C3=>E1+E2+E3. Ordinary and cancellation claims
        // retain their existing resolver tables in this module.
        use awaken_run_ingress::{Clock, DispatchQueue};

        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let session_id = "reservation-repair";
        let run_id = "reservation-repair-run";
        let activation = test_activation(session_id, run_id);
        let fingerprint = awaken_session_contract::SessionRunCommandFingerprint::current(
            &awaken_session_contract::AdmitSessionRun {
                session_id: session_id.into(),
                agent_id: activation.snapshot.root_agent_id.0.clone(),
                operation_id: "reservation-repair-operation".into(),
                run_id: activation.run_id.clone(),
                messages: activation.input.clone(),
                data_subject_id: None,
                traceparent: None,
                execution_requirements: Default::default(),
                replacement: Default::default(),
            },
        );
        let request = awaken_run_ingress::RunDispatch::new(activation)
            .for_session(awaken_agent_contract::agent::thread::Id(session_id.into()))
            .with_session_command_fingerprint(fingerprint);
        assert_eq!(
            store
                .reserve_session_run(request, 1)
                .await
                .expect("R1 reserve"),
            awaken_run_ingress::SessionRunReservationOutcome::Reserved,
            "R1/C1"
        );
        // C1 is governed by the queue-owned clock. Wait past the relative TTL,
        // then sample the edge clock used by the existing execution-lease claim.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let repair_now = awaken_run_ingress::SystemClock.now_ms();
        let claimed = store
            .claim("repair-worker", 1_000, repair_now, &Default::default())
            .await
            .expect("R1 recovery claim")
            .expect("R1 expired reservation is claimable");
        assert!(claimed.session_activity_admission_required, "R1/E1");

        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_store_dir(storage.path())
                .with_dispatch_store(store),
        );
        HostWorkerResolver {
            host: Arc::downgrade(&host),
        }
        .worker_for_claimed(&claimed)
        .await
        .expect("R1/E2 environment-free worker");
        assert!(
            host.session_environment(session_id).await.is_none(),
            "R1/E3"
        );
    }

    #[tokio::test]
    async fn ephemeral_parent_routes_a_coordinated_child_through_the_boundary_worker() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 the local Runtime authority is ephemeral; C2 the
        // parent Session therefore uses direct foreground ingress; C3 an ordinary
        // child Run is claimed from that same authority's dispatch queue with
        // parent affinity. Effects: E1 resolution reuses the parent commit,
        // Environment, and attempt context; E2 it constructs the existing child
        // boundary Worker without requiring a second per-Session durable ingress.
        // The durable-parent sibling paths are covered by cold-worker recovery.
        //
        // | Rule | Authority | Parent ingress | Claimed Thread | Effect |
        // |---|---|---|---|---|
        // | E1 | ephemeral | direct | child != parent | E1+E2 |
        use awaken_run_ingress::{DispatchQueue as _, WorkerResolver as _};

        let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
        let parent = awaken_agent_contract::agent::thread::Id("ephemeral-parent".into());
        let child = awaken_agent_contract::agent::thread::Id("ephemeral-child".into());
        let run = awaken_agent_contract::agent::run::Id("ephemeral-child-run".into());
        let parent_ctx = host
            .ctx_for(&parent.0, None)
            .await
            .expect("E1 direct parent context");
        assert!(parent_ctx.durable_ingress.is_none(), "E1 precondition");
        let store = host.dispatch_store().expect("ephemeral dispatch authority");
        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(&child.0, &run.0))
                    .for_session(parent.clone())
                    .with_session_activity_epoch(7),
            )
            .await
            .expect("E1 child admission");
        let claimed = store
            .claim("ephemeral-worker", 1_000, 0, &Default::default())
            .await
            .expect("E1 claim")
            .expect("E1 child work");
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("E1/E2 child boundary resolution");
    }

    #[tokio::test]
    async fn claimed_root_worker_is_independent_from_foreground_delivery_durability() {
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 foreground delivery is direct or durable; C2
        // this process is a local execution owner or coordinator-only; C3 a root
        // continuation has already been accepted into the dispatch queue.
        // Effects: E1 direct foreground remains non-durable; E2 a local direct
        // Session exposes and executes through its canonical claimed Worker; E3
        // durable foreground and claim resolution share the exact Worker `Arc`;
        // E4 coordinator-only resolution rejects before constructing a context.
        // Constraint: child-affinity construction is distinct and remains owned
        // by `ephemeral_parent_routes_a_coordinated_child_through_the_boundary_worker`.
        //
        // | Rule | Foreground | Local owner | Claimed root | Effect |
        // |---|---|---|---|---|
        // | R1 | direct | yes | yes | E1+E2 |
        // | R2 | durable | yes | no | E3 |
        // | R3 | durable | no | yes | E4 |
        use awaken_run_ingress::{Clock as _, DispatchQueue as _, WorkerResolver as _};

        let ephemeral = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
        let coordination: Arc<dyn awaken_session_contract::SessionAgentCoordination> =
            Arc::new(crate::coordination::RecordingSessionAgentCoordination::default());
        crate::ManagedHost::new(ephemeral.clone())
            .install_agent_coordination_application(Arc::downgrade(&coordination))
            .expect("R1 coordination authority");
        let session_id = "ephemeral-report-root";
        let direct = ephemeral
            .ctx_for(session_id, None)
            .await
            .expect("R1 direct context");
        assert!(!direct.durable, "R1/E1");
        assert!(direct.durable_ingress.is_none(), "R1/E1");

        let child_run = RunId("reported-child-run".into());
        let report_id = MessageId::agent_thread_report(&child_run);
        ephemeral
            .continue_session_agent_report(
                awaken_session_contract::SessionAgentReportContinuation {
                    session_id: session_id.into(),
                    source_thread_id: ThreadId("reported-child-thread".into()),
                    source_run_id: child_run,
                    session_activity_epoch: 7,
                    message: Message::text(report_id.clone(), Role::User, "child report"),
                },
            )
            .await
            .expect("R1 queue root continuation");
        let ephemeral_store = ephemeral.dispatch_store().expect("R1 dispatch authority");
        let now = awaken_run_ingress::SystemClock.now_ms();
        let claimed = ephemeral_store
            .claim(
                ephemeral.dispatch_owner(),
                awaken_run_ingress::DEFAULT_LEASE_MS,
                now,
                &Default::default(),
            )
            .await
            .expect("R1 claim root continuation")
            .expect("R1 root continuation is runnable");
        let ephemeral_resolver = HostWorkerResolver {
            host: Arc::downgrade(&ephemeral),
        };
        let worker = ephemeral_resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("R1/E2 resolve direct Session claim");
        let resident = ephemeral
            .session_slots
            .read(session_id, |slot| slot.runtime.clone())
            .flatten()
            .expect("R1 resident Session context");
        assert!(Arc::ptr_eq(&worker, &resident.claimed_worker), "R1/E2");
        let (_, state) = worker
            .drive_claimed(claimed, Arc::new(awaken_run_ingress::ManualClock::new(now)))
            .await
            .expect("R1/E2 execute root continuation")
            .expect("R1/E2 root continuation settles");
        assert!(matches!(state, RunState::Ended(_)), "R1/E2");
        assert!(
            ephemeral
                .committed_messages(session_id)
                .await
                .expect("R1 committed transcript")
                .iter()
                .any(|message| message.id == report_id),
            "R1/E2 report reached the root Run"
        );

        let durable_store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                .expect("R2 dispatch store"),
        );
        let durable = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(durable_store),
        );
        let durable_thread = ThreadId("durable-claimed-root".into());
        let durable_ctx = durable
            .ctx_for(&durable_thread.0, None)
            .await
            .expect("R2 durable context");
        let durable_ingress = durable_ctx
            .durable_ingress
            .as_ref()
            .expect("R2 durable foreground ingress");
        assert!(durable_ctx.durable, "R2 precondition");
        assert!(
            Arc::ptr_eq(
                &durable_ctx.claimed_worker,
                &durable_ingress.worker_handle()
            ),
            "R2/E3 durable ingress owns the canonical claimed Worker"
        );
        let durable_resolver = HostWorkerResolver {
            host: Arc::downgrade(&durable),
        };
        let resolved = durable_resolver
            .worker_for(&durable_thread, None)
            .await
            .expect("R2 resolve resident durable worker");
        assert!(
            Arc::ptr_eq(&resolved, &durable_ctx.claimed_worker),
            "R2/E3 resolver reuses the same Worker Arc"
        );

        let coordinator_store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                .expect("R3 dispatch store"),
        );
        let coordinator = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_coordinator_dispatch_store(coordinator_store.clone()),
        );
        let coordinator_thread = "coordinator-only-claimed-root";
        let coordinator_claim = claim(
            &coordinator_store,
            coordinator_thread,
            "coordinator-only-run",
            "remote-worker",
            now,
        )
        .await;
        let coordinator_resolver = HostWorkerResolver {
            host: Arc::downgrade(&coordinator),
        };
        let error = match coordinator_resolver
            .worker_for_claimed(&coordinator_claim)
            .await
        {
            Ok(_) => panic!("R3/E4 coordinator-only Host must not resolve locally"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("coordinator-only Host"),
            "R3/E4: {error}"
        );
        assert!(
            coordinator
                .session_slots
                .read(coordinator_thread, |slot| slot.runtime.is_none())
                .unwrap_or(true),
            "R3/E4 rejects before context construction"
        );
    }

    /// Cold-Worker delegation cause/effect decision table and FMECA. Causes:
    /// C1 the Worker has no process-local publication catalog; C2 the claim
    /// carries the exact child publication; C3 the bundle is absent. Effects:
    /// E1 parent context and `agent_run` admission are constructed from claim
    /// truth; E2 C3 fails before inference. Rules D1=C1+C2=>E1 and
    /// D2=C1+C3=>E2. FMECA: dropping the bundle is high severity and previously
    /// caused an endlessly retried Session; D2 turns it into a classified,
    /// claim-settled setup failure while D1 proves no Control lookup is needed.
    #[tokio::test]
    async fn cold_worker_uses_only_the_claimed_delegation_publication_closure() {
        use awaken_run_ingress::{Clock, DispatchQueue};
        use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
        use awaken_runtime_contract::snapshot::{
            AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
            AgentSnapshotMetadata,
        };

        let now = awaken_run_ingress::SystemClock.now_ms();
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };
        let child = awaken_runtime_contract::ExecutableAgentSnapshot::builder("researcher")
            .model(ModelBinding::new("test", "model", "native"))
            .fingerprint("researcher-v2")
            .metadata(AgentSnapshotMetadata {
                source: AgentConfigRevisionRef {
                    agent_id: AgentId("researcher".into()),
                    revision: 2,
                },
                publication_version: AgentPublicationVersion("researcher-v2".into()),
                resolution: Default::default(),
                fingerprint: AgentSnapshotFingerprint("researcher-v2".into()),
            })
            .build();
        let delegated = |thread: &str, run: &str| {
            let mut activation = test_activation(thread, run);
            activation.snapshot.resolved_spec.plugin_config.agent = AgentBindings {
                delegates: vec![AgentDelegateBinding {
                    agent_id: AgentId("researcher".into()),
                    source_revision: Some(2),
                    recursive_self: false,
                }],
                ..Default::default()
            };
            activation
        };

        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(delegated(
                    "cold-delegation",
                    "run-cold-delegation",
                ))
                .with_agent_publications(vec![child]),
            )
            .await
            .expect("enqueue D1");
        let complete = store
            .claim("worker-a", 1_000, now, &Default::default())
            .await
            .expect("claim D1")
            .expect("D1 available");
        resolver
            .worker_for_claimed(&complete)
            .await
            .expect("D1/E1 claimed publication constructs delegation");

        let missing_store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                .expect("missing dispatch store"),
        );
        let missing_host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_dispatch_store(missing_store.clone()),
        );
        let _missing_managed =
            crate::ManagedHost::new(missing_host.clone()).install_dispatch_session_runtime();
        let missing_resolver = HostWorkerResolver {
            host: Arc::downgrade(&missing_host),
        };
        missing_store
            .enqueue(awaken_run_ingress::RunDispatch::new(delegated(
                "cold-delegation-missing",
                "run-cold-delegation-missing",
            )))
            .await
            .expect("enqueue D2");
        let missing = missing_store
            .claim("worker-a", 1_000, now, &Default::default())
            .await
            .expect("claim D2")
            .expect("D2 available");
        let error = match missing_resolver.worker_for_claimed(&missing).await {
            Ok(_) => panic!("D2/E2 missing closure must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("invalid Agent publication closure"),
            "D2/E2: {error}"
        );
    }

    #[tokio::test]
    async fn claimed_root_cache_is_bound_to_the_exact_dispatch_publication_closure() {
        // Cause/effect graph: C1 an unclaimed warm context exists; C2 a claim
        // carries the same root but a different unversioned child publication;
        // C3 run/owner/lease epoch change while the publication closure and
        // effective model remain identical; C4 only the per-run effective model
        // changes. Effects: E1 C1+C2 rebuilds instead of inheriting the warm
        // catalog source; E2 the cache records exactly the frozen root/non-root
        // publications and effective fallback model captured by Runtime plugins;
        // E3 C3 reuses that Runtime; E4 C4 rebuilds it. Constraints: claim and
        // lease identity remain attempt context, while publication/model inputs
        // are Runtime construction inputs. Rules K1=C1+C2=>E1+E2,
        // K2=C3=>E3, K3=C4=>E4.
        use awaken_run_ingress::{Clock, DispatchQueue};
        use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};

        let child_v1 = awaken_runtime_contract::ExecutableAgentSnapshot::builder("researcher")
            .instructions("catalog v1")
            .model(ModelBinding::new("test", "model", "native"))
            .fingerprint("researcher-v1")
            .build();
        let child_v2 = awaken_runtime_contract::ExecutableAgentSnapshot::builder("researcher")
            .instructions("claimed v2")
            .model(ModelBinding::new("test", "model", "native"))
            .fingerprint("researcher-v2")
            .build();
        let mut activation = test_activation("claimed-cache", "run-claimed-cache");
        activation.snapshot.resolved_spec.plugin_config.agent = AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("researcher".into()),
                source_revision: None,
                recursive_self: false,
            }],
            ..Default::default()
        };
        activation.snapshot.resolved_spec.model_candidates = vec![
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
                "test",
                "alternate-model",
                "native",
            )),
        ];
        activation
            .snapshot
            .recompute_fingerprint()
            .expect("coherent root fixture");
        let root = activation.snapshot.clone();
        let catalog = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([child_v1])
            .expect("warm catalog source");
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_dispatch_store(store.clone())
                .with_agent_publications(Arc::new(catalog)),
        );
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let warm = host
            .ctx_for_snapshot_with_sandbox("claimed-cache", Some("agent-a"), Some(root), None)
            .await
            .expect("K1 warm unclaimed context");
        let request = awaken_run_ingress::RunDispatch::new(activation)
            .with_agent_publications(vec![child_v2]);
        store.enqueue(request).await.expect("enqueue K1");
        let now = awaken_run_ingress::SystemClock.now_ms();
        let claimed = store
            .claim("worker-a", 30_000, now, &Default::default())
            .await
            .expect("claim K1")
            .expect("K1 available");
        let expected_identity = RuntimePublicationIdentity::from_publications(
            &claimed.request.activation.snapshot,
            &claimed.request.agent_publications,
            claimed.request.activation.effective_model_ref(),
        );
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };
        resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("K1 claimed resolve");
        let claimed_ctx = host
            .session_slots
            .read("claimed-cache", |slot| slot.runtime.clone())
            .flatten()
            .expect("K1 claimed context resident");
        assert!(!Arc::ptr_eq(&warm, &claimed_ctx), "K1/E1");
        assert_eq!(
            claimed_ctx.runtime_publication_identity.as_ref(),
            Some(&expected_identity),
            "K1/E2"
        );
        let reclaimed = store
            .claim("worker-b", 30_000, now + 31_000, &Default::default())
            .await
            .expect("K2 reclaim")
            .expect("K2 expired claim available");
        resolver
            .worker_for_claimed(&reclaimed)
            .await
            .expect("K2 same publications under a different claim");
        let retried = host
            .session_slots
            .read("claimed-cache", |slot| slot.runtime.clone())
            .flatten()
            .expect("K2 context resident");
        assert!(Arc::ptr_eq(&claimed_ctx, &retried), "K2/E3");

        store
            .settle(
                &reclaimed.lease.run_id,
                reclaimed.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("settle K2");
        let mut changed_model_request = reclaimed.request;
        changed_model_request.activation.run_id = RunId("run-claimed-cache-model".into());
        changed_model_request.activation.model_ref_override = Some("alternate-model".into());
        store
            .enqueue(changed_model_request)
            .await
            .expect("enqueue K3");
        let changed_model = store
            .claim("worker-c", 30_000, now + 32_000, &Default::default())
            .await
            .expect("claim K3")
            .expect("K3 dispatch available");
        resolver
            .worker_for_claimed(&changed_model)
            .await
            .expect("K3 effective-model change");
        let model_changed_ctx = host
            .session_slots
            .read("claimed-cache", |slot| slot.runtime.clone())
            .flatten()
            .expect("K3 context resident");
        assert!(!Arc::ptr_eq(&retried, &model_changed_ctx), "K3/E4");
    }

    /// D1-D5: durable lazy placement is fenced by the current dispatch claim.
    /// Brain resolution stays sandbox-free; a replacement claim rejects stale
    /// publication; and a crash gap after dispatch binding is repaired by adoption.
    #[tokio::test]
    async fn durable_deferred_sandbox_publication_decision_table() {
        use awaken_run_ingress::{Clock, DispatchQueue};
        use awaken_session_contract::SessionRuntime;

        let now = awaken_run_ingress::SystemClock.now_ms();
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let sink = Arc::new(ToggleBindingSink {
            fail: std::sync::atomic::AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        });
        let managed = prepare_deferred_session(host.clone(), "durable-lazy").await;
        managed.install_environment_binding_sink(sink.clone());
        let claimed = claim(&store, "durable-lazy", "run-lazy", "worker-a", now).await;
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };
        resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("D1 resolves Brain worker without Sandbox");
        assert!(
            host.session_environment("durable-lazy").await.is_none(),
            "D1"
        );

        let replacement = store
            .claim("worker-b", 1_000, now + 2_000, &Default::default())
            .await
            .expect("replacement claim")
            .expect("expired claim is recoverable");
        let deferred = host
            .session_slots
            .read("durable-lazy", |slot| slot.deferred_executor.clone())
            .flatten()
            .expect("deferred executor");
        let error = deferred
            .invoke(&awaken_runtime_contract::tool::ToolCall {
                call_id: "stale-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "missing"}),
            })
            .await
            .expect_err("D3 stale claim is fenced");
        assert!(
            error.to_string().contains("replacement claim"),
            "D3: {error}"
        );
        assert!(
            host.session_environment("durable-lazy").await.is_none(),
            "D3"
        );

        resolver
            .worker_for_claimed(&replacement)
            .await
            .expect("install replacement claim");
        sink.fail.store(true, Ordering::SeqCst);
        let deferred = host
            .session_slots
            .read("durable-lazy", |slot| slot.deferred_executor.clone())
            .flatten()
            .expect("replacement deferred executor");
        let error = deferred
            .invoke(&awaken_runtime_contract::tool::ToolCall {
                call_id: "crash-gap-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "missing"}),
            })
            .await
            .expect_err("D4 Session binding failure");
        assert!(
            error
                .to_string()
                .contains("injected Session binding failure")
        );
        assert!(
            host.session_environment("durable-lazy").await.is_none(),
            "D4"
        );

        let adopted = store
            .claim("worker-c", 1_000, now + 4_000, &Default::default())
            .await
            .expect("adoption claim")
            .expect("dispatch-bound run is recoverable");
        assert!(adopted.sandbox.is_some(), "D4 dispatch binding survived");
        sink.fail.store(false, Ordering::SeqCst);
        resolver
            .worker_for_claimed(&adopted)
            .await
            .expect("D5 adopts and repairs Session binding");
        assert!(
            host.session_environment("durable-lazy").await.is_some(),
            "D5"
        );
        assert_eq!(
            sink.calls.load(Ordering::SeqCst),
            2,
            "D4 failure + D5 repair"
        );
    }

    struct RecordingSessionControl {
        resume_calls: Arc<AtomicUsize>,
        phases: Arc<std::sync::Mutex<Vec<&'static str>>>,
        projection: Arc<std::sync::Mutex<Option<awaken_session_contract::FrozenSessionProjection>>>,
        mcp_stage: Option<awaken_session_contract::StageMcpAttachment>,
        fail_begin: Arc<AtomicBool>,
    }

    #[derive(Default)]
    struct RecordingMcpRealizer {
        calls: std::sync::Mutex<Vec<&'static str>>,
        fail_stage: std::sync::atomic::AtomicBool,
        required_physical_environment: Option<(std::sync::Weak<SharedHost>, String)>,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::McpAttachmentRealizer for RecordingMcpRealizer {
        async fn stage_mcp_attachment(
            &self,
            request: awaken_session_contract::StageMcpAttachment,
        ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
        {
            self.calls.lock().unwrap().push("stage");
            if let Some((host, thread)) = &self.required_physical_environment {
                let physical_only = host.upgrade().is_some_and(|host| {
                    host.session_slots
                        .read(thread, |slot| {
                            slot.environment.is_some() && slot.runtime.is_none()
                        })
                        .unwrap_or(false)
                });
                if !physical_only {
                    return Err(awaken_session_contract::RunError::classified(
                        "test_session_physical_environment_missing",
                        "claimed MCP stage requires the physical Environment but no parent Runtime",
                    ));
                }
            }
            if self.fail_stage.load(Ordering::SeqCst) {
                return Err(awaken_session_contract::RunError::classified(
                    "test_mcp_stage_failed",
                    "test MCP stage failed",
                ));
            }
            Ok(awaken_session_contract::McpRealizationReceipt {
                receipt_fingerprint: request.fingerprint(),
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("publish");
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("drain");
            Ok(())
        }
    }

    #[tokio::test]
    async fn cold_claimed_stdio_mcp_opens_the_exact_agent_snapshot_before_staging() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        /* Claimed sandbox-stdio recovery cause/effect decision table. Causes:
         * C1 a cold Worker has no process-local Agent publication; C2 the durable
         * Run carries its exact immutable snapshot and non-default backend
         * projection; C3 its Session runtime envelope contains a sandbox-stdio
         * MCP stage; C4 the Session has no resident Runtime/Environment. Effects:
         * E1 open only the physical Session Environment from the claimed snapshot
         * before MCP staging; E2 stage and publish the exact MCP generation; E3
         * build the root Runtime once after publication, never as stage substrate;
         * E4 never consult or synthesize a second publication source. Rule S1:
         * C1+C2+C3+C4=>E1+E2+E3+E4. The adjacent malformed-envelope and stale-
         * claim rules retain fail-closed coverage for invalid authority. */
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let thread = "cold-stdio-recovery";
        let realizer = Arc::new(RecordingMcpRealizer {
            required_physical_environment: Some((Arc::downgrade(&host), thread.into())),
            ..Default::default()
        });
        let _managed = crate::ManagedHost::new(host.clone())
            .with_mcp_attachment_realizer(realizer.clone())
            .install_dispatch_session_runtime();
        let stage = awaken_session_contract::StageMcpAttachment {
            workspace_id: host.local_workspace().into(),
            generation: awaken_session_contract::McpGenerationRef {
                session_id: thread.into(),
                attachment_id: awaken_session_contract::McpAttachmentId("browser".into()),
                generation: awaken_session_contract::McpGeneration(1),
                runtime_incarnation: "worker-a".into(),
                lease_epoch: 1,
                lease_expires_at_unix_ms: u64::MAX,
            },
            realization_id: "realize-browser-1".into(),
            stage_idempotency_key: "stage-browser-1".into(),
            name: "browser".into(),
            target: awaken_session_contract::McpTarget::sandbox_stdio(
                "playwright-mcp",
                vec!["--headless".into()],
            )
            .expect("sandbox stdio target"),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        let runtime = awaken_run_ingress::SessionRuntimeEnvelope::from_projection(
            deferred_environment(),
            Some(Default::default()),
            vec![stage],
        )
        .expect("runtime envelope");
        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    thread,
                    "run-cold-stdio-recovery",
                ))
                .with_session_runtime(runtime),
            )
            .await
            .expect("enqueue");
        let claimed = store
            .claim(
                "worker-a",
                30_000,
                awaken_run_ingress::SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .expect("claim")
            .expect("claimed Run");

        HostWorkerResolver {
            host: Arc::downgrade(&host),
        }
        .worker_for_claimed(&claimed)
        .await
        .expect("S1 cold stdio recovery");

        assert_eq!(
            realizer.calls.lock().unwrap().as_slice(),
            ["stage", "publish"]
        );
        assert!(
            host.session_slots
                .read(thread, |slot| slot.runtime.is_some())
                .unwrap_or(false),
            "S1/E3"
        );
    }

    #[async_trait::async_trait]
    impl awaken_run_ingress_contract::ClaimedSessionControl for RecordingSessionControl {
        async fn resume_frozen(
            &self,
            claim: &awaken_run_ingress::RunClaim,
            _session_id: &str,
        ) -> Result<
            Option<awaken_session_contract::SessionRealizationDirective>,
            awaken_run_ingress_contract::ClaimedSessionControlError,
        > {
            self.resume_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.projection.lock().unwrap().clone().map(|projection| {
                awaken_session_contract::SessionRealizationDirective {
                    projection,
                    lease: awaken_session_contract::SessionRealizationLease {
                        owner: claim.owner.clone(),
                        runtime_incarnation: claim.owner.clone(),
                        epoch: claim.epoch,
                        expires_at_unix_ms: u64::MAX,
                    },
                    action: awaken_session_contract::SessionRealizationAction::Complete,
                }
            }))
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for RecordingSessionControl {
        async fn begin_session_realization(
            &self,
            command: awaken_session_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("begin");
            if self.fail_begin.load(Ordering::SeqCst) {
                return Err(awaken_session_contract::SessionRealizationControlFailure::NotReady);
            }
            let projection = self
                .projection
                .lock()
                .unwrap()
                .clone()
                .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
            let mut stage = self
                .mcp_stage
                .clone()
                .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
            stage.generation.lease_expires_at_unix_ms = command.target.lease_expires_at_unix_ms;
            stage.stage_idempotency_key =
                format!("renew:{}", command.target.lease_expires_at_unix_ms);
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection,
                lease: awaken_session_contract::SessionRealizationLease {
                    owner: command.target.owner,
                    runtime_incarnation: command.target.runtime_incarnation,
                    epoch: 1,
                    expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
                },
                action: awaken_session_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                },
            })
        }

        async fn activate_session_realization(
            &self,
            command: awaken_session_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("activate");
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("frozen Session projection"),
                lease: command.lease,
                action: awaken_session_contract::SessionRealizationAction::Publish {
                    publish: command
                        .mcp_receipts
                        .into_iter()
                        .map(|receipt| receipt.generation)
                        .collect(),
                    drain: Vec::new(),
                },
            })
        }

        async fn acknowledge_session_realization(
            &self,
            command: awaken_session_contract::AcknowledgeSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("acknowledge");
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("frozen Session projection"),
                lease: command.lease,
                action: awaken_session_contract::SessionRealizationAction::Complete,
            })
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_contract::FailSessionRealization,
        ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
            self.phases.lock().unwrap().push("fail");
            Ok(())
        }
    }

    fn ordinary_frozen_projection() -> awaken_session_contract::FrozenSessionProjection {
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            "test.worker",
        );
        let baseline = awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: awaken_session_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_session_contract::EnvironmentRevision(1),
                    self_hosted: true,
                    config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                        "env-fingerprint".into(),
                    ),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                        inference_holder: holder.clone(),
                        mcp_holder: holder.clone(),
                        resource_holder: holder,
                    },
                },
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
                mcp_authoring: Default::default(),
                agent_id: "agent-a".into(),
                agent_revision: None,
                model_override: None,
                model: "model".into(),
                runtime: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        );
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_contract::SessionRevision(2),
            baseline,
            agent_publication: None,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
        }
    }

    /// Cause/effect graph: C1 an authenticated Run claim is current; C2 its
    /// Session is already frozen; C3 the Worker has the claimed Session-control
    /// client. C1+C2+C3 cause E1 resume the canonical realization, E2 install
    /// the exact baseline and lease, and E3 create the Worker environment.
    /// C3 absent is the co-located local-pool row, covered by the cold legacy test.
    ///
    /// | Rule | Claim | Frozen | Control | Effect |
    /// |---|---|---|---|---|
    /// | O1 | live | yes | present | resume and realize frozen projection |
    /// | O2 | stale | any | present | reject before realization |
    /// | O3 | live | no | present | fail closed |
    /// | O4 | live | local pre-realized | absent | local-pool replay |
    /// | O5 | live Run without Session pointer | n/a | present | bypass Session control |
    #[tokio::test]
    async fn claimed_worker_uses_only_frozen_session_realization() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        let dispatch = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                .expect("in-memory dispatch"),
        );
        let resume_calls = Arc::new(AtomicUsize::new(0));
        let projection = Arc::new(std::sync::Mutex::new(Some(ordinary_frozen_projection())));
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_dispatch_store(dispatch.clone())
                .with_session_control(Arc::new(RecordingSessionControl {
                    resume_calls: resume_calls.clone(),
                    phases: Arc::new(std::sync::Mutex::new(Vec::new())),
                    projection,
                    mcp_stage: None,
                    fail_begin: Arc::new(AtomicBool::new(false)),
                })),
        );
        dispatch
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    "ordinary-remote",
                    "run-ordinary-remote",
                ))
                .for_session(awaken_agent_contract::agent::thread::Id(
                    "ordinary-remote".into(),
                ))
                .with_session_activity_epoch(1),
            )
            .await
            .expect("O1 enqueue");
        let claimed = dispatch
            .claim(
                "worker-a",
                30_000,
                awaken_run_ingress::SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .expect("O1 claim")
            .expect("O1 claimed Run");
        HostWorkerResolver {
            host: Arc::downgrade(&host),
        }
        .worker_for_claimed(&claimed)
        .await
        .expect("O1 canonical realization");

        assert_eq!(resume_calls.load(Ordering::SeqCst), 1, "O1/E1");
        assert!(
            host.session_environment("ordinary-remote").await.is_some(),
            "O1/E3"
        );
        assert!(
            host.session_slots
                .read("ordinary-remote", |slot| {
                    slot.baseline.is_some()
                        && slot.realization_lease.as_ref().is_some_and(|lease| {
                            lease.owner == claimed.lease.owner && lease.epoch == claimed.lease.epoch
                        })
                })
                .unwrap_or(false),
            "O1/E1-E2"
        );

        dispatch
            .enqueue(awaken_run_ingress::RunDispatch::new(test_activation(
                "ordinary-run",
                "run-without-session",
            )))
            .await
            .expect("O5 enqueue");
        let claimed = dispatch
            .claim(
                "worker-a",
                30_000,
                awaken_run_ingress::SystemClock.now_ms(),
                &Default::default(),
            )
            .await
            .expect("O5 claim")
            .expect("O5 claimed Run");
        HostWorkerResolver {
            host: Arc::downgrade(&host),
        }
        .worker_for_claimed(&claimed)
        .await
        .expect("O5 ordinary Run bypasses Session realization");
        assert_eq!(resume_calls.load(Ordering::SeqCst), 1, "O5");
    }

    #[tokio::test]
    async fn resource_manifest_must_match_the_durable_execution_scope() {
        // Test design. Causes: C1 a claim's execution scope is workspace-b while
        // its frozen Resource manifest names workspace-a. Effects: E1 resolution
        // fails before sandbox creation; E2 no mismatched manifest is installed.
        // Constraint/Invariant: durable execution scope owns Resource tenancy.
        // Decision rule: exercise the unequal-scope partition and require E1/E2.
        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let run = RunId("run-scope-mismatch".to_string());
        let request =
            awaken_run_ingress::RunDispatch::new(test_activation("thread-scope-mismatch", &run.0))
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace-b"),
                ))
                .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
                    "workspace-a",
                    r#"{"inputs":[],"skills":[]}"#,
                ));
        let claimed = awaken_run_ingress::Claimed {
            request,
            lease: awaken_run_ingress::Lease {
                run_id: run,
                owner: "worker-a".to_string(),
                expires_ms: 100,
                epoch: 1,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: false,
            pending: Vec::new(),
            recovered: false,
            session_activity_admission_required: false,
            sandbox: None,
            assignment: None,
        };
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let error = match resolver.worker_for_claimed(&claimed).await {
            Ok(_) => panic!("scope mismatch must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("outside its execution scope"));
        assert!(
            host.session_environment("thread-scope-mismatch")
                .await
                .is_none(),
            "scope rejection happens before sandbox creation"
        );
    }

    #[tokio::test]
    async fn malformed_resource_manifest_is_rejected_before_sandbox_creation() {
        // Test design. Causes: C1 a scope-matched claim carries malformed manifest
        // JSON. Effects: E1 resolution rejects it; E2 sandbox creation and Resource
        // projection remain untouched. Constraint/Invariant: manifest validation
        // precedes every live effect. Decision rule: exercise malformed input and
        // require fail-closed zero creation.
        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let run = RunId("run-malformed-resources".to_string());
        let request = awaken_run_ingress::RunDispatch::new(test_activation(
            "thread-malformed-resources",
            &run.0,
        ))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-a"),
        ))
        .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
            "workspace-a",
            "not-json",
        ));
        let claimed = awaken_run_ingress::Claimed {
            request,
            lease: awaken_run_ingress::Lease {
                run_id: run,
                owner: "worker-a".to_string(),
                expires_ms: 100,
                epoch: 1,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: false,
            pending: Vec::new(),
            recovered: false,
            session_activity_admission_required: false,
            sandbox: None,
            assignment: None,
        };
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let error = match resolver.worker_for_claimed(&claimed).await {
            Ok(_) => panic!("malformed manifest must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("invalid Session resource manifest")
        );
        assert!(
            host.session_environment("thread-malformed-resources")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn claimed_worker_installs_then_live_replaces_a_frozen_file_manifest() {
        let storage = tempfile::tempdir().expect("storage");
        let mut raw_host =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        // Cause/effect decision table: R1 no resident Environment + generation 1
        // File => stage before open with path fidelity/read-only enforcement; R2
        // resident Environment + higher empty generation => remove the physical
        // projection before publishing the empty logical manifest. Workdir remains
        // correctly ineligible for R1's immutability requirement.
        raw_host.session_provider =
            crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
                storage.path().join("sandboxes"),
                false,
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
                "/bin/sh",
                std::time::Duration::ZERO,
            );
        let host = Arc::new(raw_host);
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let bytes = b"frozen worker input".to_vec();
        let file_id = host
            .file_application()
            .expect("test startup installs File application")
            .create_uploaded_file(
                "workspace-a",
                "input.bin".into(),
                "application/octet-stream".into(),
                &bytes,
            )
            .await
            .expect("create file")
            .id;
        let manifest = awaken_session_contract::SessionResourceManifest::new(
            "workspace-a",
            awaken_session_contract::ResolvedSessionResources::try_new(
                vec![awaken_session_contract::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::new("file-binding"),
                    source: awaken_session_contract::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from(file_id.as_str()),
                    },
                    mount_path: "/uploads/input.bin".to_string(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                Vec::new(),
            )
            .unwrap(),
        );

        host.install_dispatched_resources("thread-cold-resource", &manifest, None)
            .await
            .expect("install frozen manifest");
        let activation = test_activation("thread-cold-resource", "run-cold-resource");
        host.ctx_for_snapshot_with_sandbox(
            "thread-cold-resource",
            Some("agent-a"),
            Some(activation.snapshot),
            None,
        )
        .await
        .expect("open environment after resource install");

        let mount = host
            .sandbox_spec("thread-cold-resource")
            .mounts
            .into_iter()
            .find(|mount| mount.mount_id == file_id)
            .expect("frozen File mount");
        assert_eq!(mount.mount_path, "/mnt/session/uploads/uploads/input.bin");
        assert_eq!(
            mount.access,
            awaken_provisioning_contract::MountAccess::ReadOnly
        );
        let awaken_provisioning_contract::MountSource::InlineBytes { contents, .. } = mount.source
        else {
            panic!("frozen File uses the binary-safe carried source")
        };
        assert_eq!(contents, bytes);
        assert_eq!(
            host.thread_resource_manifest("thread-cold-resource"),
            Some(manifest)
        );

        let cleared = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            2,
            awaken_session_contract::ResolvedSessionResources::default(),
        );
        host.install_dispatched_resources("thread-cold-resource", &cleared, None)
            .await
            .expect("R2 live replacement");
        assert!(
            host.session_environment("thread-cold-resource")
                .await
                .expect("resident environment")
                .list_frozen_mount_files("/mnt/session/uploads")
                .await
                .expect("list live files")
                .is_empty(),
            "R2 removes the stale physical projection"
        );
        assert!(
            host.sandbox_spec("thread-cold-resource").mounts.is_empty(),
            "R2 publishes the empty staged projection"
        );
        assert_eq!(
            host.thread_resource_manifest("thread-cold-resource"),
            Some(cleared),
            "R2 publishes generation 2 only after realization"
        );
    }

    /// Claimed Session Resource generation cause/effect decision table.
    /// Causes: C1 prior process-local generation exists; C2 incoming generation
    /// is exact, newer, older, or same-generation/different-value; C3 Workspace
    /// partition is unchanged. Effects: E1 revalidate exact bytes without a
    /// logical replacement; E2 advance to the newer generation; E3 reject stale,
    /// corrupt, or cross-Workspace replacement and retain the prior manifest.
    /// Rules: R1 exact+C3=>E1; R2 newer+C3=>E2; R3 older+C3=>E3;
    /// R4 same-revision/different-value+C3=>E3; R5 !C3=>E3.
    #[tokio::test]
    async fn claimed_worker_advances_only_to_a_newer_resource_generation() {
        let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
        let _managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let claim = awaken_run_ingress::RunClaim {
            run_id: RunId("run-generation-fence".into()),
            owner: "worker-generation-fence".into(),
            epoch: 1,
        };
        let revision_four = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            4,
            awaken_session_contract::ResolvedSessionResources::default(),
        );
        host.install_dispatched_resources("thread-generation-fence", &revision_four, None)
            .await
            .expect("install active generation");

        host.install_dispatched_resources("thread-generation-fence", &revision_four, Some(&claim))
            .await
            .expect("R1 exact replay");

        for rejected in [
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                3,
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                4,
                awaken_session_contract::ResolvedSessionResources::try_new(
                    Vec::new(),
                    vec![awaken_session_contract::ResolvedSkillBinding {
                        kind: awaken_agent_contract::AgentSkillKind::Custom,
                        skill_id: "different-same-generation".into(),
                        version: 1,
                        bundle_sha256: "sha256-different".into(),
                    }],
                )
                .unwrap(),
            ),
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-b",
                5,
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
        ] {
            let result = host
                .install_dispatched_resources("thread-generation-fence", &rejected, Some(&claim))
                .await;
            assert!(
                result.is_err(),
                "R3/R4/R5 reject non-authoritative replacement: {rejected:?}"
            );
            assert_eq!(
                host.thread_resource_manifest("thread-generation-fence"),
                Some(revision_four.clone())
            );
        }

        let revision_five = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            5,
            awaken_session_contract::ResolvedSessionResources::default(),
        );
        host.install_dispatched_resources("thread-generation-fence", &revision_five, Some(&claim))
            .await
            .expect("R2 newer generation");
        assert_eq!(
            host.thread_resource_manifest("thread-generation-fence"),
            Some(revision_five)
        );
    }

    /// Cause/effect decision table for Worker-side File staging:
    /// | Rule | Worker File source | exact claim | local File DB entry | Effect |
    /// |---|---|---|---|---|
    /// | W1 | remote adapter | present | absent | stage returned digest/bytes; later runtime validation does not reopen a local File database |
    /// | W2 | remote adapter | absent | absent | fail closed; no mount |
    #[tokio::test]
    async fn cold_worker_file_staging_uses_only_the_claim_bound_source() {
        struct ClaimFileSource;

        #[async_trait::async_trait]
        impl crate::FileContentSource<awaken_run_ingress::RunClaim> for ClaimFileSource {
            async fn read(
                &self,
                workspace_id: &str,
                file_id: &str,
                _purpose: &awaken_resource_contract::FileReadPurpose,
                claim: Option<&awaken_run_ingress::RunClaim>,
            ) -> Result<
                Option<awaken_resource_contract::ResolvedFileContent>,
                awaken_resource_contract::FileContentSourceError,
            > {
                let Some(claim) = claim else {
                    return Ok(None);
                };
                if workspace_id != "workspace-remote"
                    || file_id != "file-remote"
                    || claim.run_id.0 != "run-remote"
                    || claim.owner != "worker-remote"
                    || claim.epoch != 7
                {
                    return Ok(None);
                }
                let bytes = b"remote immutable File".to_vec();
                Ok(Some(awaken_resource_contract::ResolvedFileContent {
                    file_id: file_id.into(),
                    content_id: awaken_resource_contract::content_id(&bytes),
                    filename: "remote.txt".into(),
                    media_type: "text/plain".into(),
                    bytes,
                }))
            }
        }

        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_file_content_source(Arc::new(ClaimFileSource)),
        );
        let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let manifest = awaken_session_contract::SessionResourceManifest::new(
            "workspace-remote",
            awaken_session_contract::ResolvedSessionResources::try_new(
                vec![awaken_session_contract::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::new("remote-file-binding"),
                    source: awaken_session_contract::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from("file-remote"),
                    },
                    mount_path: "/input.txt".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                Vec::new(),
            )
            .unwrap(),
        );
        let claim = awaken_run_ingress::RunClaim {
            run_id: RunId("run-remote".into()),
            owner: "worker-remote".into(),
            epoch: 7,
        };

        host.install_dispatched_resources("remote-file-ok", &manifest, Some(&claim))
            .await
            .expect("W1");
        let mount = host
            .sandbox_spec("remote-file-ok")
            .mounts
            .into_iter()
            .find(|mount| mount.mount_id == "file-remote")
            .expect("W1 exact mount");
        assert!(
            matches!(
                mount.source,
                awaken_provisioning_contract::MountSource::InlineBytes { ref contents, .. }
                    if contents == b"remote immutable File"
            ),
            "W1"
        );
        managed
            .validate_thread_resource_bindings("remote-file-ok")
            .await
            .expect("W1 has no redundant local File catalog check");

        let error = host
            .install_dispatched_resources("remote-file-no-claim", &manifest, None)
            .await
            .expect_err("W2");
        assert!(error.to_string().contains("not found"), "W2: {error}");
        assert!(
            host.sandbox_spec("remote-file-no-claim").mounts.is_empty(),
            "W2"
        );
    }

    #[tokio::test]
    async fn sandbox_binding_is_validated_before_provider_adoption() {
        let storage = tempfile::tempdir().expect("storage");
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        assert!(
            host.adopt_bound_session_environment(
                "thread-a",
                Some("not-json"),
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                false,
            )
            .await
            .is_err()
        );

        let wrong = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            "local", "thread-b",
        ))
        .unwrap();
        assert!(
            host.adopt_bound_session_environment(
                "thread-a",
                Some(&wrong),
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                false,
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn recovery_mode_controls_the_production_adoption_seam() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-adoption";
        let first = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let first_ctx = first.ctx_for(thread, None).await.expect("first session");
        let handle = first_ctx.env.as_ref().expect("eager environment").handle();
        let encoded = serde_json::to_string(&handle).unwrap();
        drop(first_ctx);
        drop(first);

        let replacement =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let run_id = RunId("run-adoption".into());
        let (adopted, rebuild) = adopt_bound_sandbox(
            &replacement,
            Some(&encoded),
            thread,
            &run_id,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .expect("continuity mode adopts the durable handle");
        assert!(!rebuild);
        assert_eq!(adopted.unwrap().handle(), handle);

        let missing_thread = "thread-missing";
        let missing = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            handle.provider_kind(),
            missing_thread,
        ))
        .unwrap();
        let (adopted, rebuild) = adopt_bound_sandbox(
            &replacement,
            Some(&missing),
            missing_thread,
            &run_id,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await
        .expect("rebuild mode may replace a missing sandbox from committed truth");
        assert!(adopted.is_none());
        assert!(rebuild);
        assert!(
            adopt_bound_sandbox(
                &replacement,
                Some(&missing),
                missing_thread,
                &run_id,
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
            )
            .await
            .is_err(),
            "continuity mode fails closed when the bound sandbox is gone"
        );
    }

    #[tokio::test]
    async fn cold_adoption_uses_the_provider_owned_by_frozen_model_provisioning() {
        // Cause/effect graph: C1=HostExecutor/Provider model; C2=BackendOwned
        // model; C3=durable handle exists only below the selected provider root.
        // Effects: E1=adopt from configured Session root; E2=adopt from trusted
        // host-identity root; E3=missing selected root fails continuity.
        //
        // Decision table: A1 C1+C3(configured)=>E1 (covered by
        // recovery_mode_controls...); A2 C2+C3(trusted)=>E2; A3 either model
        // with its selected root absent=>E3 (covered by that test's missing row).
        let storage = tempfile::tempdir().expect("storage");
        let trusted_root = storage.path().join("trusted-local-sandboxes");
        let thread = "thread-backend-owned-adoption";
        let provisioning = awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
            credential: awaken_runtime_contract::CredentialRef {
                id: "local".into(),
                revision: 1,
            },
            model_selection: awaken_runtime_contract::resolved::BackendModelSelection::Default,
            acp: Default::default(),
        };

        let mut first =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        first.backend_owned_session_provider =
            Some(crate::session_environment::SessionEnvironmentProvider::workdir(&trusted_root));
        let created = first
            .backend_owned_session_provider
            .as_ref()
            .expect("trusted provider")
            .create(&first.sandbox_spec(thread))
            .await
            .expect("A2 create trusted environment");
        let handle = created.handle();
        let encoded = serde_json::to_string(&handle).unwrap();
        drop(created);
        drop(first);
        assert!(
            trusted_root.join(thread).exists(),
            "A2 trusted root owns the durable environment"
        );
        assert!(
            !storage.path().join("sandboxes").join(thread).exists(),
            "A2 ordinary provider root cannot satisfy this handle"
        );

        let mut replacement =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        replacement.backend_owned_session_provider =
            Some(crate::session_environment::SessionEnvironmentProvider::workdir(&trusted_root));
        let (adopted, rebuild) = adopt_bound_sandbox(
            &replacement,
            Some(&encoded),
            thread,
            &RunId("run-backend-owned-adoption".into()),
            &provisioning,
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .expect("A2 adopt from the provisioning-selected provider");
        assert!(!rebuild, "A2");
        assert_eq!(adopted.expect("A2 adopted").handle(), handle, "A2");
    }

    #[tokio::test]
    async fn a_resident_environment_is_reused_without_a_second_adoption() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-resident-adoption";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let ctx = host.ctx_for(thread, None).await.expect("resident session");
        let handle = ctx.env.as_ref().expect("eager environment").handle();
        let encoded = serde_json::to_string(&handle).unwrap();

        let (adopted, rebuild) = adopt_bound_sandbox(
            &host,
            Some(&encoded),
            thread,
            &RunId("resident-run".into()),
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .expect("resident handle is already adopted");

        assert!(adopted.is_none(), "no duplicate environment wrapper");
        assert!(!rebuild);
        assert_eq!(host.session_environment_handle(thread).await, Some(handle));
    }

    #[tokio::test]
    async fn a_dead_resident_environment_fails_continuity_and_is_fenced_before_rebuild() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-dead-resident";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let ctx = host.ctx_for(thread, None).await.expect("resident session");
        let handle = ctx.env.as_ref().expect("eager environment").handle();
        let encoded = serde_json::to_string(&handle).unwrap();
        std::fs::remove_dir_all(storage.path().join("sandboxes").join(thread))
            .expect("terminate local sandbox out of band");

        let run = RunId("dead-resident-run".into());
        assert!(
            adopt_bound_sandbox(
                &host,
                Some(&encoded),
                thread,
                &run,
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
            )
            .await
            .is_err(),
            "continuity never silently replaces a dead resident sandbox"
        );
        assert_eq!(
            host.session_environment_handle(thread).await,
            Some(handle.clone()),
            "a failed continuity check does not mutate the owner registry"
        );

        let (adopted, rebuild) = adopt_bound_sandbox(
            &host,
            Some(&encoded),
            thread,
            &run,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await
        .expect("explicit rebuild may discard the dead resident environment");
        assert!(adopted.is_none());
        assert!(rebuild);
        assert!(host.session_environment(thread).await.is_none());
        assert!(
            !host
                .session_slots
                .read(thread, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn a_stale_binding_cannot_evict_a_different_resident_environment() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-binding-fence";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let ctx = host.ctx_for(thread, None).await.expect("resident session");
        let resident = ctx.env.as_ref().expect("eager environment").handle();
        let stale = awaken_provisioning_contract::SandboxHandle::new(
            resident.provider_kind(),
            resident.sandbox_id.clone(),
        );
        let encoded = serde_json::to_string(&stale).unwrap();

        assert!(
            adopt_bound_sandbox(
                &host,
                Some(&encoded),
                thread,
                &RunId("stale-binding-run".into()),
                &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
            )
            .await
            .is_err(),
            "rebuild mode cannot override the full-handle fence"
        );
        assert_eq!(
            host.session_environment_handle(thread).await,
            Some(resident)
        );
        assert!(
            host.session_slots
                .read(thread, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn an_aba_replacement_with_the_same_handle_survives_stale_discard() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-environment-aba";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        host.ctx_for(thread, None).await.expect("resident session");
        let observed = host
            .session_environment(thread)
            .await
            .expect("observed owner");
        let replacement = Arc::new(
            host.session_provider
                .adopt(&host.sandbox_spec(thread), &observed.handle())
                .await
                .expect("same-handle replacement"),
        );
        assert_eq!(observed.handle(), replacement.handle());
        assert!(!Arc::ptr_eq(&observed, &replacement));

        host.session_slots.update(thread, |slot| {
            slot.runtime = None;
            slot.environment = Some(replacement.clone());
        });

        assert!(
            !host.discard_session_environment(thread, &observed).await,
            "object identity fences a stale observer even when the handle is reused"
        );
        let current = host
            .session_environment(thread)
            .await
            .expect("replacement kept");
        assert!(Arc::ptr_eq(&current, &replacement));
    }

    #[tokio::test]
    async fn cancellation_resolution_does_not_touch_an_invalid_sandbox_binding() {
        // Test design. Causes: C1 a cancellation-only claim carries an invalid
        // opaque sandbox binding. Effects: E1 cancellation resolves without
        // parsing/adopting that binding; E2 no sandbox side effect occurs.
        // Constraint/Invariant: terminal control does not require execution
        // realization. Decision rule: exercise C1 and require control-only success.
        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let thread = ThreadId("thread-control-only".to_string());
        let run = RunId("run-control-only".to_string());
        let fingerprint = CatalogFingerprint("control-only-catalog".to_string());
        let activation = RunActivation::new(
            run.clone(),
            thread.clone(),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("control-only-snapshot".to_string()),
                metadata: Default::default(),
                root_agent_id: AgentId("control-only-agent".to_string()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: fingerprint.clone(),
                    instructions: "test".to_string(),
                    max_steps: 1,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("provider", "model", "backend"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint,
            },
            Vec::new(),
        );
        let claimed = awaken_run_ingress::Claimed {
            request: awaken_run_ingress::RunDispatch::new(activation),
            lease: awaken_run_ingress::Lease {
                run_id: run,
                owner: "control-owner".to_string(),
                expires_ms: 100,
                epoch: 2,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: true,
            pending: Vec::new(),
            recovered: false,
            session_activity_admission_required: false,
            sandbox: Some("this is deliberately not a sandbox handle".to_string()),
            assignment: None,
        };
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("terminal control bypasses sandbox decoding/adoption");
        assert!(host.session_environment(&thread.0).await.is_none());
        assert!(
            !host
                .session_slots
                .read(&thread.0, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        );
    }
}
