//! Restart and terminal Memory participant replay across Local, Namespace, and Container.

use super::*;

struct RestartMemoryMounter {
    outcomes: std::sync::Mutex<std::collections::VecDeque<pc::Realization>>,
    mount_calls: Arc<std::sync::atomic::AtomicUsize>,
    teardown_calls: Arc<std::sync::atomic::AtomicUsize>,
    teardown_failures: Arc<std::sync::atomic::AtomicUsize>,
    response_gate: std::sync::Mutex<Option<(usize, Arc<tokio::sync::Semaphore>)>>,
}

struct RestartMemoryMount {
    realization: pc::Realization,
    teardown_calls: Arc<std::sync::atomic::AtomicUsize>,
    teardown_failures: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl pc::MemoryMounter for RestartMemoryMounter {
    async fn mount(
        &self,
        _store_id: &str,
        host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> pc::MemoryMountOutcome {
        let result: Result<Box<dyn pc::MemoryMount>, pc::SandboxError> = async {
            let call = self.mount_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let gate = self.response_gate.lock().unwrap().clone();
            if let Some((pending_call, gate)) = gate
                && call == pending_call
            {
                let _permit = gate.acquire().await.unwrap();
            }
            let realization = self
                .outcomes
                .lock()
                .expect("restart Memory outcomes lock poisoned")
                .pop_front()
                .ok_or_else(|| pc::SandboxError::backend("mount", "unexpected Memory remount"))?;
            std::fs::create_dir_all(host_path).map_err(|error| {
                pc::SandboxError::backend(
                    "mount",
                    format!("create fake Memory projection: {error}"),
                )
            })?;
            std::fs::write(host_path.join("value.txt"), b"durable-memory").map_err(|error| {
                pc::SandboxError::backend("mount", format!("write fake Memory projection: {error}"))
            })?;
            Ok(Box::new(RestartMemoryMount {
                realization,
                teardown_calls: self.teardown_calls.clone(),
                teardown_failures: self.teardown_failures.clone(),
            }) as Box<dyn pc::MemoryMount>)
        }
        .await;
        pc::MemoryMountOutcome::from_no_effect(result)
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
        reference: &str,
        evidence: &awaken_provisioning_contract::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: awaken_provisioning_contract::MountAccess,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let _ = (reference, evidence, files, access);
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "recovered Memory copy reconciliation is outside this fixture",
        ))
    }
}

#[async_trait]
impl pc::MemoryMount for RestartMemoryMount {
    fn realization(&self) -> pc::Realization {
        self.realization
    }

    fn materialization_heads(&self) -> Option<Vec<pc::MemoryMaterializationHead>> {
        (self.realization == pc::Realization::Copy).then(|| {
            vec![pc::MemoryMaterializationHead {
                path: "value.txt".into(),
                id: "head".into(),
                version: 1,
                content_sha256: "digest".into(),
            }]
        })
    }

    async fn teardown(&self) -> Result<(), pc::SandboxError> {
        self.teardown_calls.fetch_add(1, Ordering::SeqCst);
        if self
            .teardown_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(pc::SandboxError::backend(
                "teardown",
                "scripted Memory teardown failure",
            ));
        }
        Ok(())
    }
}

fn install_restart_memory_mounter(
    provider: SessionEnvironmentProvider,
    outcomes: impl IntoIterator<Item = pc::Realization>,
    mount_calls: Arc<std::sync::atomic::AtomicUsize>,
    teardown_calls: Arc<std::sync::atomic::AtomicUsize>,
    teardown_failures: Arc<std::sync::atomic::AtomicUsize>,
) -> SessionEnvironmentProvider {
    provider
        .installation()
        .install_memory_mounter(Arc::new(RestartMemoryMounter {
            outcomes: std::sync::Mutex::new(outcomes.into_iter().collect()),
            mount_calls,
            teardown_calls,
            teardown_failures,
            response_gate: std::sync::Mutex::new(None),
        }));
    provider
}

fn restart_namespace_provider(
    root: &std::path::Path,
    outcomes: impl IntoIterator<Item = pc::Realization>,
    mount_calls: Arc<std::sync::atomic::AtomicUsize>,
    teardown_calls: Arc<std::sync::atomic::AtomicUsize>,
) -> SessionEnvironmentProvider {
    let provider = SandboxEnvironmentProvider::namespace_with_agent_stderr(
        root,
        false,
        Arc::new(FakeHandExecutorFactory),
        "/bin/sh",
        std::time::Duration::ZERO,
    )
    .into_session_provider();
    install_restart_memory_mounter(
        provider,
        outcomes,
        mount_calls,
        teardown_calls,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
}

fn restart_memory_spec(scope: &str) -> (pc::SandboxSpec, pc::MountRequirement) {
    let memory = pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/mnt/memory/restart".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    };
    let mut spec = spec();
    spec.scope = scope.into();
    spec.mounts = vec![memory.clone()];
    (spec, memory)
}

#[tokio::test]
async fn local_and_namespace_restart_replay_exact_memory_participants_before_late_mounts() {
    // Restart Memory cause/effect table. Causes: C0 adoption has/has no frozen
    // spec; C1 provider is Local/Namespace and replay entry is Ready-create/
    // adopt; C2 receipt realization is Fuse/Copy; C3 canonical mounter returns
    // the same/different realization; C4 a frozen requirement is covered/not
    // covered by the receipt. Rules: M0 !C0+Fuse fails closed with zero remount;
    // M1 either C1+Fuse+same + live Provider => reuse exactly one retained guard;
    // actual Provider replacement is covered by the cold-current table below.
    // M2 C0+Copy => retain exact durable
    // evidence/files and perform no mount; M3 C0+Fuse+different => reject and
    // teardown the acquired participant; M4 covered C4 => adopted reconciliation
    // is a no-op, while uncovered C4 uses the existing attach port. M2/M3 use
    // Namespace as the provider-neutral helper's representative caller; M0/M1
    // exercise both callers. Ordinary attach over either retained Memory
    // participant remains rejected, so recovery never weakens replacement
    // fencing or creates a second Memory authority.
    // M5 repeats the exact replay/late-attach matrix for TrustedHost. A rebound
    // provider must preserve the ACP cwd and prompt interpretation and must not
    // acquire an extra Memory participant when a read-only File arrives later.
    for (namespace, trusted, label) in [
        (false, false, "local"),
        (true, false, "namespace"),
        (false, true, "trusted-host"),
    ] {
        let fuse_root = tempfile::tempdir().unwrap();
        let fuse_mounts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fuse_teardowns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let base_provider = if namespace {
            SandboxEnvironmentProvider::namespace_with_agent_stderr(
                fuse_root.path(),
                false,
                Arc::new(FakeHandExecutorFactory),
                "/bin/sh",
                std::time::Duration::ZERO,
            )
            .into_session_provider()
        } else if trusted {
            SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(
                fuse_root.path(),
                false,
            )
            .into_session_provider()
        } else {
            SandboxEnvironmentProvider::workdir(fuse_root.path()).into_session_provider()
        };
        let fuse_provider = install_restart_memory_mounter(
            base_provider,
            [
                pc::Realization::Fuse,
                pc::Realization::Fuse,
                pc::Realization::Fuse,
            ],
            fuse_mounts.clone(),
            fuse_teardowns.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let (fuse_spec, fuse_memory) = restart_memory_spec(&format!("{label}-restart-fuse"));
        let fuse_spec = fuse_provider.realizer().effective_spec(&fuse_spec).unwrap();
        let fuse_effect = pc::SandboxEffectFence::new(
            format!("create-fuse-{label}"),
            "owner",
            "runtime",
            1,
            u64::MAX,
        )
        .unwrap();
        let created = fuse_provider
            .realizer()
            .create_effective_for_effect(
                &fuse_spec,
                awaken_provisioning_contract::SandboxEffect::Fenced {
                    fence: &fuse_effect,
                    authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        &fuse_effect,
                    ),
                },
                None,
                awaken_session_environment_contract::EnvironmentRealizationKind::Create,
            )
            .await
            .expect("M1 initial Fuse participant");
        let fuse_handle = created.lifecycle().handle();
        let before = created
            .agent_tools()
            .agent_filesystem_view(Some("repo/src"), AgentFilesystemClient::AcpProcess);
        drop(created);
        let unfenced = match fuse_provider.sandbox_provider() {
            SandboxEnvironmentProvider::Workdir { provider, .. } => {
                provider.adopt_sandbox(&fuse_handle).await.map(|_| ())
            }
            SandboxEnvironmentProvider::Namespace { provider, .. } => {
                provider.adopt_sandbox(&fuse_handle).await.map(|_| ())
            }
            _ => unreachable!("restart fixture selects only Local/Namespace"),
        };
        let error = unfenced.expect_err("M0 unfenced FUSE adoption must fail closed");
        assert!(
            error
                .to_string()
                .contains("FUSE Memory replay requires the frozen Sandbox spec"),
            "M0 {label}: {error}"
        );
        assert_eq!(fuse_mounts.load(Ordering::SeqCst), 1, "M0 {label}");
        let ready_replay = fuse_provider
            .realizer()
            .create_effective_for_effect(
                &fuse_spec,
                awaken_provisioning_contract::SandboxEffect::Fenced {
                    fence: &fuse_effect,
                    authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        &fuse_effect,
                    ),
                },
                None,
                awaken_session_environment_contract::EnvironmentRealizationKind::Create,
            )
            .await
            .expect("M1 Ready replay reuses Fuse");
        assert_eq!(
            ready_replay.lifecycle().handle(),
            fuse_handle,
            "M1 {label} binding"
        );
        drop(ready_replay);
        let adopted = fuse_provider
            .adoption()
            .adopt_effective_for_effect(
                &fuse_spec,
                &fuse_handle,
                awaken_provisioning_contract::SandboxEffect::Fenced {
                    fence: &fuse_effect,
                    authority: &pc::test_support::EffectAuthority::new(&fuse_effect),
                },
            )
            .await
            .expect("M1 adoption reuses Fuse");
        assert_eq!(fuse_mounts.load(Ordering::SeqCst), 1, "M1 {label}");
        let after = adopted
            .agent_tools()
            .agent_filesystem_view(Some("repo/src"), AgentFilesystemClient::AcpProcess);
        assert_eq!(before.protocol_cwd, after.protocol_cwd, "M5 stable cwd");
        assert_eq!(
            before.command_cwd, after.command_cwd,
            "M5 stable process cwd"
        );
        assert_eq!(
            before.prompt_paths, after.prompt_paths,
            "M5 stable prompt view"
        );

        if namespace || trusted {
            let late = pc::MountRequirement {
                mount_id: "late".into(),
                source: pc::MountSource::InlineBytes {
                    contents: b"late".to_vec(),
                    content_hash: None,
                },
                mount_path: "/workspace/late.txt".into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::Session,
                required: true,
            };
            adopted
                .projection()
                .reconcile_adopted_mounts(&[fuse_memory.clone(), late])
                .await
                .expect("M4 only the late mount attaches");
            assert_eq!(fuse_mounts.load(Ordering::SeqCst), 1, "M4 no remount");
            let files = if trusted {
                adopted
                    .harvest()
                    .list_frozen_mount_files("/workspace")
                    .await
            } else {
                adopted.sandbox_environment().list_workspace_files("").await
            }
            .unwrap();
            assert!(
                files
                    .iter()
                    .any(|(path, bytes)| path == "late.txt" && bytes == b"late"),
                "M4 late mount is visible"
            );
            assert!(
                adopted
                    .projection()
                    .attach_mount(fuse_memory)
                    .await
                    .is_err(),
                "ordinary runtime replacement remains fenced"
            );
        }
        assert_eq!(fuse_teardowns.load(Ordering::SeqCst), 0, "M1/M4 {label}");
    }

    let copy_root = tempfile::tempdir().unwrap();
    let copy_mounts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let copy_teardowns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let copy_provider = restart_namespace_provider(
        copy_root.path(),
        [pc::Realization::Copy],
        copy_mounts.clone(),
        copy_teardowns,
    );
    let (copy_spec, copy_memory) = restart_memory_spec("namespace-restart-copy");
    let copy_spec = copy_provider.realizer().effective_spec(&copy_spec).unwrap();
    let copy_effect =
        pc::SandboxEffectFence::new("create-copy", "owner", "runtime", 1, u64::MAX).unwrap();
    let created = copy_provider
        .realizer()
        .create_effective_for_effect(
            &copy_spec,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &copy_effect,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &copy_effect,
                ),
            },
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .expect("M2 initial Copy participant");
    let copy_handle = created.lifecycle().handle();
    drop(created);
    let SandboxEnvironmentProvider::Namespace {
        provider: copy_namespace,
        ..
    } = copy_provider.sandbox_provider()
    else {
        unreachable!("copy fixture is Namespace")
    };
    let unfenced_copy = copy_namespace
        .adopt_sandbox(&copy_handle)
        .await
        .expect("M2 unfenced exact Copy remains adoptable");
    assert_eq!(
        unfenced_copy.list_files(&copy_memory.mount_path).unwrap(),
        vec![("value.txt".into(), b"durable-memory".to_vec())],
        "M2 unfenced Copy bytes"
    );
    drop(unfenced_copy);
    let copy_replay = copy_provider
        .realizer()
        .create_effective_for_effect(
            &copy_spec,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &copy_effect,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &copy_effect,
                ),
            },
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .expect("M2 Ready replay retains Copy");
    drop(copy_replay);
    let copy_adopted = copy_provider
        .adoption()
        .adopt_effective_for_effect(
            &copy_spec,
            &copy_handle,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &copy_effect,
                authority: &pc::test_support::EffectAuthority::new(&copy_effect),
            },
        )
        .await
        .expect("M2 adoption retains Copy");
    copy_adopted
        .projection()
        .reconcile_adopted_mounts(std::slice::from_ref(&copy_memory))
        .await
        .expect("M2 exact Copy is already reconciled");
    assert_eq!(copy_mounts.load(Ordering::SeqCst), 1, "M2 zero remount");
    assert_eq!(
        copy_adopted
            .harvest()
            .list_frozen_mount_files(&copy_memory.mount_path)
            .await
            .unwrap(),
        vec![("value.txt".into(), b"durable-memory".to_vec())],
        "M2 surviving copy bytes"
    );

    let drift_root = tempfile::tempdir().unwrap();
    let drift_mounts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let drift_teardowns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let drift_provider = restart_namespace_provider(
        drift_root.path(),
        [pc::Realization::Fuse, pc::Realization::Copy],
        drift_mounts.clone(),
        drift_teardowns.clone(),
    );
    let (drift_spec, _) = restart_memory_spec("namespace-restart-drift");
    let drift_spec = drift_provider
        .realizer()
        .effective_spec(&drift_spec)
        .unwrap();
    let drift_effect =
        pc::SandboxEffectFence::new("create-drift", "owner", "runtime", 1, u64::MAX).unwrap();
    let created = drift_provider
        .realizer()
        .create_effective_for_effect(
            &drift_spec,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &drift_effect,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &drift_effect,
                ),
            },
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .expect("M3 initial Fuse participant");
    drop(created);
    // M3 is a COLD realization drift, not wrapper loss. Replace the actual
    // Provider so its retained guard cannot correctly satisfy the replay.
    drop(drift_provider);
    let drift_provider = restart_namespace_provider(
        drift_root.path(),
        [pc::Realization::Copy],
        drift_mounts.clone(),
        drift_teardowns.clone(),
    );
    let error = match drift_provider
        .realizer()
        .create_effective_for_effect(
            &drift_spec,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &drift_effect,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &drift_effect,
                ),
            },
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
    {
        Ok(_) => panic!("M3 realization drift must fail"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("changed the exact Ready realization"),
        "M3 {error}"
    );
    assert_eq!(drift_mounts.load(Ordering::SeqCst), 2, "M3");
    assert_eq!(drift_teardowns.load(Ordering::SeqCst), 1, "M3");
}

#[tokio::test]
async fn local_and_namespace_terminal_fuse_replay_is_exact_and_wrapper_loss_convergent() {
    // Terminal Memory decision table. Causes: C1 provider is Local on every
    // host, or Namespace on Linux/macOS where the OS isolator exists;
    // C2 source is an exact handle / handle-free expected restore effect; C3
    // marker is Ready/Removing/authorized Removing/Removed; C4 receipt is
    // Fuse/Copy; C5 successor is live/foreign; C6 teardown succeeds/fails once.
    // T1 hot Fuse uses its retained guard (one mount); T2 wrapper-loss Ready and
    // response-loss Removing reuse exactly one Fuse guard across attempts for
    // both providers; T3 !C5 rejects before a second mount and leaves Ready;
    // T4 an authorization-WAL + teardown failure remains retryable, and
    // authorized Removing retains the failed guard until disposal succeeds; T5
    // Removed is idempotent with zero remount. The provider-level
    // `terminal_memory_reconstruction_accepts_only_exact_copy_evidence` test
    // owns Copy's cold zero-remount/exact-evidence row; the shared replay helper
    // test owns post-mount validation plus teardown-failure compensation.

    let hot_root = tempfile::tempdir().unwrap();
    let hot_mounts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hot_teardowns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hot_provider = install_restart_memory_mounter(
        SandboxEnvironmentProvider::workdir(hot_root.path()).into_session_provider(),
        [pc::Realization::Fuse],
        hot_mounts.clone(),
        hot_teardowns.clone(),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    let (hot_spec, _) = restart_memory_spec("terminal-fuse-hot");
    let hot_spec = hot_provider.realizer().effective_spec(&hot_spec).unwrap();
    let hot_create =
        pc::SandboxEffectFence::new("create-hot", "owner", "runtime", 2, u64::MAX).unwrap();
    let hot_terminal =
        pc::SandboxEffectFence::new("terminal-hot", "owner", "runtime", 2, u64::MAX).unwrap();
    let hot = hot_provider
        .realizer()
        .create_effective_for_effect(
            &hot_spec,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &hot_create,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &hot_create,
                ),
            },
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .expect("T1 hot Fuse create");
    hot.lifecycle()
        .acknowledge_memory_reconciliation(
            &hot_terminal,
            &[],
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&hot_terminal),
        )
        .await
        .expect("T1 exact empty Fuse evidence");
    let prepared = hot
        .lifecycle()
        .prepare_disposal_for_effect(
            &hot_terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&hot_terminal),
        )
        .await
        .expect("T1 prepared");
    let authorization = restart_disposal_authorization(prepared, "terminal-fuse-hot");
    hot.lifecycle()
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .expect("T1 dispose");
    assert_eq!(hot_mounts.load(Ordering::SeqCst), 1, "T1 no remount");
    assert_eq!(hot_teardowns.load(Ordering::SeqCst), 1, "T1 one teardown");

    let cases = [
        (false, "local"),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        (true, "namespace"),
    ];
    for (namespace, label) in cases {
        let root = tempfile::tempdir().unwrap();
        let mounts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let teardowns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let teardown_failures = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let base_provider = if namespace {
            SandboxEnvironmentProvider::namespace_with_agent_stderr(
                root.path(),
                false,
                Arc::new(FakeHandExecutorFactory),
                "/bin/sh",
                std::time::Duration::ZERO,
            )
            .into_session_provider()
        } else {
            SandboxEnvironmentProvider::workdir(root.path()).into_session_provider()
        };
        let provider = install_restart_memory_mounter(
            base_provider,
            [
                pc::Realization::Fuse,
                pc::Realization::Fuse,
                pc::Realization::Fuse,
                pc::Realization::Fuse,
            ],
            mounts.clone(),
            teardowns.clone(),
            teardown_failures,
        );
        let (spec, memory) = restart_memory_spec(&format!("terminal-fuse-{label}"));
        let spec = provider.realizer().effective_spec(&spec).unwrap();
        let create =
            pc::SandboxEffectFence::new(format!("create-{label}"), "owner", "runtime", 2, u64::MAX)
                .unwrap();
        let terminal = pc::SandboxEffectFence::new(
            format!("terminal-{label}"),
            "owner",
            "runtime",
            2,
            u64::MAX,
        )
        .unwrap();
        let created = provider
            .realizer()
            .create_effective_for_effect(
                &spec,
                awaken_provisioning_contract::SandboxEffect::Fenced {
                    fence: &create,
                    authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        &create,
                    ),
                },
                None,
                awaken_session_environment_contract::EnvironmentRealizationKind::Create,
            )
            .await
            .expect("T2 create Fuse");
        let handle = created.lifecycle().handle();
        drop(created);
        let request_handle = (!namespace).then_some(&handle);
        let foreign = pc::SandboxEffectFence::new(
            format!("foreign-{label}"),
            "owner",
            "runtime",
            1,
            u64::MAX,
        )
        .unwrap();
        assert!(
            provider
                .terminal()
                .prepare_terminal_effective_for_effect(
                    &spec,
                    request_handle,
                    Some(&create),
                    &foreign,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(&foreign)
                )
                .await
                .is_err(),
            "T3 {label}",
        );
        assert_eq!(mounts.load(Ordering::SeqCst), 1, "T3 {label} zero remount");

        let first = provider
            .terminal()
            .prepare_terminal_effective_for_effect(
                &spec,
                request_handle,
                Some(&create),
                &terminal,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
            )
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "T2 {label} Ready terminal preflight: {error:?}; mounts={}, teardowns={}",
                    mounts.load(Ordering::SeqCst),
                    teardowns.load(Ordering::SeqCst)
                )
            })
            .expect("T2 Ready participant");
        assert_eq!(
            first
                .harvest()
                .list_frozen_mount_files(&memory.mount_path)
                .await
                .unwrap(),
            vec![("value.txt".into(), b"durable-memory".to_vec())],
            "T2 {label} mounted bytes",
        );
        drop(first); // response loss after Ready -> Removing and FUSE replay

        let second = provider
            .terminal()
            .prepare_terminal_effective_for_effect(
                &spec,
                request_handle,
                Some(&create),
                &terminal,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
            )
            .await
            .expect("T2 Removing terminal preflight")
            .expect("T2 Removing participant");
        assert_eq!(mounts.load(Ordering::SeqCst), 1, "T2 {label}");
        second
            .lifecycle()
            .acknowledge_memory_reconciliation(
                &terminal,
                &[],
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
            )
            .await
            .expect("T4 exact empty Fuse evidence");
        let prepared = second
            .lifecycle()
            .prepare_disposal_for_effect(
                &terminal,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
            )
            .await
            .expect("T4 durable preparation");
        let authorization =
            restart_disposal_authorization(prepared, &format!("terminal-fuse-{label}"));
        assert!(
            second
                .lifecycle()
                .dispose_for_effect(
                    &authorization,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        authorization.effect_fence()
                    )
                )
                .await
                .is_err(),
            "T4 first teardown fails after authorization WAL",
        );
        assert_eq!(teardowns.load(Ordering::SeqCst), 1, "T4 {label}");
        drop(second); // wrapper loss retains both authorization and Provider-owned guard

        let replay = provider
            .terminal()
            .prepare_terminal_effective_for_effect(
                &spec,
                request_handle,
                Some(&create),
                authorization.effect_fence(),
                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    authorization.effect_fence(),
                ),
            )
            .await
            .expect("T4 authorized Removing preflight")
            .expect("T4 authorized Removing participant");
        assert_eq!(mounts.load(Ordering::SeqCst), 1, "T4 {label} same guard");
        replay
            .lifecycle()
            .dispose_for_effect(
                &authorization,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    authorization.effect_fence(),
                ),
            )
            .await
            .expect("T4 response-loss disposal converges");
        assert_eq!(teardowns.load(Ordering::SeqCst), 2, "T4 {label} retry");
        assert!(
            provider
                .terminal()
                .prepare_terminal_effective_for_effect(
                    &spec,
                    request_handle,
                    Some(&create),
                    authorization.effect_fence(),
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        authorization.effect_fence()
                    )
                )
                .await
                .expect("T5 exact Removed observation")
                .is_none(),
            "T5 {label}",
        );
        assert_eq!(mounts.load(Ordering::SeqCst), 1, "T5 {label} zero remount");
    }
}

#[tokio::test]
async fn cold_terminal_memory_rechecks_current_authority_after_mount_responses() {
    // Cause/effect table: Local/Namespace x first/last FUSE response x revoked,
    // replaced, expired, renewed, cancelled = 20 actual Pending schedules.
    // R1 entry denial => zero takeover/remount. R2 denial after response => no
    // NEXT mount or successful reconstruction and no unauthorized compensation.
    // R3 renewal => both mounts and exact bytes. R4 dropped caller => no NEXT
    // request; the live cold Provider retains the same mount future. Authorized
    // retry resumes/compensates only the unpublished guard, reuses any confirmed
    // first guard, and remounts only the missing participant. Actual Provider
    // replacement precedes the cold attempt; wrapper Drop alone is NOT restart.
    // Copy zero-remount/Removed and corrupt-evidence rows remain in the existing
    // replay tables. UTC is deliberately non-expiring; current proof owns R1/R2.
    for namespace in [false, true] {
        for pause_at in [3, 4] {
            for change in ["revoke", "replace", "expire", "renew", "cancel"] {
                let root = tempfile::tempdir().unwrap();
                let provider = if namespace {
                    SandboxEnvironmentProvider::namespace_with_agent_stderr(
                        root.path(),
                        false,
                        Arc::new(FakeHandExecutorFactory),
                        "/bin/sh",
                        std::time::Duration::ZERO,
                    )
                    .into_session_provider()
                } else {
                    SandboxEnvironmentProvider::workdir(root.path()).into_session_provider()
                };
                let mounts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let teardowns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mounter = Arc::new(RestartMemoryMounter {
                    outcomes: std::sync::Mutex::new(
                        std::iter::repeat_n(pc::Realization::Fuse, 8).collect(),
                    ),
                    mount_calls: mounts.clone(),
                    teardown_calls: teardowns.clone(),
                    teardown_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    response_gate: std::sync::Mutex::new(None),
                });
                provider
                    .installation()
                    .install_memory_mounter(mounter.clone());
                let (mut spec, memory) = restart_memory_spec("cold-current-memory");
                let mut second = memory.clone();
                second.mount_id = "memory-2".into();
                second.mount_path = "/mnt/memory/second".into();
                spec.mounts.push(second);
                let spec = provider.realizer().effective_spec(&spec).unwrap();
                let create =
                    pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX - 1)
                        .unwrap();
                let terminal =
                    pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX - 1)
                        .unwrap();
                let created = provider
                    .realizer()
                    .create_effective_for_effect(
                        &spec,
                        awaken_provisioning_contract::SandboxEffect::Fenced {
                            fence: &create,
                            authority:
                                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                                    &create,
                                ),
                        },
                        None,
                        awaken_session_environment_contract::EnvironmentRealizationKind::Create,
                    )
                    .await
                    .unwrap();
                let handle = created.lifecycle().handle();
                drop(created);
                let cold_provider = crate::test_support::relocated_provider(&provider, root.path());
                drop(provider);
                let provider = cold_provider;
                provider
                    .installation()
                    .install_memory_mounter(mounter.clone());
                assert_eq!(mounts.load(Ordering::SeqCst), 2);
                let denied = pc::test_support::EffectAuthority::denied();
                assert!(
                    provider
                        .terminal()
                        .prepare_terminal_effective_for_effect(
                            &spec,
                            Some(&handle),
                            None,
                            &terminal,
                            &denied
                        )
                        .await
                        .is_err()
                );
                assert_eq!(mounts.load(Ordering::SeqCst), 2, "R1");
                let authority = pc::test_support::EffectAuthority::new(&terminal);
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                *mounter.response_gate.lock().unwrap() = Some((pause_at, gate.clone()));
                let mut pending = Box::pin(tokio::task::unconstrained(
                    provider.terminal().prepare_terminal_effective_for_effect(
                        &spec,
                        Some(&handle),
                        None,
                        &terminal,
                        &authority,
                    ),
                ));
                std::future::poll_fn(|cx| {
                    assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                assert_eq!(mounts.load(Ordering::SeqCst), pause_at);
                {
                    let mut current = authority.current.lock().unwrap();
                    match change {
                        "revoke" => *current = None,
                        "replace" => current.as_mut().unwrap().0.reassign(),
                        "expire" => current.as_mut().unwrap().1 = std::time::Duration::ZERO,
                        "renew" => current.as_mut().unwrap().0.renew(),
                        _ => {}
                    }
                }
                if change == "cancel" {
                    drop(pending);
                } else {
                    gate.add_permits(1);
                    assert_eq!(
                        pending.await.is_ok(),
                        change == "renew",
                        "{namespace}/{pause_at}/{change}"
                    );
                }
                assert_eq!(
                    mounts.load(Ordering::SeqCst),
                    if change == "renew" { 4 } else { pause_at }
                );
                if !matches!(change, "renew" | "cancel") {
                    assert_eq!(
                        teardowns.load(Ordering::SeqCst),
                        0,
                        "R2 denied authority cannot compensate returned guards"
                    );
                }
                *mounter.response_gate.lock().unwrap() = None;
                gate.add_permits(1); // the retained cancelled future still owns this gate
                *authority.current.lock().unwrap() =
                    Some((terminal.clone(), std::time::Duration::from_secs(1)));
                let replay = provider
                    .terminal()
                    .prepare_terminal_effective_for_effect(
                        &spec,
                        Some(&handle),
                        None,
                        &terminal,
                        &authority,
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    mounts.load(Ordering::SeqCst),
                    if change == "renew" { 4 } else { 5 }
                );
                assert_eq!(
                    teardowns.load(Ordering::SeqCst),
                    usize::from(change != "renew")
                );
                for mount in &spec.mounts {
                    assert_eq!(
                        replay
                            .harvest()
                            .list_frozen_mount_files(&mount.mount_path)
                            .await
                            .unwrap(),
                        vec![("value.txt".into(), b"durable-memory".to_vec())]
                    );
                }
                replay
                    .lifecycle()
                    .acknowledge_memory_reconciliation(&terminal, &[], &authority)
                    .await
                    .unwrap();
                let prepared = replay
                    .lifecycle()
                    .prepare_disposal_for_effect(&terminal, &authority)
                    .await
                    .unwrap();
                let authorization = restart_disposal_authorization(prepared, "cold-current-memory");
                replay
                    .lifecycle()
                    .dispose_for_effect(
                        &authorization,
                        &pc::test_support::EffectAuthority::new(authorization.effect_fence()),
                    )
                    .await
                    .unwrap();
            }
        }
    }
}

#[tokio::test]
async fn provider_rebasing_adoption_and_container_mount_jail_cover_every_tier() {
    // Provider-projection cause/effect rules. C1=a provider is rebased;
    // C2=a current fenced Namespace handle is adopted; C3=a container mount
    // escapes its jail; C4=aggregate disposal preparation is not yet/is durably
    // accepted; C5=the closed observation carries the exact/foreign physical
    // incarnation. E1=preserve the provider kind/configuration; E2=preparation
    // quiesces and binds the exact owner but does not remove it; E3=physical
    // disposal removes only the exact evidenced root after C4; E4=reject C3
    // before provider create; E5=accept only exact terminal evidence while
    // retaining the filesystem-absence rule. Rules: P1=C1=>E1;
    // P2=C1+C2+!C4+exact-C5=>E1+E2+E5; P3=C1+C2+C4=>E3;
    // P4=C1+C3=>E4; P5=C1+C2+foreign-C5=>reject before disposal.
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();

    let workdir = SandboxEnvironmentProvider::workdir(first.path()).at_root(second.path());
    assert!(matches!(
        workdir,
        SandboxEnvironmentProvider::Workdir { .. }
    ));

    let namespace = SandboxEnvironmentProvider::namespace_with_agent_stderr(
        first.path(),
        true,
        Arc::new(FakeHandExecutorFactory),
        "/bin/sh",
        std::time::Duration::ZERO,
    )
    .at_root(second.path());
    assert!(matches!(
        &namespace,
        SandboxEnvironmentProvider::Namespace { provider, .. }
            if provider.inherits_agent_stderr()
    ));
    let mut namespace_spec = spec();
    namespace_spec.scope = "provider-namespace-adopt".into();
    let namespace_spec = namespace.effective_spec(&namespace_spec).unwrap();
    let create_fence =
        pc::SandboxEffectFence::new("create", "provider-test", "runtime", 1, u64::MAX).unwrap();
    let created = namespace
        .create_effective_for_effect(
            &namespace_spec,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &create_fence,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &create_fence,
                ),
            },
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .unwrap();
    let handle = created.lifecycle().handle();
    drop(created);
    let adopted = namespace
        .adopt_effective_for_effect(
            &namespace_spec,
            &handle,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &create_fence,
                authority: &pc::test_support::EffectAuthority::new(&create_fence),
            },
        )
        .await
        .unwrap();
    assert_eq!(adopted.lifecycle().handle(), handle);
    let terminal_fence =
        pc::SandboxEffectFence::new("terminal", "provider-test", "runtime", 1, u64::MAX).unwrap();
    adopted
        .lifecycle()
        .prepare_disposal_for_effect(
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .unwrap();
    let terminal_observation = namespace
        .observe_effective(
            &namespace_spec,
            &handle,
            None,
            pc::SandboxObservationBasis::Effect {
                spec: &namespace_spec,
                effect_fence: &terminal_fence,
            },
            awaken_provisioning_contract::SandboxObservationPurpose::TerminalCleanup,
        )
        .await
        .unwrap();
    assert!(
        matches!(
            terminal_observation,
            pc::SandboxObservation::Terminal { .. }
        ),
        "P2 preparation retains the exact physical root without forging aggregate disposal"
    );
    namespace
        .validate_closed_observation(&handle, &terminal_observation)
        .expect("P2/P5 exact marker incarnation is the sole closed evidence");
    assert!(
        namespace
            .validate_closed_observation(
                &handle,
                &pc::SandboxObservation::Terminal {
                    physical_incarnation: "foreign-physical-incarnation".into(),
                },
            )
            .is_err(),
        "P5 foreign terminal evidence fails closed",
    );
    assert!(
        namespace
            .validate_closed_observation(
                &handle,
                &pc::SandboxObservation::DefinitivelyUnavailable {
                    physical_incarnation: handle.filesystem_physical_incarnation().unwrap(),
                },
            )
            .is_err(),
        "P5 filesystem absence cannot reuse a once-live incarnation",
    );
    let preparation_fingerprint = "provider-test-preparation";
    let disposal_preparation =
        pc::SandboxDisposalPreparation::new(terminal_fence.clone(), preparation_fingerprint)
            .unwrap();
    let disposal_id = disposal_preparation.operation_id().unwrap();
    let disposal_fence =
        pc::SandboxEffectFence::new(disposal_id, "provider-test", "runtime", 1, u64::MAX).unwrap();
    let authorization = disposal_preparation.authorize(disposal_fence).unwrap();
    adopted
        .lifecycle()
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .unwrap();

    let provider = Arc::new(FakeContainerProvider::default());
    let unsafe_mount = pc::MountRequirement {
        mount_id: "unsafe".into(),
        source: pc::MountSource::Inline {
            contents: "value".into(),
        },
        mount_path: "../escape".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    };
    let container = SandboxEnvironmentProvider::container(
        provider,
        vec![unsafe_mount],
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .at_root(second.path());
    assert!(matches!(
        container,
        SandboxEnvironmentProvider::Container { .. }
    ));
    assert!(
        container
            .into_session_provider()
            .create(&spec())
            .await
            .is_err()
    );
}
