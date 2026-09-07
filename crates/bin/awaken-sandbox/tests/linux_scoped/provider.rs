use super::*;
use awaken_provisioning_contract::{CurrentExecutionAuthority, Sandbox as _};
use awaken_sandbox_local::LocalProvider;
use std::sync::{Arc, Mutex};

#[path = "repository.rs"]
mod repository;

struct Authority(
    Mutex<(Option<Duration>, usize, bool)>,
    Mutex<Option<AuthorityPause>>,
);

struct AuthorityPause {
    call: usize,
    reached: tokio::sync::oneshot::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
    failure: Option<std::io::ErrorKind>,
}

impl Authority {
    fn live() -> Arc<Self> {
        Arc::new(Self(
            Mutex::new((Some(Duration::from_secs(20)), 0, false)),
            Mutex::new(None),
        ))
    }
}

impl CurrentExecutionAuthority for Authority {
    fn with_current(
        &self,
        effect: &mut dyn FnMut(Duration) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut current = self.0.lock().unwrap();
        current.1 += 1;
        let pause = {
            let mut pause = self.1.lock().unwrap();
            if pause.as_ref().is_some_and(|pause| pause.call == current.1) {
                pause.take()
            } else {
                None
            }
        };
        if let Some(pause) = pause {
            pause.reached.send(()).unwrap();
            pause.resume.recv_timeout(Duration::from_secs(4)).unwrap();
            if let Some(error) = pause.failure {
                return Err(error.into());
            }
        }
        if current.2 && current.1 >= 2 {
            current.0 = None;
        }
        effect(current.0.ok_or(std::io::ErrorKind::PermissionDenied)?)
    }
}

fn spec() -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false, // Worker capability stays unavailable.
        scope: "scoped-provider".into(),
        isolation: pc::IsolationClass::Workdir,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: vec![pc::MountRequirement {
            mount_id: "file".into(),
            mount_path: "/mnt/session/file".into(),
            source: pc::MountSource::InlineBytes {
                contents: b"exact\0file\xff".to_vec(),
                content_hash: None,
            },
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::Session,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
    }
}

fn fence(operation: &str) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new(operation, "owner", "runtime", 1, u64::MAX).unwrap()
}

#[tokio::test]
async fn configured_execution_probe_uses_the_real_boundary_and_keeps_admission_closed() {
    // D1 exact installed binaries -> real JOIN/GO/tree-exit readiness. D2 missing
    // launcher/helper or incompatible helper -> error, never version-only success.
    // D3 provider probe reuses that exact deployment; newly realized scope is
    // selected but cannot spawn until its Session authority binds. No capability
    // bit is inferred from this narrower probe.
    use awaken_sandbox_local::LocalExecutionDeployment as Deployment;
    // D4 a trusted helper that never acknowledges handoff is deadline-bounded;
    // its timeout cannot produce readiness or install any business authority.
    use std::os::unix::fs::PermissionsExt;
    let stalled = tempfile::tempdir().unwrap();
    let helper = stalled.path().join("stalled-helper");
    std::fs::write(&helper, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(8),
            Deployment::new("/usr/bin/bwrap".into(), helper)
                .unwrap()
                .probe_ready()
        )
        .await
        .unwrap()
        .is_err()
    );
    for (launcher, helper) in [
        ("/missing-launcher", sandbox_binary()),
        ("/usr/bin/bwrap", "/missing-helper"),
        ("/usr/bin/bwrap", "/usr/bin/true"),
    ] {
        let deployment = Deployment::new(launcher.into(), helper.into()).unwrap();
        assert!(deployment.probe_ready().await.is_err());
    }
    let deployment = Deployment::discover(sandbox_binary().into()).unwrap();
    deployment.probe_ready().await.unwrap();
    let base = tempfile::tempdir().unwrap();
    let mut provider = LocalProvider::trusted_host(base.path());
    provider
        .install_execution_deployment(deployment.clone())
        .unwrap();
    pc::SandboxProvider::probe_ready(&provider).await.unwrap();
    assert_eq!(provider.execution_deployment(), Some(&deployment));
    assert!(!pc::SandboxProvider::capabilities(&provider).process_tree_supervision);
    let sandbox = provider
        .create_sandbox_for_effect(&spec(), &fence("create"), None)
        .await
        .unwrap();
    assert!(sandbox.has_scoped_execution() && sandbox.needs_execution_authority());
    assert!(
        sandbox
            .spawn(pc::Command::new(["/usr/bin/true"]))
            .await
            .is_err()
    );
    sandbox.bind_execution_authority(Authority::live()).unwrap();
    let process = sandbox
        .spawn(pc::Command::new(["/usr/bin/true"]))
        .await
        .unwrap();
    assert_eq!(process.wait().await.unwrap().code, Some(0));
    dispose(&sandbox).await;
}

fn tree(port: u16) -> pc::Command {
    let material = tree_command(port);
    let mut command = pc::Command::new(material.argv);
    command.cwd = "/workspace/nested 工作目录".into();
    command.stdio = pc::Stdio::Null;
    command.env = material
        .env
        .into_iter()
        .map(|var| pc::EnvVar {
            name: var.name,
            value: pc::EnvValue::Inline {
                value: var.value.expose().into(),
            },
            visibility: pc::EnvVisibility::Process,
        })
        .collect();
    command
}

fn marker(base: &Path) -> serde_json::Value {
    serde_json::from_slice(
        &std::fs::read(base.join(".scoped-provider.awaken-realization.json")).unwrap(),
    )
    .unwrap()
}

async fn dispose(sandbox: &awaken_sandbox_local::LocalSandbox) {
    let prepared = sandbox
        .prepare_disposal_for_effect(
            &fence("dispose"),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence("dispose")),
        )
        .await
        .unwrap();
    let preparation =
        pc::SandboxDisposalPreparation::new(prepared, "test-execution-preparation").unwrap();
    let authorization = preparation
        .authorize(fence(&preparation.operation_id().unwrap()))
        .unwrap();
    sandbox
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .unwrap();
    assert!(!sandbox.workspace_path().exists());
}

#[tokio::test]
async fn provider_spawn_records_all_trees_and_cold_adoption_waits_for_exact_exit() {
    // P1 current exact TrustedHost + live authority => both spawn and ACP's
    // spawn_agent share pre-GO WAL, exact cwd and File bytes. P2 two overlapping
    // trees => two participants; cold adopt/create replay
    // cannot touch their root. P3 one tree exits => remaining participant still
    // blocks. P4 all trees exit => cold adoption succeeds with unchanged bytes.
    // P5 closed admission cannot be reopened by refresh or another spawn.
    // The authority here is a deterministic port fixture, NOT the Session root.
    let base = tempfile::tempdir().unwrap();
    let provider = LocalProvider::trusted_host(base.path());
    let sandbox = provider
        .create_sandbox_for_effect(&spec(), &fence("create"), None)
        .await
        .unwrap();
    let authority = Authority::live();
    sandbox
        .install_scoped_execution(
            Path::new("/usr/bin/bwrap"),
            Path::new(sandbox_binary()),
            authority.clone(),
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let process = sandbox
        .spawn(tree(listener.local_addr().unwrap().port()))
        .await
        .unwrap();
    let nodes = observe_tree(&listener).await;
    let mut command = pc::Command::new([
        "/bin/bash",
        "-c",
        "printf '%s\\n' \"$PWD\"; cat ../../mnt/session/file; read -r keep",
    ]);
    command.cwd = "/workspace/nested 工作目录".into();
    let (agent, mut channel) = sandbox.spawn_agent(command).await.unwrap();
    let mut line = Vec::new();
    loop {
        let mut byte = [0];
        channel.read_exact(&mut byte).await.unwrap();
        line.push(byte[0]);
        if byte == [b'\n'] {
            break;
        }
    }
    assert_eq!(
        String::from_utf8(line).unwrap().trim_end(),
        sandbox
            .workspace_path()
            .join("workspace/nested 工作目录")
            .to_str()
            .unwrap()
    );
    let mut file = [0; 11];
    channel.read_exact(&mut file).await.unwrap();
    assert_eq!(&file, b"exact\0file\xff");
    assert_eq!(
        marker(base.path())["executions"].as_array().unwrap().len(),
        2
    );
    let handle = sandbox.handle();
    assert!(
        provider
            .adopt_sandbox_for_effect(&spec(), &handle, &fence("adopt"))
            .await
            .is_err()
    );
    assert!(
        provider
            .create_sandbox_for_effect(&spec(), &fence("create"), None)
            .await
            .is_err()
    );
    sandbox.refresh_scoped_execution().await.unwrap();
    agent.signal(pc::Signal::Kill).await.unwrap();
    agent.wait().await.unwrap();
    assert!(
        provider
            .adopt_sandbox_for_effect(&spec(), &handle, &fence("adopt"))
            .await
            .is_err()
    );
    assert_tree_live(&nodes);
    sandbox.stop_scoped_execution().await.unwrap();
    assert_tree_closed(nodes).await;
    process.wait().await.unwrap();
    assert!(
        sandbox
            .spawn(pc::Command::new(["/bin/true"]))
            .await
            .is_err()
    );
    let adopted = provider
        .adopt_sandbox_for_effect(&spec(), &handle, &fence("adopt"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(adopted.workspace_path().join("mnt/session/file")).unwrap(),
        file
    );
    dispose(&adopted).await;
    assert_eq!(marker(base.path())["phase"], "removed");
}

#[tokio::test]
async fn provider_scope_denies_late_upgrade_and_rechecks_authority_at_go() {
    // P6 unscoped first spawn freezes the weaker mode; a later installation
    // cannot pretend the earlier process was contained. P7 expiry/revocation
    // between initial deadline and GO => no workload, no fallback. P8 duplicate
    // install rejects; neither route manufactures capability.
    for revoke in [false, true] {
        let base = tempfile::tempdir().unwrap();
        let provider = LocalProvider::trusted_host(base.path());
        let sandbox = provider
            .create_sandbox_for_effect(&spec(), &fence("create"), None)
            .await
            .unwrap();
        let authority = Authority::live();
        if revoke {
            authority.0.lock().unwrap().2 = true;
            sandbox
                .install_scoped_execution(
                    Path::new("/usr/bin/bwrap"),
                    Path::new(sandbox_binary()),
                    authority.clone(),
                )
                .unwrap();
            let mut command = pc::Command::new(["/bin/sh", "-c", "printf bad > must-not-run"]);
            command.cwd = "/workspace".into();
            assert!(sandbox.spawn(command).await.is_err());
            assert!(
                !sandbox
                    .workspace_path()
                    .join("workspace/must-not-run")
                    .exists()
            );
            assert!(marker(base.path()).get("executions").is_none());
        } else {
            sandbox
                .spawn(pc::Command::new(["/bin/true"]))
                .await
                .unwrap()
                .wait()
                .await
                .unwrap();
        }
        assert!(
            sandbox
                .install_scoped_execution(
                    Path::new("/usr/bin/bwrap"),
                    Path::new(sandbox_binary()),
                    authority
                )
                .is_err()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_refresh_reobserves_exact_tree_exit_after_delivery_failure() {
    // Cause/effect graph: C1 preflight poll sees a live exact tree; C2 that
    // tree remains live or exits while the authority callback is paused;
    // C3 callback commits, denies, or reports a broken channel. E1 a live
    // failed delivery remains an error; E2 exact tree exit makes renewal
    // unnecessary; E3 normal delivery succeeds without stopping the tree.
    // Decision table (C1 throughout, actual 3-node Linux trees):
    // | rule | C2 at delivery | C3 | result |
    // | R1 | live | commit | E3 |
    // | R2-R3 | live | deny/broken | E1 (no failure-kind shortcut) |
    // | R4 | exited | commit (real broken socket) | E2 |
    // | R5-R6 | exited | deny/broken | E2 (fresh exact receipt) |
    // R7: repeating after exit skips the callback entirely, preserving marker
    // and File bytes. A barrier, not sleep, forces the preflight/delivery race.
    // BrokenPipe injection with a LIVE tree is the unknown-evidence negative
    // control; it is not a claim that production Session authority is wired.
    for exited in [false, true] {
        for failure in [
            None,
            Some(std::io::ErrorKind::PermissionDenied),
            Some(std::io::ErrorKind::BrokenPipe),
        ] {
            let base = tempfile::tempdir().unwrap();
            let provider = LocalProvider::trusted_host(base.path());
            let sandbox = Arc::new(
                provider
                    .create_sandbox_for_effect(&spec(), &fence("create"), None)
                    .await
                    .unwrap(),
            );
            let authority = Authority::live();
            sandbox
                .install_scoped_execution(
                    Path::new("/usr/bin/bwrap"),
                    Path::new(sandbox_binary()),
                    authority.clone(),
                )
                .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let process = sandbox
                .spawn(tree(listener.local_addr().unwrap().port()))
                .await
                .unwrap();
            let nodes = observe_tree(&listener).await;
            let original_marker = marker(base.path());
            let (reached, observed) = tokio::sync::oneshot::channel();
            let (resume, waiting) = std::sync::mpsc::channel();
            *authority.1.lock().unwrap() = Some(AuthorityPause {
                call: authority.0.lock().unwrap().1 + 1,
                reached,
                resume: waiting,
                failure,
            });
            let refreshing = sandbox.clone();
            let refresh = tokio::spawn(async move { refreshing.refresh_scoped_execution().await });
            tokio::time::timeout(Duration::from_secs(3), observed)
                .await
                .unwrap()
                .unwrap();
            if exited {
                process.signal(pc::Signal::Kill).await.unwrap();
                tokio::time::timeout(Duration::from_secs(3), process.wait())
                    .await
                    .unwrap()
                    .unwrap();
            } else {
                assert_tree_live(&nodes);
            }
            resume.send(()).unwrap();
            let result = refresh.await.unwrap();
            assert_eq!(
                result.is_ok(),
                exited || failure.is_none(),
                "{exited}/{failure:?}"
            );
            if !exited {
                assert_tree_live(&nodes);
            }
            assert_eq!(
                marker(base.path()),
                original_marker,
                "renewal never retires WAL"
            );
            assert_eq!(
                std::fs::read(sandbox.workspace_path().join("mnt/session/file")).unwrap(),
                b"exact\0file\xff"
            );
            sandbox.stop_scoped_execution().await.unwrap();
            assert_tree_closed(nodes).await;
            let calls = authority.0.lock().unwrap().1;
            sandbox.refresh_scoped_execution().await.unwrap();
            assert_eq!(
                authority.0.lock().unwrap().1,
                calls,
                "R7 skips exited handles"
            );
            dispose(&sandbox).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_authority_wait_cannot_extend_launch_or_refresh_budget() {
    // D13 launch / D14 refresh: fixed 50ms proof is sampled, then its authority
    // callback stalls >50ms BEFORE converting the relative duration. Both deny
    // without creating a new deadline. Refresh leaves the existing live tree
    // and durable participants unchanged, not a false stop/cleanup receipt.
    for refresh in [false, true] {
        let base = tempfile::tempdir().unwrap();
        let provider = LocalProvider::trusted_host(base.path());
        let sandbox = Arc::new(
            provider
                .create_sandbox_for_effect(&spec(), &fence("create"), None)
                .await
                .unwrap(),
        );
        let authority = Authority::live();
        sandbox
            .install_scoped_execution(
                Path::new("/usr/bin/bwrap"),
                Path::new(sandbox_binary()),
                authority.clone(),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = if refresh {
            let process = sandbox
                .spawn(tree(listener.local_addr().unwrap().port()))
                .await
                .unwrap();
            Some((process, observe_tree(&listener).await))
        } else {
            None
        };
        let before = marker(base.path());
        let (reached, observed) = tokio::sync::oneshot::channel();
        let (resume, waiting) = std::sync::mpsc::channel();
        let call = {
            let mut current = authority.0.lock().unwrap();
            current.0 = Some(Duration::from_millis(50));
            current.1 + 1
        };
        *authority.1.lock().unwrap() = Some(AuthorityPause {
            call,
            reached,
            resume: waiting,
            failure: None,
        });
        let work = sandbox.clone();
        let operation = tokio::spawn(async move {
            if refresh {
                work.refresh_scoped_execution().await
            } else {
                work.spawn(pc::Command::new(["/bin/true"]))
                    .await
                    .map(|_| ())
            }
        });
        tokio::time::timeout(Duration::from_secs(3), observed)
            .await
            .unwrap()
            .unwrap();
        // This is deliberate elapsed-time fault injection AFTER the barrier,
        // not a sleep used to guess whether the callback has been reached.
        tokio::time::sleep(Duration::from_millis(80)).await;
        resume.send(()).unwrap();
        assert!(
            operation.await.unwrap().is_err(),
            "D13/D14 expired callback budget"
        );
        assert_eq!(
            marker(base.path()),
            before,
            "no launch/retirement evidence invented"
        );
        assert_eq!(
            std::fs::read(sandbox.workspace_path().join("mnt/session/file")).unwrap(),
            b"exact\0file\xff"
        );
        if let Some((process, nodes)) = live {
            assert_tree_live(&nodes);
            sandbox.stop_scoped_execution().await.unwrap();
            assert_tree_closed(nodes).await;
            process.wait().await.unwrap();
        }
        dispose(&sandbox).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_close_racing_join_cannot_report_quiescence_or_allow_late_go() {
    // P11 pause after JOINED but before the current-authority callback commits
    // GO. Concurrent close must close admission, fail its marker try-lock and
    // retain bytes (no stop receipt). Resume => no GO; retry => quiescent. This
    // exercises the actual provider guard/OnceLock/launch path, not a toy lock.
    let base = tempfile::tempdir().unwrap();
    let provider = LocalProvider::trusted_host(base.path());
    let sandbox = Arc::new(
        provider
            .create_sandbox_for_effect(&spec(), &fence("create"), None)
            .await
            .unwrap(),
    );
    let (joined, observed) = tokio::sync::oneshot::channel();
    let (resume, waiting) = std::sync::mpsc::channel();
    let authority = Authority::live();
    *authority.1.lock().unwrap() = Some(AuthorityPause {
        call: 2,
        reached: joined,
        resume: waiting,
        failure: None,
    });
    sandbox
        .install_scoped_execution(
            Path::new("/usr/bin/bwrap"),
            Path::new(sandbox_binary()),
            authority,
        )
        .unwrap();
    let spawned = sandbox.clone();
    let launch = tokio::spawn(async move {
        spawned
            .spawn(pc::Command::new([
                "/bin/sh",
                "-c",
                "printf bad > must-not-run",
            ]))
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), observed)
        .await
        .unwrap()
        .unwrap();
    assert!(sandbox.stop_scoped_execution().await.is_err());
    assert!(sandbox.workspace_path().join("mnt/session/file").exists());
    resume.send(()).unwrap();
    assert!(launch.await.unwrap().is_err());
    assert!(!sandbox.workspace_path().join("must-not-run").exists());
    sandbox.stop_scoped_execution().await.unwrap();
}

#[tokio::test]
async fn provider_missing_helper_or_substituted_marker_cannot_fallback_to_host_exec() {
    // P12 missing deployment helper => no workload and no recorded participant.
    // P13 marker disappears after bootstrap but before WAL => no GO, no host
    // fallback and unchanged resource bytes. Restoring exact marker permits a
    // retry; errors are not converted into an empty participant receipt.
    struct MissingMarker {
        calls: std::sync::atomic::AtomicUsize,
        path: std::path::PathBuf,
    }
    impl CurrentExecutionAuthority for MissingMarker {
        fn with_current(
            &self,
            effect: &mut dyn FnMut(Duration) -> std::io::Result<()>,
        ) -> std::io::Result<()> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                std::fs::rename(&self.path, self.path.with_extension("saved"))?;
            }
            effect(Duration::from_secs(20))
        }
    }
    for missing_marker in [false, true] {
        let base = tempfile::tempdir().unwrap();
        let provider = LocalProvider::trusted_host(base.path());
        let sandbox = provider
            .create_sandbox_for_effect(&spec(), &fence("create"), None)
            .await
            .unwrap();
        let path = base.path().join(".scoped-provider.awaken-realization.json");
        let original = std::fs::read(&path).unwrap();
        let authority: Arc<dyn CurrentExecutionAuthority> = if missing_marker {
            Arc::new(MissingMarker {
                calls: Default::default(),
                path: path.clone(),
            })
        } else {
            Authority::live()
        };
        let absent_helper = base.path().join("missing-helper");
        sandbox
            .install_scoped_execution(
                Path::new("/usr/bin/bwrap"),
                if missing_marker {
                    Path::new(sandbox_binary())
                } else {
                    &absent_helper
                },
                authority,
            )
            .unwrap();
        assert!(
            sandbox
                .spawn(pc::Command::new([
                    "/bin/sh",
                    "-c",
                    "printf bad > must-not-run"
                ]))
                .await
                .is_err()
        );
        assert!(!sandbox.workspace_path().join("must-not-run").exists());
        assert_eq!(
            std::fs::read(sandbox.workspace_path().join("mnt/session/file")).unwrap(),
            b"exact\0file\xff"
        );
        if missing_marker {
            std::fs::rename(path.with_extension("saved"), &path).unwrap();
            sandbox
                .spawn(pc::Command::new(["/bin/true"]))
                .await
                .unwrap()
                .wait()
                .await
                .unwrap();
        } else {
            assert_eq!(std::fs::read(path).unwrap(), original);
        }
    }
}

#[tokio::test]
async fn provider_owner_crash_preserves_marker_and_recovery_never_signals_saved_pids() {
    // P9 abrupt owner SIGKILL does not run Drop. Three independently observed
    // detached members must close; the fresh provider then reads the ORIGINAL
    // durable marker, adopts, and prepares/deletes exactly that root. P10 a
    // foreign boot in otherwise exact evidence => unknown, retained bytes and
    // no successful adoption; restoring the original bytes permits retry.
    let base = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut owner = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scoped_provider::provider_owner_participant",
            "--ignored",
            "--nocapture",
        ])
        .env("SCOPED_PROVIDER_BASE", base.path())
        .env(
            "SCOPED_TREE_PORT",
            listener.local_addr().unwrap().port().to_string(),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let nodes = observe_tree(&listener).await;
    let original = marker(base.path());
    assert_eq!(original["executions"].as_array().unwrap().len(), 1);
    let identity: awaken_local_process::ProcessExecutionIdentity =
        serde_json::from_value(original["executions"][0].clone()).unwrap();
    assert!(!identity.is_quiescent().unwrap());
    owner.kill().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), assert_tree_closed(nodes))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !identity.is_quiescent().unwrap() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let provider = LocalProvider::trusted_host(base.path());
    let recreated = provider
        .create_sandbox_for_effect(&spec(), &fence("create"), None)
        .await
        .unwrap();
    let handle = recreated.handle();
    let mut foreign = original.clone();
    foreign["executions"][0]["boot_id"][0] =
        serde_json::json!((foreign["executions"][0]["boot_id"][0].as_u64().unwrap() + 1) % 256);
    let path = base.path().join(".scoped-provider.awaken-realization.json");
    std::fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert!(
        provider
            .adopt_sandbox_for_effect(&spec(), &handle, &fence("adopt"))
            .await
            .is_err()
    );
    assert!(
        recreated
            .prepare_disposal_for_effect(
                &fence("dispose"),
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence(
                    "dispose"
                ))
            )
            .await
            .is_err()
    );
    assert!(recreated.workspace_path().join("mnt/session/file").exists());
    // prepare may durably enter Removing before the unknown OS evidence blocks
    // effects. Restore only the original test-owned marker to exercise adoption.
    std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    drop(recreated); // Release a retained terminal guard before cold retry.
    let adopted = provider
        .adopt_sandbox_for_effect(&spec(), &handle, &fence("adopt"))
        .await
        .unwrap();
    dispose(&adopted).await;
    assert_eq!(marker(base.path())["phase"], "removed");
}

#[tokio::test]
async fn provider_terminal_retry_retains_resources_until_the_last_tree_is_absent() {
    // P14 terminal preparation may close the durable marker to Removing, but
    // a live tree prevents any successful preparation/Memory retirement/delete.
    // Stop must consume the SAME retained RemovalGuard, not try-lock itself or
    // require Ready forever. Exact exit => retry reaches Removed; repeated stop
    // after removal stays idempotent and closed admission never executes again.
    let base = tempfile::tempdir().unwrap();
    let provider = LocalProvider::trusted_host(base.path());
    let sandbox = provider
        .create_sandbox_for_effect(&spec(), &fence("create"), None)
        .await
        .unwrap();
    sandbox
        .install_scoped_execution(
            Path::new("/usr/bin/bwrap"),
            Path::new(sandbox_binary()),
            Authority::live(),
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let process = sandbox
        .spawn(tree(listener.local_addr().unwrap().port()))
        .await
        .unwrap();
    let nodes = observe_tree(&listener).await;
    assert!(
        sandbox
            .prepare_disposal_for_effect(
                &fence("dispose"),
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence(
                    "dispose"
                ))
            )
            .await
            .is_err()
    );
    assert_eq!(marker(base.path())["phase"], "removing");
    assert_eq!(
        std::fs::read(sandbox.workspace_path().join("mnt/session/file")).unwrap(),
        b"exact\0file\xff"
    );
    assert_tree_live(&nodes);
    sandbox.stop_scoped_execution().await.unwrap();
    assert_tree_closed(nodes).await;
    process.wait().await.unwrap();
    dispose(&sandbox).await;
    sandbox.stop_scoped_execution().await.unwrap();
    assert_eq!(marker(base.path())["phase"], "removed");
    assert!(
        sandbox
            .spawn(pc::Command::new(["/bin/true"]))
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "subprocess participant invoked by provider owner-crash test"]
async fn provider_owner_participant() {
    let base = std::env::var_os("SCOPED_PROVIDER_BASE").unwrap();
    let provider = LocalProvider::trusted_host(base);
    let sandbox = provider
        .create_sandbox_for_effect(&spec(), &fence("create"), None)
        .await
        .unwrap();
    sandbox
        .install_scoped_execution(
            Path::new("/usr/bin/bwrap"),
            Path::new(sandbox_binary()),
            Authority::live(),
        )
        .unwrap();
    let _process = sandbox
        .spawn(tree(
            std::env::var("SCOPED_TREE_PORT").unwrap().parse().unwrap(),
        ))
        .await
        .unwrap();
    let mut byte = [0];
    let _ = tokio::io::stdin().read(&mut byte).await;
}
