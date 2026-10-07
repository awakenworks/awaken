//! Terminal preparation, fences, and terminal Memory evidence.

use super::*;

async fn await_terminal_fence_refresh_gate<T: std::fmt::Debug, E: std::fmt::Debug>(
    gate: &tokio::sync::Notify,
    task: &mut tokio::task::JoinHandle<Result<T, E>>,
    boundary: &str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            () = gate.notified() => {}
            result = &mut *task => {
                panic!("terminal preparation ended before {boundary}: {result:?}")
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("terminal preparation did not reach {boundary}"));
}

fn terminal_memory_resources() -> awaken_session_resource_contract::ResolvedSessionResources {
    effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: "terminal-memory-store".into(),
        mount_path: "/terminal-memory".into(),
        access: awaken_resource_contract::ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }])
}

fn terminal_memory_projection() -> awaken_session_realization_contract::FrozenSessionProjection {
    terminal_memory_projection_with_idle_retention(Default::default())
}

fn terminal_memory_projection_with_idle_retention(
    idle_retention: awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy,
) -> awaken_session_realization_contract::FrozenSessionProjection {
    let resources = terminal_memory_resources();
    let mut projection = remote_terminal_cleanup_projection_with_idle_retention(idle_retention);
    projection.resource_revision = 1;
    projection.resources = resources.clone();
    projection.previous_resource_manifest = Some(
        awaken_session_resource_contract::SessionResourceManifest::at_revision(
            "terminal-workspace",
            1,
            resources,
        ),
    );
    projection
}

fn terminal_memory_evidence() -> pc::MemoryMaterializationEvidence {
    pc::MemoryMaterializationEvidence::new(
        "terminal-memory-store",
        "/mnt/terminal-memory",
        vec![pc::MemoryMaterializationHead {
            path: "seed.txt".into(),
            id: "terminal-memory-head".into(),
            version: 1,
            content_sha256: "terminal-memory-seed-sha".into(),
        }],
    )
    .expect("canonical terminal Memory evidence")
}

struct RecordingTerminalMemoryReferenceEncoder {
    events: Arc<Mutex<Vec<&'static str>>>,
    realization_attempts: Arc<std::sync::atomic::AtomicUsize>,
}

impl
    awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_dispatch_contract::RunClaim,
    > for RecordingTerminalMemoryReferenceEncoder
{
    fn encode(
        &self,
        _workspace_id: &str,
        _memory_store_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        _access: awaken_resource_contract::ResourceAccess,
        _fence: &awaken_dispatch_contract::RunClaim,
    ) -> Result<String, awaken_resource_contract::MemoryMaterializationReferenceError> {
        self.events.lock().unwrap().push("encode-run-v1");
        Ok("run-v1-reference".into())
    }
}

impl
    awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_session_realization_contract::SessionResourceMaterializationIntent,
    > for RecordingTerminalMemoryReferenceEncoder
{
    fn encode(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
        config_version: awaken_resource_contract::ConfigVersion,
        access: awaken_resource_contract::ResourceAccess,
        fence: &awaken_session_realization_contract::SessionResourceMaterializationIntent,
    ) -> Result<String, awaken_resource_contract::MemoryMaterializationReferenceError> {
        self.realization_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let awaken_session_realization_contract::SessionResourceMaterializationTarget::Input(input) =
            fence.target()
        else {
            return Err(
                awaken_resource_contract::MemoryMaterializationReferenceError::InvalidInput {
                    detail: "realization Memory encoder did not receive a Memory input".into(),
                },
            );
        };
        let awaken_session_resource_contract::ResolvedInputSource::MemoryStore { config, .. } =
            &input.source
        else {
            return Err(
                awaken_resource_contract::MemoryMaterializationReferenceError::InvalidInput {
                    detail: "realization Memory encoder did not receive a Memory input".into(),
                },
            );
        };
        if workspace_id != "realization-workspace"
            || memory_store_id != "realization-memory-store"
            || config_version != config.version
            || access != input.access
            || fence.session_id() != "realization-memory-session"
            || fence.resource_revision() != 5
            || fence.lease().owner != "realization-memory-worker"
            || fence.lease().runtime_incarnation != "realization-memory-worker:1:boot"
            || fence.lease().epoch != 2
            || input.binding_id != awaken_resource_contract::BindingId::from("realization-memory")
            || input.mount_path != "/memory/realization"
        {
            return Err(
                awaken_resource_contract::MemoryMaterializationReferenceError::AuthorityMismatch {
                    detail: "realization Memory encoder received a foreign frozen input".into(),
                },
            );
        }
        self.events.lock().unwrap().push("encode-realization-v3");
        Ok("realization-v3-reference".into())
    }
}

impl
    awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_session_state_contract::SessionTerminalMemoryIntent,
    > for RecordingTerminalMemoryReferenceEncoder
{
    fn encode(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        access: awaken_resource_contract::ResourceAccess,
        _fence: &awaken_session_state_contract::SessionTerminalMemoryIntent,
    ) -> Result<String, awaken_resource_contract::MemoryMaterializationReferenceError> {
        if workspace_id != "terminal-workspace"
            || memory_store_id != "terminal-memory-store"
            || access != awaken_resource_contract::ResourceAccess::ReadWrite
        {
            return Err(
                awaken_resource_contract::MemoryMaterializationReferenceError::AuthorityMismatch {
                    detail: "terminal Memory encoder received a foreign frozen input".into(),
                },
            );
        }
        self.events.lock().unwrap().push("encode-terminal-v2");
        Ok("terminal-v2-reference".into())
    }
}

#[tokio::test]
async fn remote_memory_staging_reuses_one_run_or_session_authority() {
    // Cause/effect graph: C1 the Host is a registered Worker with remote
    // Resource transport; C2 a Run claim is absent/present; C3 a Session
    // realization is absent/exact/stale/zero revision; C4 the frozen Memory
    // input is exact/foreign. E1 claim alone uses RunV1; E2 exact realization
    // takes precedence over a claim and passes one already-admitted V3 intent;
    // E3 neither fence or zero revision fails before encoder or mount effects;
    // E4 stale lease/foreign input reaches one encoder rejection, no mount.
    //
    // | Rule | Run claim | realization | Memory input | Effect |
    // | R1 | present | absent | exact | E1 |
    // | R2 | any | exact | exact | E2 |
    // | R3 | absent | absent/zero | exact | E3 |
    // | R4 | any | stale | exact | E4 |
    // | R5 | any | exact | foreign | E4 |
    let events = Arc::new(Mutex::new(Vec::new()));
    let realization_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .await
            .with_authority(
                crate::UpstreamAuthority::default().with_memory_reference_encoder(Arc::new(
                    RecordingTerminalMemoryReferenceEncoder {
                        events: events.clone(),
                        realization_attempts: realization_attempts.clone(),
                    },
                )),
            ),
    );
    let managed = crate::ManagedHost::new(host);
    let input = awaken_session_resource_contract::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from("realization-memory"),
        source: awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                "realization-memory-store",
            ),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                    "realization-memory-store",
                ),
                version: awaken_resource_contract::ConfigVersion(3),
                retention_policy: Default::default(),
            },
        },
        mount_path: "/memory/realization".into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
    };
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "realization-memory-worker".into(),
        runtime_incarnation: "realization-memory-worker:1:boot".into(),
        epoch: 2,
        expires_at_unix_ms: u64::MAX,
    };
    let desired = awaken_session_resource_contract::SessionResourceManifest::at_revision(
        "realization-workspace",
        5,
        awaken_session_resource_contract::ResolvedSessionResources::try_new(
            vec![input.clone()],
            Vec::new(),
        )
        .expect("R2 exact Memory manifest"),
    );
    let transition = awaken_session_resource_contract::SessionResourceTransition::new(
        awaken_session_resource_contract::SessionResourceManifest::at_revision(
            "realization-workspace",
            0,
            Default::default(),
        ),
        desired,
    )
    .expect("R2 exact aggregate transition");
    managed
        .stage_prevalidated_resource_transition_under_resource_projection(
            "realization-memory-session",
            &transition,
            None,
            Some(&lease),
            false,
        )
        .await
        .expect("R2/E2 realization-fenced remote Memory staging");
    let staged = managed
        .host
        .session_slots
        .read("realization-memory-session", |slot| slot.resources.clone())
        .expect("R2/E2 staged slot");
    let awaken_provisioning_contract::MountSource::MemoryStore {
        materialization_reference,
        ..
    } = &staged.mounts[0].source
    else {
        panic!("R2/E2 exact typed Memory mount")
    };
    assert_eq!(
        materialization_reference.as_deref(),
        Some("realization-v3-reference"),
        "R2/E2"
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &["encode-realization-v3"],
        "R2/E2 only the realization encoder"
    );

    managed
        .stage_prevalidated_resource_transition_under_resource_projection(
            "realization-memory-session",
            &transition,
            None,
            Some(&lease),
            false,
        )
        .await
        .expect("R2 exact lease replay reuses its compiled projection");
    assert_eq!(events.lock().unwrap().len(), 1, "R2 exact cache replay");
    let renewed = awaken_session_resource_contract::SessionRealizationLease {
        expires_at_unix_ms: lease.expires_at_unix_ms - 1,
        ..lease.clone()
    };
    managed
        .stage_prevalidated_resource_transition_under_resource_projection(
            "realization-memory-session",
            &transition,
            None,
            Some(&renewed),
            false,
        )
        .await
        .expect("R2 renewed lease recompiles its transport reference");
    assert_eq!(
        events.lock().unwrap().len(),
        2,
        "R2 lease generation is part of the cache authority"
    );

    fn reference(staged: &crate::staged_resources::StagedResources) -> Option<&str> {
        let awaken_provisioning_contract::MountSource::MemoryStore {
            materialization_reference,
            ..
        } = &staged.mounts[0].source
        else {
            panic!("expected one typed Memory mount")
        };
        materialization_reference.as_deref()
    }
    let claim = awaken_dispatch_contract::RunClaim {
        run_id: RunId("realization-run".into()),
        owner: "realization-memory-worker".into(),
        epoch: 2,
    };
    let run_only = managed
        .stage_resolved_input("realization-workspace", &input, Some(&claim), None, None)
        .await
        .expect("R1 Run claim alone");
    assert_eq!(reference(&run_only), Some("run-v1-reference"), "R1/E1");
    assert_eq!(
        realization_attempts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "R1 does not call the Session encoder"
    );
    let both = managed
        .stage_resolved_input(
            "realization-workspace",
            &input,
            Some(&claim),
            Some(("realization-memory-session", 5, &lease)),
            None,
        )
        .await
        .expect("R2 Session realization supersedes a Run claim");
    assert_eq!(reference(&both), Some("realization-v3-reference"), "R2/E2");
    assert_eq!(
        realization_attempts.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "R2 invokes the exact Session encoder once"
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            "encode-realization-v3",
            "encode-realization-v3",
            "encode-run-v1",
            "encode-realization-v3",
        ],
        "R1/R2 select distinct existing encoders"
    );

    let stale = awaken_session_resource_contract::SessionRealizationLease {
        epoch: 1,
        ..lease.clone()
    };
    let stale_error = match managed
        .stage_resolved_input(
            "realization-workspace",
            &input,
            Some(&claim),
            Some(("realization-memory-session", 5, &stale)),
            None,
        )
        .await
    {
        Ok(_) => panic!("R4 stale Session lease unexpectedly encoded"),
        Err(error) => error,
    };
    assert_eq!(
        stale_error.kind,
        awaken_session_error_contract::RunErrorKind::Internal,
        "R4 encoder rejection remains closed"
    );
    let mut foreign_input = input.clone();
    foreign_input.binding_id = awaken_resource_contract::BindingId::from("foreign-memory");
    let foreign_error = match managed
        .stage_resolved_input(
            "realization-workspace",
            &foreign_input,
            None,
            Some(("realization-memory-session", 5, &lease)),
            None,
        )
        .await
    {
        Ok(_) => panic!("R5 foreign Memory input unexpectedly encoded"),
        Err(error) => error,
    };
    assert_eq!(
        foreign_error.kind,
        awaken_session_error_contract::RunErrorKind::Internal,
        "R5 encoder rejection remains closed"
    );
    assert_eq!(
        realization_attempts.load(std::sync::atomic::Ordering::SeqCst),
        5,
        "R4/R5 each reach the existing encoder exactly once"
    );
    assert_eq!(events.lock().unwrap().len(), 4, "R4/R5 no mount reference");
    let zero_error = match managed
        .stage_resolved_input(
            "realization-workspace",
            &input,
            None,
            Some(("realization-memory-session", 0, &lease)),
            None,
        )
        .await
    {
        Ok(_) => panic!("R3 zero revision unexpectedly encoded"),
        Err(error) => error,
    };
    assert_eq!(
        zero_error.kind,
        awaken_session_error_contract::RunErrorKind::BadRequest,
        "R3 zero revision rejects at outer admission"
    );
    assert_eq!(
        realization_attempts.load(std::sync::atomic::Ordering::SeqCst),
        5,
        "R3 zero revision never reaches encoder"
    );

    let error = match managed
        .stage_prevalidated_resource_transition_under_resource_projection(
            "unfenced-realization-memory-session",
            &transition,
            None,
            None,
            false,
        )
        .await
    {
        Ok(_) => panic!("R3/E3 unfenced remote Memory staging must fail"),
        Err(error) => error,
    };
    assert!(
        error
            .message
            .contains("dispatch claim or Session realization lease"),
        "R3/E3: {error:?}"
    );
    assert_eq!(
        realization_attempts.load(std::sync::atomic::Ordering::SeqCst),
        5,
        "R3 no fence never reaches encoder"
    );
    assert_eq!(events.lock().unwrap().len(), 4, "R3 no mount reference");
}

struct RecordingTerminalMemoryMounter {
    events: Arc<Mutex<Vec<&'static str>>>,
    reject: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl pc::MemoryMounter for RecordingTerminalMemoryMounter {
    async fn mount(
        &self,
        _store_id: &str,
        _host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> pc::MemoryMountOutcome {
        pc::MemoryMountOutcome::NoEffect(pc::SandboxError::unsupported(
            "terminal reconciliation fixture must not materialize a new Run mount",
        ))
    }

    async fn recover_unknown_mount(
        &self,
        _: &str,
        _: &std::path::Path,
        _: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn reconcile_recovered_copy(
        &self,
        operation_reference: &str,
        evidence: &pc::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        if operation_reference != "terminal-v2-reference"
            || evidence.store_id != "terminal-memory-store"
            || evidence.mount_path != "/mnt/terminal-memory"
            || files != [("changed.txt".into(), b"changed-under-terminal-v2".to_vec())]
            || access != pc::MountAccess::ReadWrite
        {
            return Err(pc::SandboxError::conflict(
                "terminal Memory reconciliation changed its frozen inputs",
            ));
        }
        self.events.lock().unwrap().push("reconcile-terminal-v2");
        if self.reject.load(Ordering::SeqCst) {
            return Err(pc::SandboxError::unauthorized(
                "Memory reconciliation rejected",
            ));
        }
        Ok(())
    }
}

#[tokio::test]
async fn source_release_preparation_is_withheld_for_an_absent_pod_with_live_claim() {
    /* Host continuation table HS1. Causes: C1 aggregate binding carries exact
     * V2 Pod UID A plus retained claim UID P; C2 A is absent while P remains
     * live; C3 the current realization lease authorizes source disposal.
     * Effects: E1 provider rejects before mutation because no Pod finalizer can
     * serialize P against Rebuild; E2 Host emits no preparation receipt and
     * retains the exact binding for explicit recovery. Rule HS1
     * C1+C2+C3=>E1+E2. A/P total absence is the separate effect-fenced
     * response-loss row and does not fabricate a cleanup Environment. */
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        restored_disposals: Mutex::new(Vec::new()),
        events: events.clone(),
        scenario: TerminalRecoveryScenario::OrphanClaim,
        fence_refresh_probe: None,
    });
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .await
            .with_session_container_provider(
                provider,
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            ),
    );
    let managed = install_test_dispatch_runtime(&host);
    install_test_session_application(&host);
    let thread = "absent-auxiliary-source";
    let projection = remote_terminal_cleanup_projection();
    let source_manifest = projection
        .previous_resource_manifest
        .clone()
        .expect("HS1 frozen source manifest");
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &managed,
        thread,
        projection,
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("HS1 install frozen source projection");
    host.register_thread_resource_manifest(thread, source_manifest);

    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread));
    let source_binding = serde_json::to_string(&handle).unwrap();
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "source-cleanup-worker".into(),
        runtime_incarnation: "source-cleanup-worker:incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    host.install_session_environment_owner_projection(
        thread,
        "terminal-workspace",
        &awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: source_binding.clone(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    )
    .expect("HS1 project the exact durable source binding");
    host.session_slots.update(thread, |slot| {
        slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
            crate::session_slot::test_observed_lease(lease.clone()),
        );
    });
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(thread),
        1,
        u64::MAX,
        "environment-a",
        "image-a",
    );
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "terminal-workspace",
        &awaken_session_types::SessionId::from(thread),
        "dispose-source",
        &generation,
        1,
        Some(lease.clone()),
        None,
    );
    let preparation: awaken_session_state_contract::SourceReleasePreparationEffect =
        serde_json::from_value(serde_json::json!({
            "operation": operation,
            "lease": lease,
        }))
        .expect("HS1 decode the aggregate-owned preparation transport");

    let error =
        awaken_session_execution_contract::EnvironmentRuntime::prepare_checkpoint_source_disposal(
            &managed,
            thread,
            &preparation,
            &generation,
            &source_binding,
        )
        .await
        .expect_err("HS1 orphan claim cannot produce a preparation receipt");
    assert!(error.message.contains("cleanup gate"), "HS1/E1: {error:?}");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &["observe-orphan-claim"],
        "HS1/E1 zero preparation/disposal"
    );
    assert_eq!(
        host.session_slots
            .read(thread, |slot| {
                slot.environment_owner.durable_binding().map(str::to_owned)
            })
            .flatten()
            .as_deref(),
        Some(source_binding.as_str()),
        "HS1/E2 binding retained"
    );
    assert!(host.session_environment(thread).await.is_none(), "HS1/E2");
}

struct RecordingTerminalArtifactRecovery {
    events: Arc<Mutex<Vec<&'static str>>>,
    receipt: awaken_resource_contract::ArtifactPublicationReceipt,
}

#[async_trait::async_trait]
impl
    awaken_resource_contract::ArtifactPublisher<
        awaken_dispatch_contract::ArtifactPublicationFence,
    > for RecordingTerminalArtifactRecovery
{
    async fn publish(
        &self,
        _publication: awaken_resource_contract::ArtifactPublication<
            awaken_dispatch_contract::ArtifactPublicationFence,
        >,
    ) -> Result<
        awaken_resource_contract::ArtifactPublicationReceipt,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "total-absence recovery must not publish live bytes",
        ))
    }

    async fn recover(
        &self,
        _recovery: awaken_resource_contract::ArtifactRecovery<
            awaken_dispatch_contract::ArtifactPublicationFence,
        >,
    ) -> Result<
        Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.events.lock().unwrap().push("recover-artifacts");
        Ok(vec![self.receipt.clone()])
    }

    async fn capture(
        &self,
        _workspace_id: &str,
        _session_id: &str,
        _fence: Option<&awaken_dispatch_contract::ArtifactPublicationFence>,
    ) -> Result<
        Option<awaken_resource_contract::ArtifactCapture>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "fixture does not capture Artifacts",
        ))
    }

    async fn replace_capture(
        &self,
        _request: awaken_resource_contract::ReplaceArtifactCapture,
        _fence: Option<&awaken_dispatch_contract::ArtifactPublicationFence>,
    ) -> Result<
        awaken_resource_contract::ReplaceArtifactCaptureOutcome,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "fixture does not replace Artifact captures",
        ))
    }
}

struct RecordingScopedArtifactAuthority {
    events: Arc<Mutex<Vec<&'static str>>>,
    records: Arc<Mutex<Vec<awaken_resource_contract::FileRecord>>>,
}

#[async_trait::async_trait]
impl
    awaken_resource_contract::ArtifactPublisher<
        awaken_dispatch_contract::ArtifactPublicationFence,
    > for RecordingScopedArtifactAuthority
{
    async fn publish(
        &self,
        _publication: awaken_resource_contract::ArtifactPublication<
            awaken_dispatch_contract::ArtifactPublicationFence,
        >,
    ) -> Result<
        awaken_resource_contract::ArtifactPublicationReceipt,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "Disposing cleanup must not republish source-operation bytes",
        ))
    }

    async fn recover(
        &self,
        recovery: awaken_resource_contract::ArtifactRecovery<
            awaken_dispatch_contract::ArtifactPublicationFence,
        >,
    ) -> Result<
        Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.events.lock().unwrap().push("recover-terminal-scope");
        recovery.receipts_from_records(self.records.lock().unwrap().clone())
    }

    async fn capture(
        &self,
        _workspace_id: &str,
        _session_id: &str,
        _fence: Option<&awaken_dispatch_contract::ArtifactPublicationFence>,
    ) -> Result<
        Option<awaken_resource_contract::ArtifactCapture>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "fixture does not capture Artifacts",
        ))
    }

    async fn replace_capture(
        &self,
        _request: awaken_resource_contract::ReplaceArtifactCapture,
        _fence: Option<&awaken_dispatch_contract::ArtifactPublicationFence>,
    ) -> Result<
        awaken_resource_contract::ReplaceArtifactCaptureOutcome,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "fixture does not replace Artifact captures",
        ))
    }
}

#[tokio::test]
async fn terminal_provider_fences_use_only_the_current_same_generation_renewal() {
    // Cause/effect graph: C1 a terminal effect was admitted more than 20s ago
    // and its asserted expiry elapsed; C2 the local slot either has a live
    // monotonic renewal, no renewal, or a successor epoch; C3 the boundary is
    // preparation or destructive disposal. Effects: E1 a same-generation live
    // renewal authorizes both boundaries and projects its current expiry into
    // the provider fence; E2 an expired current generation fails closed; E3 a
    // successor epoch rejects the predecessor effect. The aggregate's durable
    // disposal preparation remains unchanged; no provider timer authority is
    // introduced.
    //
    // | Rule | asserted effect | slot current | boundary | Effect |
    // |---|---|---|---|---|
    // | R1 | expired >20s | same generation renewed/live | prepare + dispose | E1 |
    // | R2 | expired >20s | same expired lease | prepare + dispose | E2 |
    // | R3 | expired >20s | successor epoch/live | prepare + dispose | E3 |
    // R4/R5 separate clock coordinates from proof lifetime: a Control expiry
    // already behind Worker UTC plus a fresh request-relative proof permits
    // both projections; a future UTC expiry plus exhausted proof denies both.
    // R6 closed/uninstalled slots deny even with matching retained lease bytes.
    // These are Host projection decisions, not async Provider commit grants.
    let host = SharedHost::new(Arc::new(OkModel), "stub").await;
    let session_id = "terminal-renewed-provider-fence";
    let now_unix_ms = crate::terminal_repository_publication::runtime_unix_now_ms();
    let expired_lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "terminal-worker".into(),
        runtime_incarnation: "terminal-worker:boot".into(),
        epoch: 12,
        expires_at_unix_ms: now_unix_ms.saturating_sub(20_001),
    };
    let renewed_lease = awaken_session_resource_contract::SessionRealizationLease {
        expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
        ..expired_lease.clone()
    };
    let projection = remote_terminal_cleanup_projection();
    let asserted_session = terminal_test_session(
        session_id,
        &projection,
        expired_lease.clone(),
        std::iter::empty(),
        None,
    );
    let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        asserted_session
            .terminal_cleanup
            .command_for(
                &awaken_session_types::SessionId::from(session_id),
                session_id,
            )
            .expect("R1 canonical terminal command"),
        expired_lease.clone(),
    );
    let disposal = terminal_disposal_effect(
        awaken_session_state_contract::SessionCleanupPreparation::try_new(
            &effect,
            effect.sandbox_effect_fence().unwrap(),
            Vec::new(),
        )
        .unwrap(),
        expired_lease.clone(),
        &projection,
    );

    host.session_slots.update(session_id, |slot| {
        slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
            crate::session_slot::test_observed_lease(renewed_lease.clone()),
        );
    });
    assert_eq!(
        host.terminal_cleanup_effect_fence(&effect)
            .expect("R1/E1 preparation fence")
            .expires_at_unix_ms(),
        renewed_lease.expires_at_unix_ms,
        "R1/E1 preparation projects current expiry"
    );
    assert_eq!(
        host.terminal_cleanup_disposal_authorization(&disposal)
            .expect("R1/E1 disposal fence")
            .effect_fence()
            .expires_at_unix_ms(),
        renewed_lease.expires_at_unix_ms,
        "R1/E1 disposal projects current expiry"
    );

    let skewed_proof =
        awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
            expired_lease.clone(),
            expired_lease.expires_at_unix_ms - 30_000,
        )
        .unwrap()
        .observed(awaken_authority_clock::authority_proof_started().unwrap());
    host.session_slots.update(session_id, |slot| {
        slot.realization_authority =
            crate::session_slot::SessionRealizationAuthority::Live(skewed_proof);
    });
    assert_eq!(
        host.terminal_cleanup_effect_fence(&effect).unwrap(),
        effect.sandbox_effect_fence().unwrap(),
        "R4 fresh relative proof, not Worker UTC, owns preparation"
    );
    assert_eq!(
        host.terminal_cleanup_disposal_authorization(&disposal)
            .expect("R4 same relative proof owns disposal projection"),
        disposal
            .sandbox_disposal_authorization_for_current_generation(&expired_lease)
            .unwrap(),
        "R4 preserve exact aggregate preparation and successor identity"
    );
    host.session_slots.update(session_id, |slot| {
        slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
            crate::session_slot::test_expired_observed_lease(renewed_lease.clone()),
        );
    });
    assert!(host.terminal_cleanup_effect_fence(&effect).is_err(), "R5");
    assert!(
        host.terminal_cleanup_disposal_authorization(&disposal)
            .is_err(),
        "R5"
    );
    for closed in [
        crate::session_slot::SessionRealizationAuthority::Closed(Some(renewed_lease.clone())),
        crate::session_slot::SessionRealizationAuthority::Uninstalled,
    ] {
        host.session_slots
            .update(session_id, |slot| slot.realization_authority = closed);
        assert!(host.terminal_cleanup_effect_fence(&effect).is_err(), "R6");
        assert!(
            host.terminal_cleanup_disposal_authorization(&disposal)
                .is_err(),
            "R6"
        );
    }

    host.session_slots.update(session_id, |slot| {
        slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
            crate::session_slot::test_expired_observed_lease(expired_lease.clone()),
        );
    });
    assert!(
        host.terminal_cleanup_effect_fence(&effect).is_err(),
        "R2/E2"
    );
    assert!(
        host.terminal_cleanup_disposal_authorization(&disposal)
            .is_err(),
        "R2/E2"
    );

    let successor = awaken_session_resource_contract::SessionRealizationLease {
        epoch: expired_lease.epoch + 1,
        expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
        ..expired_lease
    };
    host.session_slots.update(session_id, |slot| {
        slot.realization_authority = crate::session_slot::SessionRealizationAuthority::Live(
            crate::session_slot::test_observed_lease(successor),
        );
    });
    assert!(
        host.terminal_cleanup_effect_fence(&effect).is_err(),
        "R3/E3"
    );
    assert!(
        host.terminal_cleanup_disposal_authorization(&disposal)
            .is_err(),
        "R3/E3"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_preparation_refreshes_the_fence_at_each_physical_boundary() {
    // Cause/effect graph: C1 one terminal preparation starts under lease L0;
    // C2 Artifact capture is a long I/O and Control installs same-generation
    // renewal L1 before the next physical boundary; C3 Memory acknowledgement
    // starts with L1, then blocks while Control installs L2; C4 checkpoint
    // cleanup starts with L2, then blocks while Control installs L3. Effects:
    // E1 Memory receives L1, not the pre-I/O L0 fence; E2 checkpoint receives
    // L2, not L0/L1; E3 provider preparation receives L3, not any predecessor;
    // E4 the receipt keeps work assertion L0 and separately binds provider P=L3.
    // The slot remains the single lease projection: this test adds no queue,
    // lock, timer, or independently advancing provider authority.
    //
    // | Rule | prior long I/O | current slot at boundary | Effect |
    // |---|---|---|---|
    // | R1 | Artifact under L0 | L1 | Memory receives L1 (E1) |
    // | R2 | Memory under L1 | L2 | checkpoint receives L2 (E2) |
    // | R3 | checkpoint under L2 | L3 | provider receives and receipt binds L3 (E3/E4) |
    let thread = "terminal-fence-refresh-between-effects";
    let (lease, effect) = terminal_recovery_effect(thread, "terminal-refresh-worker");
    let renewed = |expires_at_unix_ms| awaken_session_resource_contract::SessionRealizationLease {
        expires_at_unix_ms,
        ..lease.clone()
    };
    let renewal_1 = renewed(lease.expires_at_unix_ms + 10_000);
    let renewal_2 = renewed(lease.expires_at_unix_ms + 20_000);
    let renewal_3 = renewed(lease.expires_at_unix_ms + 30_000);
    let events = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::new(TerminalFenceRefreshProbe::default());
    let provider = Arc::new(TerminalRecoveryProvider {
        restored_disposals: Mutex::new(Vec::new()),
        events: events.clone(),
        scenario: TerminalRecoveryScenario::LiveMemory,
        fence_refresh_probe: Some(probe.clone()),
    });
    let raw_host = SharedHost::new(Arc::new(OkModel), "stub")
        .await
        .with_authority(
            crate::UpstreamAuthority::default().with_memory_reference_encoder(Arc::new(
                RecordingTerminalMemoryReferenceEncoder {
                    events: events.clone(),
                    realization_attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                },
            )),
        )
        .with_session_container_provider(
            provider,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
        )
        .with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
            events: events.clone(),
            expected_id: "unused-pending-checkpoint-id".into(),
            delete_gate: None,
        }));
    raw_host.install_memory_mounter(Arc::new(RecordingTerminalMemoryMounter {
        events: events.clone(),
        reject: Arc::new(AtomicBool::new(false)),
    }));
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());

    let idle_retention = awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy {
        mode: awaken_session_configuration_contract::EnvironmentIdleRetentionMode::CheckpointAndRelease,
        checkpoint_after_secs: 1,
        retention_secs: 3_600,
        expiry_behavior: Default::default(),
        max_checkpoint_bytes: 4_096,
        max_checkpoint_duration_secs: 30,
        checkpoint_format: "awaken-fs-tar-v1".into(),
    };
    let mut projection = terminal_memory_projection_with_idle_retention(idle_retention);
    let run_claim = awaken_dispatch_contract::RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("terminal-refresh-run".into()),
        owner: "terminal-refresh-run-worker".into(),
        epoch: 1,
    };
    host.install_frozen_session_projection(
        thread,
        projection.clone(),
        Some(&run_claim),
        true,
        None,
    )
    .await
    .expect("install the exact frozen Memory/checkpoint projection");
    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread))
        .with_memory_materializations(vec![terminal_memory_evidence()])
        .expect("install exact writable Memory evidence");
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(thread),
        1,
        u64::MAX,
        "terminal-refresh-environment",
        "terminal-refresh-image",
    );
    let checkpoint_operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "terminal-workspace",
        &awaken_session_types::SessionId::from(thread),
        "suspend",
        &generation,
        7,
        Some(lease.clone()),
        None,
    );
    // A changed aggregate Environment is a new root-CAS snapshot.
    projection.revision.0 += 1;
    projection.environment = awaken_session_state_contract::SessionEnvironmentState::Suspending {
        operation: checkpoint_operation,
        source_effect_id: Box::new("terminal-source-effect".into()),
        source_binding: serde_json::to_string(&handle)
            .expect("encode the exact terminal source binding"),
        generation,
        suspend_phase: awaken_session_state_contract::SuspendPhase::Uploading,
        checkpoint: None,
        source_release_preparation: None,
    };
    host.install_test_terminal_cleanup_projection(
        &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            authority: crate::session_slot::test_lease_receipt(lease.clone()),
        },
    )
    .await
    .expect("install the exact terminal generation");

    let authorization = terminal_preparation_authorization(&effect, &projection);
    let asserted_effect = effect.clone();
    let mut preparation = tokio::spawn(async move {
        managed
            .terminal_cleanup()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime",
                )
            })?
            .prepare_terminal_cleanup_for_effect(effect, authorization)
            .await
    });
    await_terminal_fence_refresh_gate(
        &probe.artifact_entered,
        &mut preparation,
        "Artifact capture",
    )
    .await;
    host.install_session_realization_lease(thread, renewal_1.clone())
        .unwrap();
    probe.artifact_release.notify_one();

    await_terminal_fence_refresh_gate(
        &probe.memory_entered,
        &mut preparation,
        "Memory acknowledgement",
    )
    .await;
    host.install_session_realization_lease(thread, renewal_2.clone())
        .unwrap();
    probe.memory_release.notify_one();

    await_terminal_fence_refresh_gate(
        &probe.checkpoint_entered,
        &mut preparation,
        "checkpoint cleanup",
    )
    .await;
    host.install_session_realization_lease(thread, renewal_3.clone())
        .unwrap();
    probe.checkpoint_release.notify_one();
    let receipt = tokio::time::timeout(std::time::Duration::from_secs(2), preparation)
        .await
        .expect("terminal preparation completes after all boundary releases")
        .expect("terminal preparation task joins")
        .expect("same-generation renewals keep the preparation authorized");
    assert_eq!(
        receipt.effect, asserted_effect,
        "R3/E4 work assertion remains L0"
    );
    assert_eq!(
        receipt.provider_prepared_effect_fence(),
        &renewal_3
            .sandbox_effect_fence(asserted_effect.operation_id())
            .unwrap(),
        "R3/E4 provider predecessor is actual L3",
    );

    let fences = probe.fences.lock().unwrap();
    assert_eq!(fences.len(), 3, "R1-R3 one fence per physical boundary");
    for ((boundary, fence), (expected_boundary, expected_lease)) in fences.iter().zip([
        ("memory", &renewal_1),
        ("checkpoint", &renewal_2),
        ("provider", &renewal_3),
    ]) {
        assert_eq!(boundary, &expected_boundary, "R1-R3 boundary order");
        assert_eq!(
            fence.expires_at_unix_ms(),
            expected_lease.expires_at_unix_ms,
            "{expected_boundary} receives the latest current fence",
        );
        assert_eq!(fence.owner(), expected_lease.owner, "same owner generation");
        assert_eq!(
            fence.runtime_incarnation(),
            expected_lease.runtime_incarnation,
            "same runtime generation",
        );
        assert_eq!(fence.epoch(), expected_lease.epoch, "same epoch generation");
    }
}

fn terminal_recovery_receipt(
    thread: &str,
    effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    file_id: &str,
) -> awaken_resource_contract::ArtifactPublicationReceipt {
    let content = b"already durable";
    let content_id = awaken_resource_contract::content_id(content);
    let artifact_effect =
        awaken_resource_contract::harvest_idempotency_key(thread, "result.txt", &content_id);
    awaken_resource_contract::ArtifactPublicationReceipt {
        effect_id: artifact_effect.clone(),
        content_id: content_id.clone(),
        record: awaken_resource_contract::FileRecord {
            id: file_id.into(),
            workspace_id: "terminal-workspace".into(),
            blob_id: content_id,
            filename: "result.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: content.len() as u64,
            created_at: "2026-08-30T00:00:00Z".into(),
            expires_at: None,
            downloadable: true,
            scope_id: Some(thread.into()),
            logical_path: Some("result.txt".into()),
            harvest_key: Some(artifact_effect),
            artifact_idempotency_scope: Some(effect.operation_id().into()),
            deleted: false,
        },
    }
}

#[tokio::test]
async fn total_absence_recovers_artifacts_before_separate_terminal_disposal() {
    /* Host terminal recovery table HS2. Causes: C1 exact V2 source and every
     * typed physical participant are absent after disposal response loss; C2
     * the aggregate terminal effect remains live; C3 Resources already owns a
     * durable Artifact publication receipt. Effects: E1 provider returns no
     * Environment, so Host performs no live output read; E2 the existing
     * ArtifactHarvester invokes publisher.recover under the exact terminal
     * fence; E3 preparation carries that receipt without physical deletion; E4
     * a later aggregate-derived disposal returns exact absence evidence with no
     * live/source-dependent I/O. Rule HS2 C1+C2+C3=>E1->E2->E3; durable
     * preparation=>E4. No cleanup-only Sandbox or second receipt registry exists. */
    let thread = "total-absence-artifact-recovery";
    let (lease, effect) = terminal_recovery_effect(thread, "total-absence-worker");
    let receipt = terminal_recovery_receipt(thread, &effect, "file_total_absence_receipt");
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        restored_disposals: Mutex::new(Vec::new()),
        events: events.clone(),
        scenario: TerminalRecoveryScenario::TotalAbsence,
        fence_refresh_probe: None,
    });
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub")
        .await
        .with_session_container_provider(
            provider,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
        );
    raw_host.resources.artifact_publisher = Arc::new(RecordingTerminalArtifactRecovery {
        events: events.clone(),
        receipt: receipt.clone(),
    });
    let host = Arc::new(raw_host);
    let managed = install_test_dispatch_runtime(&host);
    install_test_session_application(&host);
    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread));
    let binding = serde_json::to_string(&handle).unwrap();
    let terminal_projection = install_terminal_recovery_projection(
        &host,
        &managed,
        thread,
        lease.clone(),
        awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding,
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    )
    .await;

    let preparation_authorization =
        terminal_preparation_authorization(&effect, &terminal_projection);
    let preparation = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .prepare_terminal_cleanup_for_effect(effect.clone(), preparation_authorization)
        .await
        .expect("HS2 total-absence terminal preparation");
    assert_eq!(preparation.artifact_receipts, vec![receipt], "HS2/E3");
    assert_eq!(
        preparation.provider_prepared_effect_fence(),
        &effect.sandbox_effect_fence().unwrap(),
        "HS2/E3 no provider boundary uses the final current fence",
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            "observe-total-absence",
            "prepare-total-absence",
            "recover-artifacts",
        ],
        "HS2/E1->E2"
    );
    assert!(
        host.session_slots.contains(thread),
        "HS2 preparation is not disposal"
    );

    events.lock().unwrap().clear();
    let disposal_effect = terminal_disposal_effect(preparation, lease, &terminal_projection);
    let disposal = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
        .await
        .expect("HS2 total-absence physical-disposal proof");
    assert_eq!(disposal.effect_id, disposal_effect.command.effect_id);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &["observe-total-absence", "prepare-total-absence"],
        "HS2 disposal performs only effect-free reconstruction and exact absence proof",
    );
}

#[tokio::test]
async fn disposing_cleanup_prepares_durability_before_resuming_physical_disposal() {
    /* Host terminal I/O table HS3. Causes: C1 the exact V2 realization is
     * observed Disposing, which is provider evidence that every required
     * source-durability effect preceded its cleanup gate without asserting a
     * terminal-scoped Artifact association; C2 the aggregate terminal fence
     * remains live; C3 Resources owns an already-durable source-operation File
     * with no terminal idempotency scope; C4 aggregate state carries a committed
     * checkpoint object. Effects: E1 Host discards any executable owner and
     * preserves the continuation's exact provider preparation A without
     * replaying raw terminal preparation T; E2 it does zero
     * live artifact/container/checkpoint I/O, preserves C3, and does not invent
     * a terminal receipt association; E3 it
     * deletes C4 through the existing checkpoint store without reading the Pod;
     * E4 only an aggregate-derived typed authorization then resumes exact
     * physical disposal; E5 preparation carries no fabricated receipt. Ordinary Ready/Terminal observations use
     * Live I/O (existing terminal tests); total absence is HS2; incompatible
     * orphan state is HS1.
     *
     * | Rule | observation | receipt/checkpoint | live I/O | dispose | Effect |
     * | H1 | Ready/Terminal | any | allowed | after durable effects | existing Live path |
     * | H2 | Disposing with durable continuation A | prior unscoped/exact | forbidden | receipt recovery, then A/fpA + live successor disposal | E1+E2+E3+E4+E5 |
     * | H3 | absent | exact/any | forbidden | none | HS2 |
     * | H4 | incompatible | any | forbidden | none | HS1 | */
    let thread = "disposing-artifact-recovery";
    let (lease, effect) = terminal_recovery_effect(thread, "disposing-worker");
    let mut source_file =
        terminal_recovery_receipt(thread, &effect, "file_disposing_source").record;
    source_file.artifact_idempotency_scope = None;
    let source_records = Arc::new(Mutex::new(vec![source_file.clone()]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        restored_disposals: Mutex::new(Vec::new()),
        events: events.clone(),
        scenario: TerminalRecoveryScenario::Disposing,
        fence_refresh_probe: None,
    });
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub")
        .await
        .with_session_container_provider(
            provider,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
        );
    raw_host.resources.artifact_publisher = Arc::new(RecordingScopedArtifactAuthority {
        events: events.clone(),
        records: source_records.clone(),
    });
    let checkpoint_id = "checkpoint-disposing".to_string();
    raw_host =
        raw_host.with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
            events: events.clone(),
            expected_id: checkpoint_id.clone(),
            delete_gate: None,
        }));
    let host = Arc::new(raw_host);
    let managed = install_test_dispatch_runtime(&host);
    install_test_session_application(&host);
    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread));
    let (disposing_environment, source_preparation, source_preparation_receipt) =
        disposing_terminal_environment(thread, &lease, &handle, &checkpoint_id);
    let terminal_projection = install_terminal_recovery_projection(
        &host,
        &managed,
        thread,
        lease.clone(),
        disposing_environment,
    )
    .await;

    let preparation_authorization =
        terminal_preparation_authorization(&effect, &terminal_projection);
    let preparation = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .prepare_terminal_cleanup_for_effect(effect.clone(), preparation_authorization)
        .await
        .expect("HS3 prepare exact disposing cleanup");
    assert!(
        preparation.artifact_receipts.is_empty(),
        "HS3/E2 terminal recovery cannot associate an unscoped source File"
    );
    assert_eq!(
        source_records.lock().unwrap().as_slice(),
        std::slice::from_ref(&source_file),
        "HS3/E2 prior source publication remains durable"
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            "observe-disposing",
            "prepare-disposing",
            "recover-terminal-scope",
            "delete-checkpoint",
        ],
        "HS3/E1->E2->E3; preparation must not physically dispose"
    );
    assert!(
        !events.lock().unwrap().contains(&"dispose-physical"),
        "HS3 physical disposal waits for the aggregate preparation CAS",
    );

    let disposal_effect = terminal_disposal_effect(preparation, lease, &terminal_projection);
    assert_eq!(
        disposal_effect
            .command
            .provider_disposal
            .prepared_effect_fence(),
        &source_preparation.sandbox_effect_fence().unwrap(),
        "HS3/E1 physical disposal inherits continuation A exactly",
    );
    assert_eq!(
        disposal_effect
            .command
            .provider_disposal
            .preparation_fingerprint(),
        source_preparation_receipt.receipt_fingerprint(),
        "HS3/E1 physical disposal inherits continuation fpA exactly",
    );
    let disposal = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
        .await
        .expect("HS3 resume exact physical disposal");
    assert_eq!(disposal.effect_id, disposal_effect.command.effect_id);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            "observe-disposing",
            "prepare-disposing",
            "recover-terminal-scope",
            "delete-checkpoint",
            "observe-disposing",
            "dispose-physical",
        ],
        "HS3/E1->E2->E3->E4; disposal repeats no source-dependent effect",
    );
}

#[tokio::test]
async fn inherited_preparation_closes_absent_writable_memory_response_loss() {
    /* Host continuation-takeover table HS3b. Causes: C1 the aggregate root
     * carries a verified Disposing source receipt and inherited provider
     * preparation A; C2 the provider now proves the physical source totally
     * absent after response loss; C3 the durable handle/frozen transition join
     * names exact writable Copy evidence; C4 Artifact publication and the
     * committed checkpoint are independently durable. Effects: E1 the closed
     * root authorization, not provider observation, selects AlreadyPrepared;
     * E2 Host performs receipt-only Artifact recovery and checkpoint deletion,
     * but zero live Memory read, acknowledgement, or raw terminal provider
     * preparation; E3 aggregate disposal inherits A/fpA and treats exact total
     * absence as physical response-loss success.
     *
     * | Rule | aggregate A | provider observation | RW evidence | Effect |
     * | A1 | exact | DefinitivelyUnavailable | exact | E1 -> E2 -> E3 |
     * | A2 | none | DefinitivelyUnavailable+aux | exact | HM0 fail closed |
     * | A3 | exact | Disposing | exact/RO | HS3 recover and exact re-ack |
     * | A4 | none | Ready/Terminal | exact | HM1 live reconciliation |
     *
     * Thus neither observation nor a naked boolean is a second preparation
     * authority; only Control's effect-bound closed value can override A2. */
    let thread = "disposing-absent-writable-memory";
    let (lease, effect) = terminal_recovery_effect(thread, "disposing-absent-worker");
    let mut source_file =
        terminal_recovery_receipt(thread, &effect, "file_disposing_absent_source").record;
    source_file.artifact_idempotency_scope = None;
    let source_records = Arc::new(Mutex::new(vec![source_file.clone()]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        restored_disposals: Mutex::new(Vec::new()),
        events: events.clone(),
        scenario: TerminalRecoveryScenario::TotalAbsence,
        fence_refresh_probe: None,
    });
    let checkpoint_id = "checkpoint-disposing-absent";
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub")
        .await
        .with_session_container_provider(
            provider,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
        )
        .with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
            events: events.clone(),
            expected_id: checkpoint_id.into(),
            delete_gate: None,
        }));
    raw_host.resources.artifact_publisher = Arc::new(RecordingScopedArtifactAuthority {
        events: events.clone(),
        records: source_records.clone(),
    });
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());

    let mut projection = terminal_memory_projection();
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &managed,
        thread,
        projection.clone(),
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("A1 install frozen writable Memory input");
    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread))
        .with_memory_materializations(vec![terminal_memory_evidence()])
        .expect("A1 durable handle carries exact writable Copy evidence");
    let (disposing_environment, source_preparation, source_preparation_receipt) =
        disposing_terminal_environment(thread, &lease, &handle, checkpoint_id);
    // A changed aggregate Environment is a new root-CAS snapshot.
    projection.revision.0 += 1;
    projection.environment = disposing_environment;
    host.install_test_terminal_cleanup_projection(
        &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            authority: crate::session_slot::test_lease_receipt(lease.clone()),
        },
    )
    .await
    .expect("A1 install exact Disposing assignment");

    let authorization = terminal_preparation_authorization(&effect, &projection);
    let expected_provider_preparation =
        awaken_provisioning_contract::SandboxDisposalPreparation::new(
            source_preparation.sandbox_effect_fence().unwrap(),
            source_preparation_receipt.receipt_fingerprint(),
        )
        .unwrap();
    assert_eq!(
        authorization.inherited_provider_disposal(),
        Some(&expected_provider_preparation),
        "A1/E1 closed authorization carries exact continuation A/fpA",
    );
    let preparation = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .prepare_terminal_cleanup_for_effect(effect, authorization)
        .await
        .expect("A1 exact inherited preparation closes physical response loss");
    assert!(preparation.artifact_receipts.is_empty(), "A1/E2");
    assert_eq!(
        source_records.lock().unwrap().as_slice(),
        std::slice::from_ref(&source_file),
        "A1/E2 preserves the unscoped source publication",
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            "observe-total-absence",
            "prepare-total-absence",
            "recover-terminal-scope",
            "delete-checkpoint",
        ],
        "A1/E2 zero live Memory/ack/raw provider preparation/physical disposal",
    );

    events.lock().unwrap().clear();
    let disposal_effect = terminal_disposal_effect(preparation, lease, &projection);
    assert_eq!(
        disposal_effect.command.provider_disposal, expected_provider_preparation,
        "A1/E3 aggregate disposal keeps exact A/fpA",
    );
    managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .dispose_terminal_cleanup_for_effect(disposal_effect)
        .await
        .expect("A1/E3 total absence is exact physical response-loss success");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        ["observe-total-absence", "prepare-total-absence"],
        "A1/E3 physical phase performs observation/reconstruction only",
    );
}

#[tokio::test]
async fn unavailable_auxiliary_writable_memory_fails_before_terminal_effects() {
    /* Host unavailable-Memory table HM0. Causes: C1 provider proves the primary
     * Sandbox definitively absent but reconstructs an exact auxiliary owner;
     * C2 the durable V2 handle carries one writable Copy materialization; C3
     * the frozen Resource transition names that same RW MemoryStore; C4 no
     * prior provider disposal preparation/gate exists. Effects: E1 the shared
     * Option-aware Memory join recognizes the exact writable intent; E2 Host
     * fails closed before Artifact publication/recovery, Memory acknowledgement,
     * checkpoint cleanup, provider preparation, or physical disposal; E3 the
     * reconstructed exact auxiliary owner remains available for retry/takeover.
     *
     * | Rule | primary | auxiliary | Memory | prior prep | Effect |
     * | M0 | absent | exact | RW Copy | none | E1 -> E2 + E3 |
     * | M1 | absent | none | none/RO | none | HS2 receipt-only recovery |
     * | M2 | Disposing gate | exact | exact evidence | yes | HS3 recovery/ack |
     * | M3 | Ready/Terminal | live | RW Copy | none | HM1 live reconciliation |
     *
     * Observation and effect-free reconstruction are provider evidence reads,
     * not source effects; the exact event list therefore contains only C1. */
    // M4/M5 cross the same absent-primary evidence with a retained Candidate
    // before commit / after a lost commit response. Every retry must reach the
    // same classified Memory boundary without relabeling or replacing its Arc.
    for candidate_unmaterialized in [None, Some(true), Some(false)] {
        let thread = "unavailable-auxiliary-writable-memory";
        let events = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(TerminalRecoveryProvider {
            restored_disposals: Mutex::new(Vec::new()),
            events: events.clone(),
            scenario: TerminalRecoveryScenario::UnavailableAuxiliaryMemory,
            fence_refresh_probe: None,
        });
        let host = Arc::new(
            SharedHost::new(Arc::new(OkModel), "stub")
                .await
                .with_session_container_provider(
                    provider,
                    Arc::new(crate::session_environment::UnusedHandExecutorFactory),
                ),
        );
        let managed = managed_with_resource_source(host.clone());

        let mut projection = terminal_memory_projection();
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &managed,
            thread,
            projection.clone(),
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await
        .expect("M0 install frozen writable Memory input");

        let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread))
            .with_memory_materializations(vec![terminal_memory_evidence()])
            .expect("M0 durable handle carries exact writable Copy evidence");
        let binding = serde_json::to_string(&handle).expect("M0 encode durable binding");
        // A changed aggregate Environment is a new root-CAS snapshot.
        projection.revision.0 += 1;
        projection.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding,
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        let (lease, effect) = terminal_recovery_effect(thread, "unavailable-memory-worker");
        let mut retained = None;
        if let Some(unmaterialized) = candidate_unmaterialized {
            let fence = lease
                .sandbox_effect_fence("unacknowledged-auxiliary-create")
                .unwrap();
            let environment = Arc::new(
                host.env
                    .session_provider
                    .terminal()
                    .prepare_terminal_effective_for_effect(
                        &host.sandbox_spec(thread),
                        Some(&handle),
                        None,
                        &fence,
                        &pc::test_support::EffectAuthority::new(&fence),
                    )
                    .await
                    .unwrap()
                    .unwrap(),
            );
            host.begin_session_environment_preparation(thread, environment.clone())
                .unwrap();
            retained = Some(environment);
            if unmaterialized {
                projection.environment =
                    awaken_session_state_contract::SessionEnvironmentState::Unmaterialized;
            }
        }
        host.install_test_terminal_cleanup_projection(
            &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                session_id: thread.into(),
                projection: projection.clone(),
                authority: crate::session_slot::test_lease_receipt(lease),
            },
        )
        .await
        .expect("M0 install exact terminal assignment");

        // M0 retry must retain the first exact auxiliary owner, including its
        // non-durable participants; primary absence is not permission to replace it.
        for attempt in 0..2 {
            events.lock().unwrap().clear();
            let preparation_authorization =
                terminal_preparation_authorization(&effect, &projection);
            let error = managed
                .terminal_cleanup()
                .ok_or_else(|| {
                    awaken_session_error_contract::RunError::unavailable(
                        "TerminalCleanupRuntime is unsupported by this runtime",
                    )
                })
                .expect("runtime role")
                .prepare_terminal_cleanup_for_effect(effect.clone(), preparation_authorization)
                .await
                .expect_err("M0 unavailable writable Memory must fail before source effects");
            assert_eq!(
                error.kind,
                awaken_session_error_contract::RunErrorKind::Unavailable,
                "M0/E2",
            );
            assert_eq!(
                error.code, "session_terminal_memory_source_absent",
                "M0/E2 retry {attempt}"
            );
            assert_eq!(
                events.lock().unwrap().as_slice(),
                [
                    "observe-unavailable-memory",
                    "reconstruct-unavailable-memory"
                ],
                "M0/E2 publish/ack/checkpoint/provider-prep/dispose are all zero",
            );
            assert!(
                host.session_slots
                    .read(thread, |slot| {
                        match &slot.environment_owner {
                            crate::session_slot::SessionEnvironmentOwner::Retiring(retiring) => {
                                let environment = retiring.owned.environment();
                                if let Some(previous) = &retained {
                                    assert!(
                                        Arc::ptr_eq(previous, &environment),
                                        "M0/M4/M5 preserve exact Arc and participants"
                                    );
                                }
                                retained = Some(environment);
                                if candidate_unmaterialized.is_some() {
                                    assert!(
                                        matches!(&retiring.owned,
                                crate::session_slot::RetiringEnvironmentOwner::Unbound(candidate)
                                if candidate.durable_identity().is_none()),
                                        "M4/M5 no fabricated commitment"
                                    );
                                }
                                true
                            }
                            _ => false,
                        }
                    })
                    .unwrap_or(false),
                "M0/E3 exact auxiliary owner remains Retiring for retry/takeover"
            );
        }
    }
}

#[tokio::test]
async fn provider_proved_unmaterialized_absence_closes_missing_memory_evidence() {
    /* Host unmaterialized-Memory table HM0b. Causes: C1 frozen inputs contain
     * RW MemoryStore; C2 the aggregate is Unmaterialized with no binding; C3
     * the provider atomically proves the exact create-fenced root absent and
     * publishes its Removed tombstone. Effects: E1 C3 upgrades unknown `None`
     * to explicit `Some([])` at the existing Option-aware contract join; E2 no
     * Memory reconciliation intent or live source I/O is produced; E3 the
     * Artifact phase proves the exact empty batch without a publisher call; E4
     * the terminal projection remains for the separately authorized disposal.
     * Rule HM0b C1+C2+C3=>E1+E2->E3+E4.
     * Constraint: without C3, contract rule T1 still rejects `None`+RW as
     * ambiguous; the contract decision-table test owns that negative row. */
    let thread = "proved-absent-terminal-memory-evidence";
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub").await;
    raw_host.resources.artifact_publisher = Arc::new(RecordingScopedArtifactAuthority {
        events: events.clone(),
        records: Arc::new(Mutex::new(Vec::new())),
    });
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());

    let projection = terminal_memory_projection();
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &managed,
        thread,
        projection.clone(),
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("HM0b install frozen writable Memory input");
    let (lease, effect) = terminal_recovery_effect(thread, "legacy-memory-worker");
    host.install_test_terminal_cleanup_projection(
        &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            authority: crate::session_slot::test_lease_receipt(lease),
        },
    )
    .await
    .expect("HM0b install exact terminal assignment");

    let preparation_authorization = terminal_preparation_authorization(&effect, &projection);
    let preparation = managed
        .terminal_cleanup()
        .ok_or_else(|| {
            awaken_session_error_contract::RunError::unavailable(
                "TerminalCleanupRuntime is unsupported by this runtime",
            )
        })
        .expect("runtime role")
        .prepare_terminal_cleanup_for_effect(effect, preparation_authorization)
        .await
        .expect("HM0b exact provider absence closes missing handle evidence");
    assert!(preparation.artifact_receipts.is_empty(), "HM0b/E3");
    assert!(
        events.lock().unwrap().is_empty(),
        "HM0b/E2+E3 no Artifact publisher or Memory source effect"
    );
    assert!(host.session_slots.contains(thread), "HM0b/E4");
}

#[tokio::test]
async fn resident_and_cold_terminal_memory_use_one_terminal_v2_authority() {
    /* Host terminal Memory table HM1. Provider tests own fresh Copy-guard
     * creation and exact acknowledgement semantics; this table owns the Host
     * caller shared by a resident same-process Environment and a cold
     * reconstruction after Worker loss. Causes: C1 the V2 binding carries one
     * complete writable Copy evidence M; C2 the frozen aggregate input exactly
     * joins M; C3 the Environment is resident/cold; C4 the Host constructor
     * selected the database-less Worker role. Its frozen input was materialized
     * under an exact Run claim whose RunV1 reference expires at terminal time.
     * Effects: E1 one live Artifact batch precedes Memory; E2 the Sandbox copy
     * is read once; E3 only SessionTerminalMemoryIntent is encoded and
     * reconciled; E4 the exact complete evidence is acknowledged after E3 and
     * provider preparation returns without deletion; E5 only a separately
     * aggregate-derived authorization physically disposes. A Disposing retry instead uses HS3's
     * ReceiptOnly/zero-live-I/O row, while foreign evidence and takeover are
     * owned by NM2 and the shared acknowledgement kernel.
     *
     * | Rule | M/input | owner | encoded reference | Effect |
     * | H1 | exact RW | resident | terminal-v2 only | E1->E2->E3->E4->E5 |
     * | H2 | exact RW | cold | terminal-v2 only | prepare->E1->E2->E3->E4->E5 |
     * | H3 | missing/foreign | any | none | fail closed (contract tests) |
     * | H4 | Disposing | cleanup-only | none | HS3 | */
    // H5/H6 resident/cold + rejected Memory writeback (including copy UTF-8
    // admission failure) => repeated error, no acknowledgement, preparation or
    // disposal receipt, exact durable binding retained. After the participant
    // succeeds, the SAME terminal effect resumes through E3->E4->E5. The port
    // fixture controls failure; Memory's SQLite U2-U5 table owns byte admission.
    // H7/H8 extend the same participants to unacknowledged Candidates, with
    // root Unmaterialized / Resident (lost response), crossed with rejection.
    // Unmaterialized forbids Artifact harvesting but still reconciles the exact
    // mounted Memory; neither state fabricates ordinary commit identity or
    // admits LiveSource. Failed writeback must retain the same physical owner.
    #[derive(Clone, Copy)]
    struct Rule {
        name: &'static str,
        resident: bool,
        rejection: bool,
        candidate_unmaterialized: Option<bool>,
    }

    for rule in [
        Rule {
            name: "resident",
            resident: true,
            rejection: false,
            candidate_unmaterialized: None,
        },
        Rule {
            name: "cold",
            resident: false,
            rejection: false,
            candidate_unmaterialized: None,
        },
        Rule {
            name: "resident-rejected",
            resident: true,
            rejection: true,
            candidate_unmaterialized: None,
        },
        Rule {
            name: "cold-rejected",
            resident: false,
            rejection: true,
            candidate_unmaterialized: None,
        },
        Rule {
            name: "candidate-unmaterialized",
            resident: false,
            rejection: false,
            candidate_unmaterialized: Some(true),
        },
        Rule {
            name: "candidate-lost-response",
            resident: false,
            rejection: false,
            candidate_unmaterialized: Some(false),
        },
        Rule {
            name: "candidate-unmaterialized-rejected",
            resident: false,
            rejection: true,
            candidate_unmaterialized: Some(true),
        },
        Rule {
            name: "candidate-lost-response-rejected",
            resident: false,
            rejection: true,
            candidate_unmaterialized: Some(false),
        },
    ] {
        let thread = format!("terminal-memory-{}", rule.name);
        let events = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(TerminalRecoveryProvider {
            restored_disposals: Mutex::new(Vec::new()),
            events: events.clone(),
            scenario: TerminalRecoveryScenario::LiveMemory,
            fence_refresh_probe: None,
        });
        let raw_host = SharedHost::new(Arc::new(OkModel), "stub")
            .await
            .with_authority(
                crate::UpstreamAuthority::default().with_memory_reference_encoder(Arc::new(
                    RecordingTerminalMemoryReferenceEncoder {
                        events: events.clone(),
                        realization_attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    },
                )),
            )
            .with_session_container_provider(
                provider,
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            );
        let reject = Arc::new(AtomicBool::new(rule.rejection));
        raw_host.install_memory_mounter(Arc::new(RecordingTerminalMemoryMounter {
            events: events.clone(),
            reject: reject.clone(),
        }));
        let host = Arc::new(raw_host);
        let managed = managed_with_resource_source(host.clone());

        let mut projection = terminal_memory_projection();
        let resources = projection.resources.clone();
        let run_claim = awaken_dispatch_contract::RunClaim {
            run_id: awaken_agent_contract::agent::run::Id(format!(
                "terminal-memory-{}-run",
                rule.name
            )),
            owner: "terminal-memory-run-worker".into(),
            epoch: 1,
        };
        host.install_frozen_session_projection(
            &thread,
            projection.clone(),
            Some(&run_claim),
            true,
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("{} installs frozen Memory input: {error}", rule.name));

        let evidence = terminal_memory_evidence();
        let handle = terminal_fixture_container_handle(&thread, &host.sandbox_spec(&thread))
            .with_memory_materializations(vec![evidence])
            .expect("HM1 current V2 handle carries exact Memory evidence");
        let binding = serde_json::to_string(&handle).expect("HM1 encode terminal binding");
        // A changed aggregate Environment is a new root-CAS snapshot.
        projection.revision.0 += 1;
        projection.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: binding.clone(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        let (lease, effect) = terminal_recovery_effect(&thread, "terminal-memory-worker");
        if let Some(unmaterialized) = rule.candidate_unmaterialized {
            let fence = lease
                .sandbox_effect_fence("unacknowledged-memory-create")
                .unwrap();
            let environment = host
                .env
                .session_provider
                .terminal()
                .prepare_terminal_effective_for_effect(
                    &host.sandbox_spec(&thread),
                    Some(&handle),
                    None,
                    &fence,
                    &pc::test_support::EffectAuthority::new(&fence),
                )
                .await
                .unwrap()
                .unwrap();
            host.begin_session_environment_preparation(&thread, Arc::new(environment))
                .unwrap();
            if unmaterialized {
                projection.environment =
                    awaken_session_state_contract::SessionEnvironmentState::Unmaterialized;
            }
        }
        host.install_test_terminal_cleanup_projection(
            &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                session_id: SessionId::from(&thread),
                projection: projection.clone(),
                authority: crate::session_slot::test_lease_receipt(lease.clone()),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{} installs terminal assignment: {error}", rule.name));

        if rule.resident {
            let warm_fence = lease
                .sandbox_effect_fence("resident-terminal-memory-preparation")
                .expect("HM1 live resident preparation fence");
            host.prepare_bound_environment_for_effect_under_lifecycle(
                &thread,
                &binding,
                &warm_fence,
                Some(&resources),
                crate::host::BoundEnvironmentPreparationMode::LiveSource,
            )
            .await
            .unwrap_or_else(|error| panic!("{} prepares resident owner: {error}", rule.name));
            assert!(
                host.session_slots
                    .read(&thread, |slot| {
                        slot.environment_owner
                            .terminal_bound_environment()
                            .is_some()
                    })
                    .unwrap_or(false),
                "H1/C3 exact same-process owner is retained behind Preparation"
            );
        } else {
            assert!(host.session_environment(&thread).await.is_none(), "H2/C3");
        }
        events.lock().unwrap().clear();

        if rule.rejection {
            for _ in 0..2 {
                let authorization = terminal_preparation_authorization(&effect, &projection);
                let error = managed
                    .terminal_cleanup()
                    .ok_or_else(|| {
                        awaken_session_error_contract::RunError::unavailable(
                            "TerminalCleanupRuntime is unsupported by this runtime",
                        )
                    })
                    .expect("runtime role")
                    .prepare_terminal_cleanup_for_effect(effect.clone(), authorization)
                    .await
                    .expect_err("H5/H6 rejected Memory cannot prepare disposal");
                assert!(error.message.contains("Memory reconciliation rejected"));
                assert_eq!(
                    events.lock().unwrap().last(),
                    Some(&"reconcile-terminal-v2")
                );
                for forbidden in ["ack-memory", "prepare-source", "dispose-physical"] {
                    assert!(
                        !events.lock().unwrap().contains(&forbidden),
                        "H5/H6 {forbidden}"
                    );
                }
                assert_eq!(
                    host.session_slots
                        .read(&thread, |slot| {
                            slot.environment_owner.physical_binding().map(str::to_owned)
                        })
                        .flatten()
                        .as_deref(),
                    Some(binding.as_str()),
                    "H5/H6 exact binding retained"
                );
                events.lock().unwrap().clear();
            }
            reject.store(false, Ordering::SeqCst);
        }

        let preparation_authorization = terminal_preparation_authorization(&effect, &projection);
        let preparation = managed
            .terminal_cleanup()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime",
                )
            })
            .expect("runtime role")
            .prepare_terminal_cleanup_for_effect(effect.clone(), preparation_authorization)
            .await
            .unwrap_or_else(|error| panic!("{} terminal Memory cleanup: {error}", rule.name));
        assert!(preparation.artifact_receipts.is_empty(), "HM1/E1");
        // A rejected cold attempt retains its exact reconstructed Environment;
        // retry must reuse it, not materialize a fresh projection over the source.
        let mut preparation_events =
            if rule.resident || rule.rejection || rule.candidate_unmaterialized.is_some() {
                vec![
                    "observe-live-memory",
                    "capture-artifacts",
                    "read-terminal-memory",
                    "encode-terminal-v2",
                    "reconcile-terminal-v2",
                    "ack-memory",
                    "prepare-source",
                ]
            } else {
                vec![
                    "observe-live-memory",
                    "prepare-live-memory",
                    "capture-artifacts",
                    "read-terminal-memory",
                    "encode-terminal-v2",
                    "reconcile-terminal-v2",
                    "ack-memory",
                    "prepare-source",
                ]
            };
        if rule.candidate_unmaterialized == Some(true) {
            preparation_events.retain(|event| *event != "capture-artifacts");
        }
        assert_eq!(
            events.lock().unwrap().as_slice(),
            preparation_events,
            "HM1 {} preparation",
            rule.name,
        );
        assert!(
            !events.lock().unwrap().contains(&"dispose-physical"),
            "HM1/E5 aggregate preparation must precede physical disposal",
        );
        assert!(
            !events.lock().unwrap().contains(&"encode-run-v1"),
            "HM1/E3 old RunV1 reference is never reused"
        );
        assert!(
            host.session_slots
                .read(&thread, |slot| {
                    matches!(
                        &slot.environment_owner,
                        crate::session_slot::SessionEnvironmentOwner::Retiring(_)
                    )
                })
                .unwrap_or(false),
            "HM1/E4 exact owner remains Retiring until Disposal"
        );
        if rule.candidate_unmaterialized.is_some() {
            assert!(host.session_slots.read(&thread, |slot| matches!(
                &slot.environment_owner,
                crate::session_slot::SessionEnvironmentOwner::Retiring(crate::session_slot::RetiringSessionEnvironment {
                    owned: crate::session_slot::RetiringEnvironmentOwner::Unbound(candidate), ..
                }) if candidate.durable_identity().is_none()
            )).unwrap(), "H7/H8 no invented commit evidence");
            let before = events.lock().unwrap().clone();
            let error = host
                .prepare_bound_environment_for_effect_under_lifecycle(
                    &thread,
                    &binding,
                    &effect.sandbox_effect_fence().unwrap(),
                    Some(&resources),
                    crate::host::BoundEnvironmentPreparationMode::LiveSource,
                )
                .await
                .err()
                .expect("H7/H8 retired Candidate cannot become a live source");
            assert_eq!(error.code, "session_terminal_unpublished_candidate");
            let error = host
                .adopt_bound_session_environment(
                    &thread,
                    Some(&binding),
                    &host.env.session_provider,
                    None,
                    false,
                )
                .await
                .expect_err("H7/H8 terminal Candidate cannot ordinarily adopt");
            assert!(error.message.contains("cannot resume ordinary adoption"));
            assert_eq!(
                *events.lock().unwrap(),
                before,
                "H7/H8 reject before Provider observation or I/O"
            );
        }

        let disposal_effect = terminal_disposal_effect(preparation, lease, &projection);
        let disposal = managed
            .terminal_cleanup()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "TerminalCleanupRuntime is unsupported by this runtime",
                )
            })
            .expect("runtime role")
            .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
            .await
            .unwrap_or_else(|error| panic!("{} terminal physical disposal: {error}", rule.name));
        assert_eq!(disposal.effect_id, disposal_effect.command.effect_id);
        assert_eq!(
            events.lock().unwrap().last(),
            Some(&"dispose-physical"),
            "HM1/E5 physical disposal is the final effect",
        );
        assert!(host.session_environment(&thread).await.is_none(), "HM1/E5");
    }
}
