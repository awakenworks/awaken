//! Original projection and recovery integration tests.
use super::*;
use awaken_session_realization::peers::EnvironmentPeer as _;
#[cfg(test)]
mod tests {
    use super::super::tests::{RecoveryControl, RecoveryEnvironmentBindingSink, frozen_projection};
    use super::*;
    use crate::host::worker_resolver::test_support::{
        AdoptionModel, eager_environment, empty_frozen_projection_for_snapshot, test_activation,
    };
    use awaken_dispatch_contract::{
        DispatchClaimOwnership, DispatchClaims, DispatchEnqueue, DispatchExecution,
    };
    use awaken_sandbox_local::FilesystemHandleCodec as _;
    use awaken_session_environment::test_support::SandboxEnvironmentAccess as _;
    use std::sync::Arc;

    /// Record an explicitly non-owning response without fabricating Root
    /// history. Ordinary Run authority must bypass this port; Managed authority
    /// must reject its response before a new physical effect.
    #[derive(Default)]
    struct LegacyUnownedEnvironmentBindingSink {
        requested_kinds:
            std::sync::Mutex<Vec<awaken_session_contract::SessionEnvironmentEffectKind>>,
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::SessionEnvironmentBindingSink
        for LegacyUnownedEnvironmentBindingSink
    {
        async fn authorize(
            &self,
            intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
        ) -> Result<
            awaken_session_contract::SessionEnvironmentEffectAuthorization,
            awaken_session_types::RunError,
        > {
            self.requested_kinds
                .lock()
                .unwrap()
                .push(intent.kind().clone());
            Ok(
                awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned {
                    intent: Box::new(intent.clone()),
                },
            )
        }

        async fn persist(
            &self,
            _receipt: awaken_session_contract::SessionEnvironmentReceipt,
        ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_types::RunError>
        {
            panic!("legacy aggregate-Unowned recovery must not persist a durable binding")
        }
    }

    /// Claimed durable-Hand rebuild cause/effect graph: C1 an exact durable
    /// Environment A and Runtime R1 are Resident; C2 realization revocation
    /// closes A's Hand and clears R1 while preserving the physical Sandbox; C3
    /// a claimed Rebuild realization projects the same durable binding and the
    /// provider reports Ready. Effects: E1 the existing synchronizer moves only
    /// exact `Retiring(RealizationRevocation, Durable)` through canonical
    /// adoption; E2 Environment B and its Hand are fresh Arcs; E3 B retains A's
    /// exact typed handle/root and root sentinel; E4 the final claimed resolver
    /// installs a fresh Runtime R2 with B as the sole Resident owner.
    ///
    /// | Rule | claim | policy | owner | observation | Effect |
    /// |---|---|---|---|---|---|
    /// | DH1 | yes | Rebuild | exact Durable revocation | Ready | E1-E4 |
    /// | DH2 | no | any | exact Durable revocation | Ready | no claimed transition |
    /// | DH3 | yes | Continuity | foreign/non-Ready | any | retain exact fence |
    #[tokio::test]
    #[cfg_attr(
        not(target_os = "linux"),
        ignore = "requires Linux mount-namespace path fidelity"
    )]
    async fn claimed_ready_durable_revocation_adopts_a_fresh_hand_and_runtime() {
        claimed_ready_durable_replacement(false, true, false, InitialEnvironment::Resident).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_revocation_rebinds_execution_not_only_hand() {
        // SR1: scoped Resident A + explicit revocation + a fresh Session epoch
        // + exact Ready durable binding => fresh executable B, same root/bytes;
        // A remains denied. A fresh Hand around A's closed execution binding is
        // insufficient. This reuses DH1's production projection/receipt path.
        claimed_ready_durable_replacement(true, true, false, InitialEnvironment::Resident).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_new_epoch_replaces_resident_execution() {
        // SR2: scoped Resident A + no explicit revocation + new Session epoch
        // => stop A before publishing executable B; preserve the exact physical
        // binding. This is the warm approval/resume path, distinct from SR1.
        // Both rules assert real process admission, not merely wrapper identity.
        claimed_ready_durable_replacement(true, false, false, InitialEnvironment::Resident).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_unknown_predecessor_blocks_replacement_and_retries() {
        // SR3: new epoch + indeterminate durable process observer => stop the
        // exact local tree but retain A as Retiring, old lease and root bytes;
        // do not publish a Candidate/new proof. Restore exact observer evidence
        // under this fixture's exclusive ownership => retry reaches SR2's B.
        claimed_ready_durable_replacement(true, false, true, InitialEnvironment::Resident).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_committed_candidate_replaces_execution() {
        // SC1: Create returned, sink readback committed, local publication did
        // not happen; a new Session generation must retire that exact hidden
        // Candidate and adopt fresh execution over the same root. SC2: renewal
        // must retain its Arc and hidden phase; a foreign binding must neither
        // publish nor stop it. Reuse SR2's real execution and receipt oracles.
        claimed_ready_durable_replacement(
            true,
            false,
            false,
            InitialEnvironment::CommittedCandidate,
        )
        .await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_committed_candidate_unknown_then_retry() {
        // SC3: SC1 + unknown predecessor observation => retain exact Bound
        // Retiring owner, old authority, no Adopt receipt. Exact evidence on
        // retry => fresh executable successor, old Arc permanently denied.
        claimed_ready_durable_replacement(
            true,
            false,
            true,
            InitialEnvironment::CommittedCandidate,
        )
        .await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_committed_candidate_explicit_revocation() {
        // SC4: committed hidden Candidate + explicit revocation before new
        // projection => Bound retirement, not Unbound cleanup; ordinary Ready
        // adoption must still preserve the root and create fresh execution.
        claimed_ready_durable_replacement(
            true,
            true,
            false,
            InitialEnvironment::CommittedCandidate,
        )
        .await;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InitialEnvironment {
        Resident,
        // Only the Linux scoped-creation rows construct this state.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        CommittedCandidate,
        BeforeCommit,
        LostResponse,
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_candidate_cancelled_before_commit_recovers() {
        // UC1: actual context Future is cancelled at the sink before root
        // commit. UC2: after root commit, only its response is lost (next test).
        // Cross direct epoch replacement, unknown observer then exact retry,
        // and explicit revocation. All rows must retain the sole physical root,
        // stop old execution, obtain a new authorized Adopt receipt, then
        // publish executable successor. No Create retry or raw cache may invent
        // commitment; same-generation renewal must preserve the hidden Arc.
        for (revoke, unknown) in [(false, false), (false, true), (true, false)] {
            claimed_ready_durable_replacement(
                true,
                revoke,
                unknown,
                InitialEnvironment::BeforeCommit,
            )
            .await;
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_candidate_lost_commit_response_recovers() {
        // UC2 crosses the same three UC1 schedules, but the sole Control root
        // already contains Create. Adoption must preserve its binding and the
        // old object must stay denied even though its initial receipt was lost.
        for (revoke, unknown) in [(false, false), (false, true), (true, false)] {
            claimed_ready_durable_replacement(
                true,
                revoke,
                unknown,
                InitialEnvironment::LostResponse,
            )
            .await;
        }
    }

    async fn claimed_ready_durable_replacement(
        scoped: bool,
        revoke: bool,
        unknown: bool,
        initial: InitialEnvironment,
    ) {
        claimed_ready_durable_replacement_chain(scoped, revoke, unknown, initial, &[]).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "required real Linux helper supplied by the portability supervision gate"]
    async fn trusted_host_scoped_session_successive_adopt_cancellations_recover() {
        // MC cause/effect graph: C1 A is Resident or an uncommitted Create;
        // C2 B's Adopt commits before cancellation; C3 C's Adopt commits before
        // cancellation. D is an uncancelled current successor. All 2x2x2 rules
        // are feasible: MC00/01/10/11 for each initial provenance. Actual
        // Pending Futures are dropped at the delegated sink, not simulated by
        // changing owner phases. E1 retain one exact physical root and bytes;
        // E2 every retired execution Arc is denied; E3 B/C remain unpublished;
        // E4 only committed prefixes plus D write receipts; E5 D's fresh
        // Runtime consumes only D. Same immutable Sandbox generation is not
        // proof that this Adopt committed. The fixture root is in-memory;
        // this is Host/real-process evidence, not distributed database CAS.
        // MC-M: a published filesystem tool selects filesystem delivery, not
        // semantic Memory. All eight schedules carry one writable Copy Memory
        // participant. Each actual scoped process reads its predecessor's
        // exact bytes before writing its own; cancelled adoption must not
        // rematerialize the seed or lose the materialization heads. The source
        // store stays unchanged until terminal reconciliation, outside this
        // live adoption test. This does not stand in for FUSE/Worker-death tests.
        for initial in [
            InitialEnvironment::Resident,
            InitialEnvironment::BeforeCommit,
        ] {
            for schedule in [[false, false], [false, true], [true, false], [true, true]] {
                claimed_ready_durable_replacement_chain(true, false, false, initial, &schedule)
                    .await;
            }
        }
    }

    async fn claimed_ready_durable_replacement_chain(
        scoped: bool,
        revoke: bool,
        unknown: bool,
        initial: InitialEnvironment,
        successor_cancellations: &[bool],
    ) {
        use awaken_session_realization_contract::SessionProjectionSynchronizer as _;
        let committed_candidate = initial != InitialEnvironment::Resident;
        let acknowledged = matches!(
            initial,
            InitialEnvironment::Resident | InitialEnvironment::CommittedCandidate
        );
        let persisted_creates = usize::from(initial != InitialEnvironment::BeforeCommit);

        let thread = "claimed-ready-durable-hand-rebuild";
        let storage = tempfile::tempdir().expect("durable Hand storage");
        let mut dependencies =
            crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub").await;
        dependencies.env.session_provider =
            awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
                storage.path(),
                false,
                Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
                "/bin/sh",
                std::time::Duration::ZERO,
            )
            .into_provider_roles();
        #[cfg(target_os = "linux")]
        if scoped {
            let mut provider = awaken_session_environment::SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(storage.path(), false);
            provider.install_workdir_hand(
                Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
                crate::acp_backend::deployment_test_binary("AWAKEN_TEST_SCOPED_HELPER")
                    .to_string_lossy()
                    .into_owned(),
                std::time::Duration::ZERO,
            );
            provider
                .install_scoped_execution(
                    awaken_sandbox_local::LocalExecutionDeployment::new(
                        crate::acp_backend::deployment_test_binary("AWAKEN_TEST_SCOPED_LAUNCHER"),
                        crate::acp_backend::deployment_test_binary("AWAKEN_TEST_SCOPED_HELPER"),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            dependencies.env.session_provider = provider.into_provider_roles();
        }
        let dependencies = dependencies.with_store_dir(storage.path()).await;
        let dependencies = if successor_cancellations.is_empty() {
            dependencies
        } else {
            crate::host::with_test_memory_mounter(dependencies)
        };
        let host = ((dependencies).with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_resource_validator(crate::host::test_resource_validator()),
        ))
        .build()
        .expect("validated Host dependencies");
        let managed =
            crate::host::worker_resolver::test_support::complete_managed_test_host(host.clone());
        let snapshot = test_activation(thread, "run-claimed-ready-durable").snapshot;
        let snapshot = if successor_cancellations.is_empty() {
            snapshot
        } else {
            awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
                .resolved_model(snapshot.resolved_spec.model_binding.clone())
                .tools(
                    crate::config::advertised_tools(&Default::default(), &Default::default(), &[])
                        .into_iter()
                        .filter(|tool| tool.id == "bash"),
                )
                .build()
        };
        let mut projection = frozen_projection();
        let memory_store = "replacement-chain-memory";
        if !successor_cancellations.is_empty() {
            host.resources
                .memory_stores
                .writer()
                .create(memory_store, "/note.txt", "seed")
                .await
                .unwrap();
            let resources = awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
                vec![awaken_session_configuration_contract::resource::ResolvedInput {
                    created_at_unix_ms: None,
                    updated_at_unix_ms: None,
                    binding_id: "chain-memory".into(),
                    source: awaken_session_configuration_contract::resource::ResolvedInputSource::MemoryStore {
                        memory_store_id: memory_store.into(),
                        config: awaken_resource_contract::MemoryStoreConfigVersion {
                            memory_store_id: memory_store.into(),
                            version: awaken_resource_contract::ConfigVersion::INITIAL,
                            retention_policy: Default::default(),
                        },
                    },
                    mount_path: "/mnt/memory".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadWrite,
                    instructions: None,
                }],
                Vec::new(),
            )
            .unwrap();
            projection = crate::host::worker_resolver::test_support::frozen_projection_for_manifest(
                projection.baseline.environment.clone(),
                &awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
                    projection.workspace_id.clone(),
                    1,
                    resources,
                ),
            );
        }
        projection.agent_publication = Some(snapshot.clone());
        let control = RecoveryControl::fixed(projection.clone());
        let binding_sink = Arc::new(RecoveryEnvironmentBindingSink::new(&control));
        managed
            .run_assembly
            .install_test_environment_binding_sink(binding_sink.clone());
        let initial_lease =
            awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a/boot-1".into(),
                epoch: 1,
                expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms()
                    + 60_000,
            };
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &*managed.run_assembly,
            thread,
            projection,
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                lease: awaken_session_realization::test_support::test_observed_lease(
                    initial_lease.clone(),
                ),
                prepare_session: true,
            },
        )
        .await
        .expect("DH1 install complete durable projection");
        if !successor_cancellations.is_empty() {
            assert_eq!(
                host.realization_executor.sandbox_spec(thread).mounts.len(),
                1,
                "MC-M complete frozen projection stages the Memory requirement before Create"
            );
        }
        let cached_runtime = if !acknowledged {
            let sink = Arc::new(
                crate::host::worker_resolver::test_support::BlockingBindingSink::around(
                    binding_sink.clone(),
                    initial == InitialEnvironment::LostResponse,
                ),
            );
            managed
                .run_assembly
                .install_test_environment_binding_sink(sink.clone());
            let creating = {
                let host = host.clone();
                let snapshot = snapshot.clone();
                tokio::spawn(async move {
                    host.run_assembly
                        .ctx_for_snapshot(thread, Some("agent-a"), Some(snapshot))
                        .await
                })
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), sink.entered.notified())
                .await
                .expect("UC1/UC2 actual persistence boundary reached");
            let candidate = host
                .run_assembly
                .prepared_session_environment(thread)
                .unwrap();
            assert!(
                candidate.durable_identity().is_none(),
                "UC no returned commit proof"
            );
            assert_eq!(
                binding_sink.calls(),
                persisted_creates,
                "UC root commit prefix"
            );
            creating.abort();
            match creating.await {
                Err(error) => assert!(error.is_cancelled()),
                Ok(_) => panic!("UC pending context unexpectedly finished"),
            }
            assert!(
                host.run_assembly
                    .prepared_session_environment(thread)
                    .unwrap()
                    .exact_matches(&candidate)
            );
            managed
                .run_assembly
                .install_test_environment_binding_sink(binding_sink.clone());
            None
        } else if committed_candidate {
            // Stop at the production sink-readback/local-publication boundary.
            // No synthetic generation or Resident -> Candidate demotion: use
            // the canonical authorization, fenced Provider and retention edges.
            let _lifecycle = host
                .run_assembly
                .installed_sessions
                .realizations()
                .lock_lifecycle(thread)
                .await
                .expect("installed Sessions");
            let effect = host
                .realization_executor
                .environments()
                .authorize_environment_effect_before_io(
                    host.realization_executor.peers(),
                    thread,
                    &host
                        .realization_executor
                        .authority()
                        .new_environment_effect_intent(
                            thread,
                            awaken_session_contract::SessionEnvironmentEffectKind::Create,
                            None,
                        )
                        .expect("installed Sessions"),
                )
                .await
                .unwrap();
            let fence = effect.provider_fence.as_ref().unwrap();
            let authority = host
                .realization_executor
                .authority()
                .provider_effect_authority(thread, fence)
                .unwrap();
            let environment = Arc::new(
                host.realization_executor
                    .environments()
                    .providers()
                    .session_provider
                    .realizer()
                    .create_effective_for_effect(
                        &host.realization_executor.sandbox_spec(thread),
                        awaken_provisioning_contract::SandboxEffect::Fenced {
                            fence,
                            authority: &authority,
                        },
                        None,
                        awaken_session_environment_contract::EnvironmentRealizationKind::Create,
                    )
                    .await
                    .unwrap(),
            );
            let candidate = host
                .realization_executor
                .environments()
                .begin_session_environment_preparation(
                    host.realization_executor.peers(),
                    thread,
                    environment,
                    Some(effect.authorization.intent()),
                )
                .unwrap();
            let identity = host
                .run_assembly
                .persist_environment_before_publish(thread, &candidate)
                .await
                .unwrap();
            host.realization_executor
                .environments()
                .retain_committed_session_environment_candidate(thread, &candidate, identity)
                .unwrap();
            assert!(
                host.realization_executor
                    .environments()
                    .session_environment(thread)
                    .await
                    .expect("installed Sessions")
                    .is_none(),
                "SC1 still hidden"
            );
            None
        } else {
            Some(
                host.run_assembly
                    .ctx_for_snapshot(thread, Some("agent-a"), Some(snapshot.clone()))
                    .await
                    .expect("DH1/C1 build durable Runtime R1"),
            )
        };
        // This fixture also compiles for Namespace on non-Linux platforms.
        // Query the existing portable publication phases, not Linux's scoped
        // execution-authority view; renewal/conflict checks share this oracle.
        let owned_environment = || {
            host.run_assembly
                .installed_sessions
                .realizations()
                .read_realization(thread, |r| {
                    r.environment().owner.resident().or_else(|| {
                        r.environment()
                            .owner
                            .unpublished_candidate()
                            .map(|candidate| candidate.environment.clone())
                    })
                })
                .expect("installed Sessions")
                .flatten()
                .expect("DH1/SC1 exact owned Environment A")
        };
        let original = owned_environment();
        let original_handle = original.lifecycle().handle();
        let original_hand = original.agent_tools().tool_executor();
        let mut memory_bytes = "seed".to_string();
        if !successor_cancellations.is_empty() {
            scoped_memory_edit(&original, &memory_bytes, "generation-1").await;
            memory_bytes = "generation-1".into();
            assert_eq!(
                original_handle
                    .memory_materializations()
                    .unwrap()
                    .unwrap()
                    .len(),
                1
            );
        }
        let projection = control.current_projection();
        let binding = serde_json::to_string(&original_handle).unwrap();
        let claimed_binding = projection.environment.binding();
        if let Some(persisted) = claimed_binding {
            assert_eq!(persisted, binding, "DH1/UC2 exact root binding");
        }
        assert_eq!(
            binding_sink.calls(),
            persisted_creates,
            "DH1/UC root Create prefix"
        );
        let root_sentinel = storage
            .path()
            .join("sandboxes")
            .join(thread)
            .join("durable-root-sentinel");
        std::fs::write(&root_sentinel, b"same physical durable root")
            .expect("DH1/C2 root sentinel");
        if scoped {
            // SR4: same generation, later expiry => preserve A and its bound
            // execution capability. Renewal is NOT a replacement or shutdown.
            let mut renewed = initial_lease.clone();
            renewed.expires_at_unix_ms += 30_000;
            host.run_assembly
                .installed_sessions
                .install_frozen_session_projection(
                    (
                        &host.run_assembly.realization_executor,
                        &host.run_assembly.mcp,
                    ),
                    thread,
                    projection.clone(),
                    None,
                    true,
                    Some(
                        awaken_session_realization::test_support::test_observed_lease(
                            renewed.clone(),
                        ),
                    ),
                )
                .await
                .unwrap();
            assert!(Arc::ptr_eq(&original, &owned_environment()));
            assert_eq!(
                host.realization_executor
                    .environments()
                    .session_environment(thread)
                    .await
                    .expect("installed Sessions")
                    .is_none(),
                committed_candidate,
                "SR4/SC2 renewal preserves publication phase"
            );
            assert_eq!(
                binding_sink.calls(),
                persisted_creates,
                "SR4 no adoption receipt"
            );
            scoped_admission(&original)
                .await
                .expect("SR initial execution is admitted");
        }
        let predecessor = if scoped {
            let mut command = awaken_provisioning_contract::Command::new([
                "/bin/sh",
                "-c",
                "setsid /bin/sleep 60 & /bin/sleep 60 & printf active > scope-ready; wait",
            ]);
            command.cwd = "/workspace".into();
            command.stdio = awaken_provisioning_contract::Stdio::Piped;
            let (process, channel) = original.agent_process().spawn_agent(command).await.unwrap();
            let ready = root_sentinel
                .parent()
                .unwrap()
                .join("workspace/scope-ready");
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while std::fs::read(&ready).ok().as_deref() != Some(b"active") {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(process.poll().await.unwrap().is_none());
            Some((process, channel))
        } else {
            None
        };
        let lifecycle = host
            .run_assembly
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| r.environment().lifecycle.clone())
            .expect("installed Sessions")
            .expect("DH1 lifecycle owner");
        if revoke {
            let _lifecycle = lifecycle.lock().await;
            // UC3: repeat after Live -> Closed. Closed provenance must retain
            // the same Candidate; it is not permission to delete its root.
            for _ in 0..2 {
                assert!(
                    host.realization_executor
                        .environments()
                        .retire_session_environment_for_revocation(
                            thread,
                            (
                                &host.run_assembly.mcp,
                                host.run_assembly.installed_sessions.as_ref()
                            ),
                            None,
                        )
                        .await
                        .expect("DH1/C2 revoke exact durable owner"),
                    "DH1/C2 owner enters Retiring"
                );
            }
        }
        assert!(
            !revoke
                || host
                    .run_assembly
                    .installed_sessions
                    .read(thread, |installed| installed.run.context.is_none()
                        && !installed.realization.environment().owner.is_resident())
                    .expect("installed Sessions")
                    .unwrap_or(false),
            "DH1/C2 revocation clears R1 and hides Retiring A"
        );

        // CR4 projection publication now carries the actual Dispatch claim.
        // Use its authoritative queue row, not a fabricated tuple that would
        // bypass the deferred binding fence when the installed Session had no claim.

        let dispatch = host
            .run_assembly
            .authority
            .dispatch_binding()
            .map(|binding| binding.queue())
            .unwrap();
        dispatch
            .enqueue(awaken_dispatch_contract::RunDispatch::new(test_activation(
                thread,
                "run-claimed-ready-durable",
            )))
            .await
            .unwrap();
        let claimed = dispatch
            .claim("worker-a", 300_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: claim.owner.clone(),
            runtime_incarnation: "worker-a/boot-2".into(),
            epoch: 2,
            expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms()
                + 60_000,
        };
        let directive = awaken_session_realization_contract::SessionRealizationDirective {
            projection: projection.clone(),
            authority: awaken_session_realization::test_support::test_lease_receipt(lease.clone()),
            action: awaken_session_realization_contract::SessionRealizationAction::Complete,
        };
        if scoped && !revoke {
            // SR5/SC2: a newer root with a foreign binding OR generation is not retirement
            // authority. Validate the prospective owner before any tree stop;
            // final publication validation alone would reject too late.
            for foreign_generation in [false, true] {
                // An unacknowledged Candidate knows its physical handle, not
                // a Store-generated generation; do not fabricate that proof.
                if foreign_generation && !acknowledged {
                    continue;
                }
                let mut foreign = projection.clone();
                foreign.revision.0 += 1;
                if foreign.environment.binding().is_none() {
                    foreign.environment =
                        awaken_session_contract::SessionEnvironmentState::Resident {
                            binding: binding.clone(),
                            effect_id: None,
                            generation: None,
                            idle_since_unix_ms: None,
                        };
                }
                if let awaken_session_contract::SessionEnvironmentState::Resident {
                    binding,
                    generation,
                    ..
                } = &mut foreign.environment
                {
                    if foreign_generation {
                        let source = generation.as_ref().unwrap();
                        *generation = Some(awaken_session_contract::SandboxGeneration::new(
                            &awaken_session_types::SessionId::from("foreign-session"),
                            source.created_at_unix_ms(),
                            source.expires_at_unix_ms(),
                            source.environment_fingerprint(),
                            source.base_image_fingerprint(),
                        ));
                    } else {
                        binding.push_str("-foreign");
                    }
                } else {
                    panic!("SR5 Resident projection")
                }
                assert!(
                    host.run_assembly
                        .installed_sessions
                        .install_frozen_session_projection(
                            (
                                &host.run_assembly.realization_executor,
                                &host.run_assembly.mcp
                            ),
                            thread,
                            foreign,
                            None,
                            true,
                            Some(
                                awaken_session_realization::test_support::test_observed_lease(
                                    lease.clone()
                                )
                            )
                        )
                        .await
                        .is_err()
                );
                assert!(Arc::ptr_eq(&original, &owned_environment()));
                assert_eq!(
                    host.realization_executor
                        .environments()
                        .session_environment(thread)
                        .await
                        .expect("installed Sessions")
                        .is_none(),
                    committed_candidate,
                    "SR5/SC2 foreign projection preserves publication phase"
                );
                assert!(
                    predecessor
                        .as_ref()
                        .unwrap()
                        .0
                        .poll()
                        .await
                        .unwrap()
                        .is_none()
                );
                scoped_admission(&original)
                    .await
                    .expect("SR5 valid prior execution was not retired");
                assert_eq!(binding_sink.calls(), persisted_creates);
            }
        }
        if unknown {
            let marker = root_sentinel
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join(format!(".{thread}.awaken-realization.json"));
            let original_marker = std::fs::read(&marker).unwrap();
            let mut foreign: serde_json::Value = serde_json::from_slice(&original_marker).unwrap();
            for execution in foreign["executions"].as_array_mut().unwrap() {
                execution["boot_id"] = serde_json::to_value([7_u8; 16]).unwrap();
            }
            std::fs::write(&marker, serde_json::to_vec(&foreign).unwrap()).unwrap();
            let result = WorkerProjectionSynchronizer::for_directive(
                host.run_assembly.as_ref(),
                &directive,
                Some(&claim),
                Some(&snapshot),
                true,
                claimed_binding,
            )
            .synchronize_session_projection(
                &awaken_session_types::SessionId::from(thread),
                &projection,
                &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
                true,
            )
            .await;
            assert!(
                result.is_err(),
                "SR3 unknown tree cannot authorize replacement"
            );
            host.run_assembly
                .installed_sessions
                .read(thread, |installed| {
                    assert!(matches!(&installed.realization.environment().owner,
                    crate::environment_owner::SessionEnvironmentOwner::Retiring(retiring)
                    if Arc::ptr_eq(&retiring.owned.environment(), &original)));
                    assert_eq!(
                        installed
                            .realization
                            .execution()
                            .authority
                            .lease()
                            .unwrap()
                            .epoch,
                        1
                    );
                    assert!(installed.run.context.is_none());
                })
                .unwrap();
            assert_eq!(
                binding_sink.calls(),
                persisted_creates,
                "SR3 no Adopt receipt"
            );
            assert_eq!(
                std::fs::read(&root_sentinel).unwrap(),
                b"same physical durable root"
            );
            std::fs::write(&marker, original_marker).unwrap();
        }
        let mut retired = vec![original.clone()];
        for round in 0..=successor_cancellations.len() {
            let projection = control.current_projection();
            let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
                epoch: 2 + round as u64,
                runtime_incarnation: format!("worker-a/boot-{}", 2 + round),
                ..lease.clone()
            };
            let directive = awaken_session_realization_contract::SessionRealizationDirective {
                projection: projection.clone(),
                authority: awaken_session_realization::test_support::test_lease_receipt(
                    lease.clone(),
                ),
                action: awaken_session_realization_contract::SessionRealizationAction::Complete,
            };
            let cancelled_sink = successor_cancellations.get(round).map(|commit| {
                Arc::new(
                    crate::host::worker_resolver::test_support::BlockingBindingSink::around(
                        binding_sink.clone(),
                        *commit,
                    ),
                )
            });
            if let Some(sink) = &cancelled_sink {
                managed
                    .run_assembly
                    .install_test_environment_binding_sink(sink.clone());
            }
            {
                let recovering = async {
                    WorkerProjectionSynchronizer::for_directive(
                        host.run_assembly.as_ref(),
                        &directive,
                        Some(&claim),
                        Some(&snapshot),
                        true,
                        projection.environment.binding(),
                    )
                    .synchronize_session_projection(
                        &awaken_session_types::SessionId::from(thread),
                        &projection,
                        &awaken_session_realization::test_support::test_observed_lease(
                            lease.clone(),
                        ),
                        true,
                    )
                    .await
                    .expect("DH1/MC canonical claimed adoption");
                    if projection.environment.binding().is_none() {
                        // UC1 has no invented dispatch binding: ordinary lazy
                        // Tool entry recovers the retained physical owner.
                        host.realization_executor
                            .ensure_session_environment_for_tool(thread)
                            .await
                            .expect("UC1/MC ordinary lazy Environment recovery");
                    }
                };
                tokio::pin!(recovering);
                if let Some(sink) = &cancelled_sink {
                    tokio::select! {
                        _ = &mut recovering => panic!("MC expected pending Adopt"),
                        result = tokio::time::timeout(
                            std::time::Duration::from_secs(5), sink.entered.notified(),
                        ) => result.expect("MC actual Adopt persistence boundary"),
                    }
                    assert_eq!(sink.calls(), 1, "MC one pending Adopt attempt");
                } else {
                    recovering.await;
                }
                // Drop the borrowed future here, releasing the actual locks.
            }
            let after = control.current_projection();
            if successor_cancellations.get(round) == Some(&false) {
                assert_eq!(
                    after.environment, projection.environment,
                    "MC no root commit"
                );
            } else {
                assert_ne!(
                    after.environment.effect_id(),
                    projection.environment.effect_id(),
                    "MC each committed Adopt has its own receipt, including final recovery"
                );
                assert_eq!(after.environment.binding(), Some(binding.as_str()));
                if let Some(generation) = projection.environment.generation() {
                    assert_eq!(
                        after.environment.generation(),
                        Some(generation),
                        "MC Adopt never rotates the physical Sandbox generation"
                    );
                }
            }
            if let Some(sink) = cancelled_sink {
                let candidate = host
                    .run_assembly
                    .prepared_session_environment(thread)
                    .expect("MC cancelled successor retains Candidate");
                assert!(
                    host.realization_executor
                        .environments()
                        .session_environment(thread)
                        .await
                        .expect("installed Sessions")
                        .is_none(),
                    "MC hidden owner"
                );
                assert_eq!(candidate.environment.lifecycle().handle(), original_handle);
                for old in &retired {
                    assert!(!Arc::ptr_eq(old, &candidate.environment));
                    assert!(
                        scoped_admission(old).await.is_err(),
                        "MC retired Arc stays denied"
                    );
                }
                scoped_admission(&candidate.environment)
                    .await
                    .expect("MC current Candidate execution remains valid before replacement");
                let next_bytes = format!("generation-{}", claim.epoch);
                scoped_memory_edit(&candidate.environment, &memory_bytes, &next_bytes).await;
                memory_bytes = next_bytes;
                assert_eq!(
                    binding_sink.calls(),
                    persisted_creates
                        + successor_cancellations[..=round]
                            .iter()
                            .filter(|commit| **commit)
                            .count()
                );
                assert_eq!(
                    std::fs::read(&root_sentinel).unwrap(),
                    b"same physical durable root"
                );
                retired.push(candidate.environment);
                managed
                    .run_assembly
                    .install_test_environment_binding_sink(binding_sink.clone());
                drop(sink);
            }
        }

        let rebuilt = host
            .realization_executor
            .environments()
            .session_environment(thread)
            .await
            .expect("installed Sessions")
            .expect("DH1/E1-E2 fresh Resident Environment B");
        let rebuilt_hand = rebuilt.agent_tools().tool_executor();
        if !successor_cancellations.is_empty() {
            scoped_memory_edit(&rebuilt, &memory_bytes, "generation-final").await;
            assert_eq!(
                std::fs::read(root_sentinel.parent().unwrap().join("mnt/memory/note.txt")).unwrap(),
                b"generation-final",
                "MC-M exact local Copy bytes survive every cancelled adoption"
            );
            assert_eq!(
                host.resources
                    .memory_stores
                    .queries()
                    .get_by_path(memory_store, "/note.txt")
                    .await
                    .unwrap()
                    .unwrap()
                    .content
                    .as_deref(),
                Some("seed"),
                "MC-M live adoption does not invent terminal writeback"
            );
        }
        if scoped {
            assert!(
                predecessor
                    .as_ref()
                    .unwrap()
                    .0
                    .poll()
                    .await
                    .unwrap()
                    .is_some(),
                "SR predecessor tree stops before successor execution"
            );
            for old in &retired {
                assert!(
                    scoped_admission(old).await.is_err(),
                    "SR/MC old execution stays denied"
                );
            }
            scoped_admission(&rebuilt)
                .await
                .expect("SR fresh execution must be admitted");
        }
        assert!(!Arc::ptr_eq(&original, &rebuilt), "DH1/E2 fresh wrapper");
        assert!(
            !Arc::ptr_eq(&original_hand, &rebuilt_hand),
            "DH1/E2 fresh Hand"
        );
        assert_eq!(
            rebuilt.lifecycle().handle(),
            original_handle,
            "DH1/E3 typed handle"
        );
        assert!(root_sentinel.exists(), "DH1/E3 root sentinel survives");
        assert_eq!(
            std::fs::read(&root_sentinel).unwrap(),
            b"same physical durable root"
        );
        assert_eq!(
            binding_sink.calls(),
            persisted_creates
                + successor_cancellations
                    .iter()
                    .filter(|commit| **commit)
                    .count()
                + 1,
            "DH1/UC/MC committed prefixes plus final canonical Adopt"
        );

        let effective_model_ref = snapshot
            .resolved_spec
            .model_binding
            .binding()
            .identity()
            .1
            .to_owned();
        let publications = Arc::new(
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot.clone()])
                .expect("DH1 claimed publication source"),
        );
        let attempt = crate::host::session_ctx::ClaimedRuntimeInput {
            identity: crate::host::session_ctx::RuntimePublicationIdentity::from_publications(
                &snapshot,
                &[],
                &effective_model_ref,
            ),
            publications,
            effective_model_ref,
        };
        let fresh_runtime = host
            .run_assembly
            .ctx_for_claimed_snapshot(thread, Some("agent-a"), snapshot, attempt)
            .await
            .expect("DH1/E4 claimed Runtime R2");
        if let Some(cached_runtime) = cached_runtime {
            assert!(
                !Arc::ptr_eq(&cached_runtime, &fresh_runtime),
                "DH1/E4 R2 cannot reuse revoked R1"
            );
        }
        assert!(
            fresh_runtime
                .env
                .as_ref()
                .is_some_and(|environment| Arc::ptr_eq(environment, &rebuilt)),
            "DH1/E4 R2 consumes only Resident B"
        );
        assert!(
            host.run_assembly
                .installed_sessions
                .read(thread, |installed| installed
                    .run
                    .context
                    .as_ref()
                    .is_some_and(|runtime| {
                        Arc::ptr_eq(runtime, &fresh_runtime)
                            && installed.realization.environment().owner.is_resident()
                    }))
                .expect("installed Sessions")
                .unwrap_or(false),
            "DH1/E4 fresh Runtime and Resident are published together"
        );
    }

    async fn scoped_memory_edit(
        environment: &crate::session_environment::SessionEnvironment,
        before: &str,
        after: &str,
    ) {
        // Literal fixture bytes are positional arguments, never shell source.
        let view = environment.agent_tools().agent_filesystem_view(
            None,
            crate::session_environment::AgentFilesystemClient::AcpProcess,
        );
        let handle = environment.lifecycle().handle();
        let materializations = handle.memory_materializations().unwrap().unwrap();
        assert_eq!(materializations.len(), 1, "MC-M exact Copy participant");
        assert_eq!(materializations[0].mount_path, "/mnt/memory");
        let rendered = view.prompt_paths.render("/mnt/memory/note.txt");
        let mut command = awaken_provisioning_contract::Command::new([
            "/bin/sh",
            "-c",
            "test \"$(cat \"$3\")\" = \"$1\" && printf %s \"$2\" > \"$3\"",
            "memory-chain",
            before,
            after,
            &rendered,
        ]);
        command.cwd = "/workspace".into();
        command.stdio = awaken_provisioning_contract::Stdio::Piped;
        let (process, _channel) = environment
            .agent_process()
            .spawn_agent(command)
            .await
            .unwrap();
        assert_eq!(
            process.wait().await.unwrap().code,
            Some(0),
            "MC-M real Memory read/write before {after}"
        );
        // The provider creates the process cwd at launch. Observe from the
        // host AFTER that operation; a lexical cwd/../mount path need not be
        // traversable before the first process. The child checked prior bytes.
        assert_eq!(
            std::fs::read(std::path::Path::new(&view.protocol_cwd).join(&rendered)).unwrap(),
            after.as_bytes(),
            "MC-M independent physical Memory bytes after {after}"
        );
    }

    async fn scoped_admission(
        environment: &crate::session_environment::SessionEnvironment,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let mut command = awaken_provisioning_contract::Command::new(["/usr/bin/true"]);
        command.cwd = "/workspace".into();
        command.stdio = awaken_provisioning_contract::Stdio::Piped;
        let (process, _channel) = environment.agent_process().spawn_agent(command).await?;
        assert_eq!(
            process.wait().await?.code,
            Some(0),
            "real scoped process exits"
        );
        Ok(())
    }

    /// Reuse the production provider/create owner to establish the one legacy
    /// V1 revocation fixture shared by claimed pre-install ordering tests.
    async fn revoked_legacy_v1_environment(
        host: &ExecutorComponents,
        thread: &str,
    ) -> (
        Arc<crate::session_environment::SessionEnvironment>,
        String,
        std::path::PathBuf,
    ) {
        let staging = host.realization_executor.staging();
        let provider = host
            .realization_executor
            .environments()
            .projected_session_environment_provider(thread, None)
            .expect("select frozen provider for LegacyDirect fixture");
        let spec = staging
            .sandbox_spec_for_provider(thread, provider)
            .expect("installed Sessions");
        let environment = Arc::new(
            host.realization_executor
                .environments()
                .create_session_environment(host.realization_executor.peers(), provider, &spec)
                .await
                .expect("create marker-free LegacyDirect V1 owner"),
        );
        assert!(
            environment
                .lifecycle()
                .handle()
                .filesystem_physical_incarnation()
                .expect("local V1 handle")
                .is_none(),
            "fixture must exercise handle-equal V1 recreation"
        );
        let binding =
            serde_json::to_string(&environment.lifecycle().handle()).expect("raw V1 binding");
        let sentinel = host
            .storage_dir()
            .expect("legacy fixture storage")
            .join("sandboxes")
            .join(thread)
            .join("retired-owner-sentinel");
        std::fs::write(&sentinel, b"retiring LegacyDirect owner").expect("legacy root sentinel");
        host.run_assembly
            .install_test_resident_session_environment(thread, environment.clone());
        let lifecycle = host
            .run_assembly
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| r.environment().lifecycle.clone())
            .expect("installed Sessions")
            .expect("legacy lifecycle owner");
        {
            let _lifecycle = lifecycle.lock().await;
            assert!(
                host.realization_executor
                    .environments()
                    .retire_session_environment_for_revocation(
                        thread,
                        (
                            &host.run_assembly.mcp,
                            host.run_assembly.installed_sessions.as_ref()
                        ),
                        None,
                    )
                    .await
                    .expect("revoke exact LegacyDirect owner"),
                "LegacyDirect owner enters Retiring"
            );
        }
        (environment, binding, sentinel)
    }

    /// Legacy recovery decision table: C1 the exact current Run claim and Ready
    /// revoked LegacyDirect V1 owner authorize disposal of A; C2 a complete
    /// Managed projection and lease are subsequently installed; C3 the selected
    /// Root port returns Unowned, supplying no original materialization fact.
    /// Effects: E1 dispose/confirm A before the projection; E2 reject new
    /// Adopt/Create before provider I/O; E3 retain only the projected raw binding,
    /// with no Candidate, Resident, new root or durable binding persistence.
    /// C1 and C2/C3 are separate authorization facts: rejecting new effects
    /// cannot invalidate the completed exact old-owner disposal.
    ///
    /// | Rule | C1 old Run/owner | C2 Managed | C3 Unowned | Effect |
    /// |---|---|---|---|---|
    /// | CR1 | exact + Ready | yes | yes | E1 + E2 + E3 |
    /// | CR2 | no claim | any | any | no claimed cleanup (merge L1-L4) |
    /// | CR3 | Continuity | any | any | no destructive recovery |
    /// | CR4 | exact + closed/error | any | any | typed owner table CP3 |
    /// Ordinary successful V1 recreation is OR1 below; it must not borrow C2.
    #[tokio::test]
    async fn claimed_rebuild_disposes_legacy_v1_before_projection_install() {
        use awaken_session_realization_contract::SessionProjectionSynchronizer as _;

        let thread = "claimed-ready-v1-preinstall-rebuild";
        let storage = tempfile::tempdir().expect("legacy rebuild storage");
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await)
            .build()
            .expect("validated Host dependencies");
        let managed = host.clone();
        let sink = Arc::new(LegacyUnownedEnvironmentBindingSink::default());
        managed
            .run_assembly
            .install_test_environment_binding_sink(sink.clone());

        let snapshot = test_activation(thread, "run-claimed-ready-v1").snapshot;
        let mut projection = frozen_projection();
        projection.agent_publication = Some(snapshot.clone());
        host.run_assembly
            .installed_sessions
            .install_frozen_session_projection(
                (
                    &host.run_assembly.realization_executor,
                    &host.run_assembly.mcp,
                ),
                thread,
                projection.clone(),
                None,
                true,
                None,
            )
            .await
            .expect("CR1 install exact frozen publication before the legacy owner");
        let (original, raw_binding, retired_sentinel) =
            revoked_legacy_v1_environment(host.as_ref(), thread).await;
        let dispatch = host
            .run_assembly
            .authority
            .dispatch_binding()
            .map(|binding| binding.queue())
            .unwrap();
        dispatch
            .enqueue(awaken_dispatch_contract::RunDispatch::new(test_activation(
                thread,
                "run-claimed-ready-v1",
            )))
            .await
            .unwrap();
        let claimed = dispatch
            .claim("worker-a", 300_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
        assert!(dispatch.claim_is_current(&claim).await.unwrap(), "CR1 C1");
        assert!(
            dispatch
                .bind_sandbox(&claim, &raw_binding)
                .await
                .unwrap()
                .applied(),
            "CR1 C1 exact Run owns the original raw binding"
        );
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: claim.owner.clone(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: claim.epoch,
            expires_at_unix_ms: u64::MAX,
        };
        let error = WorkerProjectionSynchronizer {
            execution: host.run_assembly.as_ref(),
            claim: Some(&claim),
            published_snapshot: Some(&snapshot),
            rebuild_unavailable_environment: true,
            requires_runtime_before_effects: false,
            claimed_environment_binding: Some(&raw_binding),
        }
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from(thread),
            &projection,
            &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
            true,
        )
        .await
        .expect_err("CR1 Managed authority cannot borrow an Unowned response");
        assert_eq!(
            error.run_error().code,
            "session_environment_owned_root_missing",
            "CR1/E2 typed pending"
        );
        assert!(!retired_sentinel.exists(), "CR1/E1 exact old root disposed");
        assert!(
            !retired_sentinel.parent().unwrap().exists(),
            "CR1/E3 no same-id physical creation after rejection"
        );
        assert_eq!(
            original.sandbox_environment().status().await.unwrap(),
            awaken_provisioning_contract::SandboxStatus::Terminated,
            "CR1/E1 old owner is closed and no successor exists"
        );
        assert!(
            host.realization_executor
                .environments()
                .session_environment(thread)
                .await
                .expect("installed Sessions")
                .is_none(),
            "CR1/E3"
        );
        host.run_assembly.installed_sessions.realizations().read_realization(thread, |r| {
            assert!(matches!(&r.environment().owner,
                crate::environment_owner::SessionEnvironmentOwner::Preparing(
                    crate::environment_owner::SessionEnvironmentPreparation::AwaitingAdoption { binding, .. }
                ) if binding == &raw_binding), "CR1/E3 only exact projected binding remains");
            assert!(r.environment().owner.unpublished_candidate().is_none(), "CR1/E3 no Candidate");
            assert_eq!(r.execution().authority.lease(), Some(&lease), "CR1 C2 remains Managed");
        }).expect("installed Sessions").unwrap();
        assert_eq!(
            sink.requested_kinds.lock().unwrap().as_slice(),
            &[awaken_session_contract::SessionEnvironmentEffectKind::Adopt],
            "CR1/E2 one rejected Adopt, zero new Create; persist would panic"
        );
    }

    #[tokio::test]
    async fn ordinary_resume_rebuilds_legacy_v1_under_its_exact_run_binding() {
        // OR1 causes: C1 real SQLite Queue owns the current Run claim and A's
        // raw binding; C2 exact Ready LegacyDirect A is Retiring; C3 only direct
        // Environment/publication inputs exist (no baseline, Session marker or
        // realization lease); C4 explicit resume selects the existing recovery.
        // E1 C1+C2+C3+C4 observes/disposes A and canonically creates B with the
        // same V1 handle but new Environment/Hand Arcs; E2 the exact claim fences
        // Queue binding before publication; E3 no Root authorize/persist is used.
        // CR1 above negates C3 and rejects the Unowned response. CP2/CP3 own old
        // disposal/unknown observation rows; existing binding-fence cases own
        // stale claim rejection. No new authorization or history is fabricated.
        // | Rule | Run/old owner | New-effect authority | Trigger | Effect |
        // |---|---|---|---|---|
        // | OR1 | current/Ready | ordinary, no Session grant | explicit resume | E1-E3 |
        // | CR1 | current/Ready | Managed + Unowned response | projection sync | dispose A, reject B |
        // | CP1 | no authorized recovery | either | ordinary lookup | retain A |
        // | CP3 | exact closed evidence | either | authorized recovery | consume only exact A |
        // These rows reuse the existing owner and Queue; this fixture adds no
        // Root CAS, history record, physical-domain proof or execution grant.
        // ABA constraint: after same-id recreation old.status() may correctly
        // reject B's different physical inode. Assert old Terminated at CP2's
        // disposal boundary, not after OR1 installs B. The original sentinel,
        // new Arcs and exact handle are the separate OR1 effects.
        let thread = "ordinary-ready-v1-resume-rebuild";
        let run = "ordinary-ready-v1-resume-run";
        let storage = tempfile::tempdir().unwrap();
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let store = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .unwrap(),
        );
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await
            .with_ephemeral_dispatch(store.clone()))
        .build()
        .unwrap();
        let managed = host.clone();
        let sink = Arc::new(LegacyUnownedEnvironmentBindingSink::default());
        managed
            .run_assembly
            .install_test_environment_binding_sink(sink.clone());
        let snapshot = test_activation(thread, run).snapshot;
        host.capabilities
            .register_thread_agent_projection(thread, snapshot.root_agent_id.0.as_str())
            .expect("installed Sessions");
        host.realization_executor
            .environments()
            .publication()
            .retain_session_publication(thread, Some(&snapshot))
            .unwrap();
        host.run_assembly
            .install_environment_projection(thread, &eager_environment())
            .unwrap();
        host.run_assembly
            .installed_sessions
            .read(thread, |installed| {
                assert!(
                    installed.inputs.baseline.is_none(),
                    "OR1 C3 no Managed baseline"
                );
                assert!(!installed.run.session_dispatch, "OR1 C3 ordinary Run");
                assert!(
                    matches!(
                        &installed.realization.execution().authority,
                        crate::realization_state::SessionRealizationAuthority::Uninstalled
                    ),
                    "OR1 C3 no fabricated realization lease"
                );
            })
            .unwrap();
        let (original, raw_binding, retired_sentinel) =
            revoked_legacy_v1_environment(host.as_ref(), thread).await;
        let original_handle = original.lifecycle().handle();
        let original_hand = original.agent_tools().tool_executor();
        let claimed = crate::host::worker_resolver::test_support::claim(
            &store,
            thread,
            run,
            "ordinary-worker",
            0,
            &clock,
        )
        .await;
        let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
        assert!(store.claim_is_current(&claim).await.unwrap(), "OR1 C1");
        assert!(
            store
                .bind_sandbox(&claim, &raw_binding)
                .await
                .unwrap()
                .applied(),
            "OR1 C1"
        );
        host.run_assembly
            .installed_sessions
            .install_run(thread, |state| state.dispatch_claim = Some(claim));
        let resumed = host
            .run_assembly
            .ctx_for_resume(thread)
            .await
            .expect("OR1 existing ordinary recovery");
        let rebuilt = resumed.env.as_ref().expect("OR1 Resident Environment B");
        assert!(!Arc::ptr_eq(&original, rebuilt), "OR1/E1 fresh Arc");
        assert_eq!(
            original_handle,
            rebuilt.lifecycle().handle(),
            "OR1/E1 same V1 handle"
        );
        assert!(
            !Arc::ptr_eq(&original_hand, &rebuilt.agent_tools().tool_executor()),
            "OR1/E1 fresh Hand"
        );
        assert!(
            !retired_sentinel.exists(),
            "OR1/E1 old root disposed before recreation"
        );
        assert!(
            host.run_assembly
                .installed_sessions
                .realizations()
                .read_realization(thread, |r| r
                    .environment()
                    .owner
                    .resident()
                    .is_some_and(|owned| Arc::ptr_eq(&owned, rebuilt)))
                .expect("installed Sessions")
                .unwrap(),
            "OR1/E1 one published owner"
        );
        assert!(
            sink.requested_kinds.lock().unwrap().is_empty(),
            "OR1/E3 Root port unused; persist would panic"
        );
        // This reads the original Queue after expiry, without another bind that
        // could repair a missing write. Equal V1 bytes are expected; their
        // readback is delivery evidence, not physical-incarnation authority.
        clock.set(2_002);
        let readback = store
            .claim("ordinary-readback", 60_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(readback.lease.run_id().0, run, "OR1/E2 same Run");
        assert_eq!(
            readback.sandbox.as_deref(),
            Some(raw_binding.as_str()),
            "OR1/E2 exact Queue binding"
        );
    }

    /// Remote claimed-realization cause/effect graph: C1 a claimed rebuild
    /// carries an A2A-only publication; C2 the installed Session retains an exact Ready,
    /// revoked local LegacyDirect owner; C3 neither the aggregate projection nor
    /// raw delivery cache supplies a local binding. Effects: E1 classify the
    /// conflict before legacy disposal, projection/Resource installation, or MCP
    /// eligibility; E2 retain the exact Retiring Arc, Ready root sentinel, frozen
    /// publication/lease caches, and Resource/MCP projections unchanged.
    ///
    /// | Rule | claim | route | local input | policy | Effect |
    /// |---|---|---|---|---|---|
    /// | RA1 | yes | A2A-only | Retiring owner | Rebuild | E1 + E2 |
    /// | RA2 | yes | A2A-only | none | either | ordinary remote realization |
    /// | RA3 | yes | local-capable | Retiring LegacyDirect | Rebuild | CR1 |
    #[tokio::test]
    async fn claimed_remote_a2a_rejects_a_retiring_local_owner_before_any_effect() {
        use awaken_session_realization_contract::SessionProjectionSynchronizer as _;

        let thread = "claimed-remote-a2a-retiring-local";
        let storage = tempfile::tempdir().expect("remote A2A guard storage");
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(AdoptionModel), "stub")
            .await
            .with_store_dir(storage.path())
            .await)
            .build()
            .expect("validated Host dependencies");
        let _managed = host.clone();
        let local_snapshot = test_activation(thread, "run-local-owner").snapshot;
        let mut local_projection = frozen_projection();
        local_projection.agent_publication = Some(local_snapshot.clone());
        let cached_lease =
            awaken_session_configuration_contract::resource::SessionRealizationLease {
                owner: "previous-worker".into(),
                runtime_incarnation: "previous-worker/boot-1".into(),
                epoch: 1,
                expires_at_unix_ms: u64::MAX,
            };
        host.run_assembly
            .installed_sessions
            .install_frozen_session_projection(
                (
                    &host.run_assembly.realization_executor,
                    &host.run_assembly.mcp,
                ),
                thread,
                local_projection,
                None,
                true,
                Some(
                    awaken_session_realization::test_support::test_observed_lease(
                        cached_lease.clone(),
                    ),
                ),
            )
            .await
            .expect("RA1 install retained local projection");
        let (retiring, _raw_binding, root_sentinel) =
            revoked_legacy_v1_environment(host.as_ref(), thread).await;
        let cached = host
            .run_assembly
            .installed_sessions
            .read(thread, |installed| {
                (
                    installed.inputs.published_snapshot.clone(),
                    installed.realization.execution().authority.lease().cloned(),
                    installed
                        .inputs
                        .environment_projection
                        .as_ref()
                        .map(|projection| projection.fingerprint.clone()),
                    installed.realization.staging().mcp.projection_count(),
                )
            })
            .expect("installed Sessions")
            .expect("RA1 retained installed Session caches");
        let cached_resources = host
            .realization_executor
            .staging()
            .thread_resource_manifest(thread)
            .expect("installed Sessions");

        let remote_snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "remote-agent",
                "",
                "a2a:https://agent.example.test",
            ))
            .build();
        let mut remote_projection = empty_frozen_projection_for_snapshot(
            "workspace",
            eager_environment(),
            &remote_snapshot,
        );
        remote_projection.agent_publication = Some(remote_snapshot.clone());
        let claim = awaken_dispatch_contract::RunClaim {
            run_id: RunId("run-remote-a2a-owner-conflict".into()),
            owner: "worker-a".into(),
            epoch: 2,
        };
        let lease = awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: claim.owner.clone(),
            runtime_incarnation: "worker-a/boot-2".into(),
            epoch: claim.epoch,
            expires_at_unix_ms: u64::MAX,
        };
        let directive = awaken_session_realization_contract::SessionRealizationDirective {
            projection: remote_projection.clone(),
            authority: awaken_session_realization::test_support::test_lease_receipt(lease.clone()),
            action: awaken_session_realization_contract::SessionRealizationAction::Complete,
        };
        let error = WorkerProjectionSynchronizer::for_directive(
            host.run_assembly.as_ref(),
            &directive,
            Some(&claim),
            Some(&remote_snapshot),
            true,
            None,
        )
        .synchronize_session_projection(
            &awaken_session_types::SessionId::from(thread),
            &remote_projection,
            &awaken_session_realization::test_support::test_observed_lease(lease.clone()),
            true,
        )
        .await
        .expect_err("RA1/E1 remote A2A must fail before local effects");
        assert_eq!(
            error.run_error().code,
            "session_runtime_remote_environment_conflict",
            "RA1/E1 classified fail-closed"
        );

        assert_eq!(
            retiring
                .sandbox_environment()
                .status()
                .await
                .expect("RA1 retained owner status"),
            awaken_provisioning_contract::SandboxStatus::Ready,
            "RA1/E2 rebuild disposal never ran"
        );
        assert!(root_sentinel.exists(), "RA1/E2 physical root is untouched");
        assert!(
            host.run_assembly.installed_sessions.realizations()
                .read_realization(thread, |r| {
                    matches!(
                        &r.environment().owner,
                        crate::environment_owner::SessionEnvironmentOwner::Retiring(current)
                            if current.cause
                                == crate::environment_owner::SessionEnvironmentRetirementCause::RealizationRevocation
                                && Arc::ptr_eq(&current.owned.environment(), &retiring)
                    )
                }).expect("installed Sessions")
                .unwrap_or(false),
            "RA1/E2 exact Retiring owner remains fenced"
        );
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .read(thread, |installed| {
                    (
                        installed.inputs.published_snapshot.clone(),
                        installed.realization.execution().authority.lease().cloned(),
                        installed
                            .inputs
                            .environment_projection
                            .as_ref()
                            .map(|projection| projection.fingerprint.clone()),
                        installed.realization.staging().mcp.projection_count(),
                    )
                })
                .expect("installed Sessions"),
            Some(cached),
            "RA1/E2 publication, lease, Environment, and MCP caches are unchanged"
        );
        assert_eq!(
            host.realization_executor
                .staging()
                .thread_resource_manifest(thread)
                .expect("installed Sessions"),
            cached_resources,
            "RA1/E2 Resource projection is unchanged"
        );
    }
}
