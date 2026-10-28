// @formal evidence runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
// @formal evidence session_resources.explicit_empty_manifest_revokes_runtime_projection
//! Session environment projection, live resource replacement, and eager/lazy realization.

use super::*;

fn carried_mount_bytes(mount: &awaken_provisioning_contract::MountRequirement) -> Vec<u8> {
    let awaken_provisioning_contract::MountSource::InlineBytes { contents, .. } = &mount.source
    else {
        panic!("expected a carried resource source")
    };
    contents.clone()
}

/// Live mount decision table:
/// | resident tier | requested File access | effect |
/// |---|---|---|
/// | Namespace | read-only | materialize, replace projection, evict context |
/// | Workdir | read-only | reject without changing projection (next test) |
///
/// This rule exercises the admitted branch and proves detach reverses it in the
/// same Session-owned environment.
#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires Linux mount-namespace path fidelity"
)]
async fn applying_changed_inputs_rebuilds_the_resource_projection_and_cached_sandbox() {
    // Causes: C1 an active Session's Resource inputs change to a new generation;
    // C2 a cached sandbox/projection exists; C3 the process installed Session retains a Run
    // claim from the preceding settled attempt; C4 the Resource reservation is
    // authorized by the Session root. Effects: E1 active generation and installed
    // sandbox projection advance as one pair; E2 the reservation persists through
    // the root authority without reading or rewriting the attempt's raw sandbox
    // cache; E3 Create/Adopt claim fencing remains outside this Resource rule.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | R1 | yes | yes | absent | yes | E1 |
    // | R2 | yes | yes | stale | yes | E1+E2 |
    // | R3 | create/adopt | any | stale | n/a | E3; fail closed |
    //
    // This test owns R2. Run-ingress sandbox-binding conformance owns R3.
    let mut raw_host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
    raw_host.env.session_provider =
        awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
            std::env::temp_dir().join(format!(
                "awaken-hot-attach-namespace-{}",
                std::process::id()
            )),
            false,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .into_session_provider();
    let host = (crate::host::with_test_resource_validation(raw_host))
        .build()
        .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // C0 freezes the ordinary empty generation before the first environment is
    // realized, so C1 below advances one exact durable Resource transition.
    install_complete_test_session_for_realization(
        &managed,
        "t-attach",
        bare_session("agent", host.run_assembly.local_workspace()),
    )
    .await
    .expect("install complete empty Session projection");

    // A blob to mount, and a first Run that builds + caches the Thread's sandbox.
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            host.run_assembly.local_workspace(),
            "data.txt".into(),
            "text/plain".into(),
            b"hello-attached",
        )
        .await
        .expect("create File")
        .id;
    run_prepared_session_messages(&managed, "agent", "t-attach", user("hi"))
        .await
        .expect("first Run");
    let environment_before = host
        .realization_executor
        .environments()
        .session_environment("t-attach")
        .await
        .expect("installed Sessions")
        .expect("session environment");
    let handle_before = environment_before.lifecycle().handle();
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run("t-attach", |run| run.context.is_some())
            .expect("installed Sessions")
            .unwrap_or(false),
        "the first Run caches the Thread's sandbox ctx"
    );
    let before = host
        .realization_executor
        .sandbox_spec("t-attach")
        .mounts
        .len();

    // Model the exact post-settlement residue observed by an idle durable
    // Session. No Dispatch store is installed, so any attempt to reinterpret
    // this cache as current Resource authority fails the test before effects.
    let stale_claim = awaken_dispatch_contract::RunClaim {
        run_id: RunId("settled-before-live-attach".into()),
        owner: "retired-worker".into(),
        epoch: 1,
    };
    host.run_assembly
        .installed_sessions
        .install_run("t-attach", |run| {
            run.dispatch_claim = Some(stale_claim.clone());
        });

    // Attach a file resource on the LIVE session.
    let res = TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/data.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    };
    let attached = effective_resources(vec![res.clone()]);
    managed
        .realization_executor
        .apply_session_inputs(
            "t-attach",
            &resource_transition(
                host.run_assembly.local_workspace(),
                0,
                Default::default(),
                1,
                attached.clone(),
            ),
        )
        .await
        .expect("attach");

    // The cached sandbox was evicted (so the next Run rebuilds) ...
    assert!(
        !host
            .run_assembly
            .installed_sessions
            .read_run("t-attach", |run| run.context.is_some())
            .expect("installed Sessions")
            .unwrap_or(false),
        "attach evicts the cached ctx so the next Run rebuilds with the mount"
    );
    let handle_after_attach = host
        .run_assembly
        .session_environment_handle("t-attach")
        .await
        .expect("live attach retains one Session environment");
    assert_eq!(handle_after_attach.sandbox_id, handle_before.sandbox_id);
    assert_eq!(
        handle_after_attach.provider_kind(),
        handle_before.provider_kind()
    );
    assert!(
        handle_after_attach
            .owned_paths()
            .is_some_and(|paths| paths.iter().any(|path| path.ends_with("data.txt"))),
        "live attach is reserved in the durable V2 handle before projection"
    );
    assert_eq!(
        environment_before
            .harvest()
            .list_frozen_mount_files("/mnt/session/uploads")
            .await
            .unwrap(),
        vec![("data.txt".into(), b"hello-attached".to_vec())],
        "the live environment receives the file before attach returns"
    );
    // ... and the spec the next Run will build now carries the mount + its bytes.
    let spec = host.realization_executor.sandbox_spec("t-attach");
    assert_eq!(spec.mounts.len(), before + 1, "one more mount staged");
    let dump = serde_json::to_string(&spec.mounts).expect("mounts serialize");
    assert!(
        dump.contains("data.txt"),
        "mount realized at the resource path: {dump}"
    );
    assert_eq!(
        carried_mount_bytes(spec.mounts.last().unwrap()),
        b"hello-attached"
    );
    assert_eq!(
        host.run_assembly
            .installed_sessions
            .read_run("t-attach", |run| run.dispatch_claim.clone())
            .expect("installed Sessions")
            .flatten(),
        Some(stale_claim),
        "R2 Resource reservation leaves the attempt delivery cache untouched"
    );
    host.run_assembly
        .installed_sessions
        .install_run("t-attach", |run| run.dispatch_claim = None);

    run_prepared_session_messages(&managed, "agent", "t-attach", user("after attach"))
        .await
        .expect("runtime rebuild over retained environment");
    let environment_after = host
        .realization_executor
        .environments()
        .session_environment("t-attach")
        .await
        .expect("installed Sessions")
        .expect("retained environment");
    assert!(Arc::ptr_eq(&environment_before, &environment_after));

    // Detach removes exactly that mount again.
    managed
        .realization_executor
        .apply_session_inputs(
            "t-attach",
            &resource_transition(
                host.run_assembly.local_workspace(),
                1,
                attached,
                2,
                Default::default(),
            ),
        )
        .await
        .expect("detach");
    let spec = host.realization_executor.sandbox_spec("t-attach");
    assert_eq!(spec.mounts.len(), before, "the mount is dropped on detach");
    assert!(
        !serde_json::to_string(&spec.mounts)
            .unwrap()
            .contains("data.txt")
    );
    assert!(
        environment_after
            .sandbox_environment()
            .list_workspace_files(".mnt")
            .await
            .unwrap()
            .is_empty(),
        "detach removes the projected file from the same live environment"
    );
    assert_eq!(
        host.run_assembly
            .session_environment_handle("t-attach")
            .await,
        Some(handle_after_attach),
        "detachment retains conservative path evidence for crash recovery"
    );
}

/// Live invalid-layout decision rule: L1 an installed File generation and
/// resident Namespace environment exist; L2 the next durable generation carries
/// a historical `/repo` Repository. L1+L2 => reject before verifier, Skill/File
/// compilation, projection transaction, or Git; keep the old manifest, mounts,
/// resident bytes, environment handle, and cached Runtime atomically unchanged.
#[tokio::test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "requires Linux mount-namespace path fidelity"
)]
async fn invalid_repository_live_replacement_preserves_the_installed_generation() {
    let storage = tempfile::tempdir().unwrap();
    let mut raw_host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
    raw_host.env.session_provider =
        awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
            storage.path().join("sandboxes"),
            false,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .into_session_provider();
    let verifier = Arc::new(SequencedRepositoryTransport::default());
    let host = ((raw_host).with_session_execution(
        crate::host::test_resource_execution().with_repository_binding_verifier(verifier.clone()),
    ))
    .build()
    .expect("validated Host dependencies");

    let managed = managed_with_resource_source(host.clone());
    let file_id = host
        .file_application()
        .expect("File application")
        .create_uploaded_file(
            host.run_assembly.local_workspace(),
            "retained.txt".into(),
            "text/plain".into(),
            b"retained-generation",
        )
        .await
        .unwrap()
        .id;
    let installed = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/retained.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    let mut init = bare_session("agent", host.run_assembly.local_workspace());
    init.resources = installed;
    install_complete_test_session_for_realization(&managed, "invalid-live-repository", init)
        .await
        .unwrap();
    run_prepared_session_messages(
        &managed,
        "agent",
        "invalid-live-repository",
        vec![Message::text(
            MessageId("initial-generation".into()),
            Role::User,
            "open the environment",
        )],
    )
    .await
    .unwrap();
    let environment = host
        .realization_executor
        .environments()
        .session_environment("invalid-live-repository")
        .await
        .expect("installed Sessions")
        .expect("resident environment");
    let before_handle = environment.lifecycle().handle();
    let before_manifest = host
        .realization_executor
        .staging()
        .thread_resource_manifest("invalid-live-repository")
        .expect("installed Sessions")
        .expect("installed manifest");
    let before_spec = host
        .realization_executor
        .sandbox_spec("invalid-live-repository");
    let before_files = environment
        .harvest()
        .list_frozen_mount_files("/mnt/session/uploads")
        .await
        .unwrap();

    let canonical = effective_resources(vec![TestInput {
        kind: "github_repository".into(),
        id: "https://github.com/awaken/invalid-live.git".into(),
        mount_path: "/workspace/repo".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    // L2 is historical durable input: new commands cannot construct `/repo`,
    // while the canonical replay decoder retains the exact legacy bytes so the
    // Runtime guard can reject them without silently relocating the path.
    let mut durable = serde_json::to_value(canonical).unwrap();
    durable["inputs"][0]["mount_path"] = serde_json::Value::String("/repo".into());
    let invalid = serde_json::from_value(durable).expect("decode historical Resource manifest");
    let error = managed
        .realization_executor
        .apply_session_inputs(
            "invalid-live-repository",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                invalid,
            ),
        )
        .await
        .expect_err("L2 fails before projection mutation");
    assert!(error.message.contains("/repo"), "L1/L2: {error:?}");
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 0, "L1/L2 verifier");
    assert_eq!(
        host.realization_executor
            .staging()
            .thread_resource_manifest("invalid-live-repository")
            .expect("installed Sessions"),
        Some(before_manifest),
        "L1/L2 manifest"
    );
    assert_eq!(
        host.realization_executor
            .sandbox_spec("invalid-live-repository"),
        before_spec,
        "L1/L2 mounts"
    );
    assert_eq!(
        host.run_assembly
            .session_environment_handle("invalid-live-repository")
            .await,
        Some(before_handle),
        "L1/L2 environment"
    );
    assert_eq!(
        environment
            .harvest()
            .list_frozen_mount_files("/mnt/session/uploads")
            .await
            .unwrap(),
        before_files,
        "L1/L2 physical files"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run("invalid-live-repository", |run| run.context.is_some())
            .expect("installed Sessions")
            .unwrap_or(false),
        "L1/L2 cached Runtime"
    );
    assert!(
        host.realization_executor
            .staging()
            .thread_repository_activations("invalid-live-repository")
            .is_empty(),
        "L1/L2 Git activation"
    );
}

/// Repository detach decision table on the Linux resident Namespace environment:
/// C0 the Linux Namespace substrate is available; C1 a create-time Repository
/// is physically realized; C2 the next durable generation omits it; C3 the
/// Session environment remains resident. E1 removes
/// the exact working tree before the mutation returns and E2 preserves the same
/// environment handle. A stale readable checkout is forbidden.
///
/// | Rule | C1 | C2 | C3 | Effect |
/// | RD1  | T  | T  | T  | E1 removed + E2 retained environment |
#[cfg(target_os = "linux")]
#[tokio::test]
async fn applying_repository_detach_removes_the_resident_namespace_checkout() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&source)
            .status()
            .expect("run git fixture command");
        assert!(status.success(), "git fixture command failed: {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "runtime-host-test@awaken.invalid"]);
    git(&["config", "user.name", "Awaken Runtime Host Test"]);
    std::fs::write(source.join("README.md"), "resident repository").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-q", "-m", "seed"]);

    let mut raw_host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
    raw_host.env.session_provider =
        awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
            fixture.path().join("sandboxes"),
            false,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .into_session_provider();
    let repository_path_fidelity = raw_host
        .env
        .session_provider
        .realizer()
        .capabilities()
        .path_fidelity;
    let host = (crate::host::with_test_resource_validation(raw_host))
        .build()
        .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let desired = effective_resources(vec![TestInput {
        kind: "github_repository".into(),
        id: source.to_string_lossy().into_owned(),
        mount_path: "/workspace/live-repo".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    install_complete_test_session_for_realization(
        &managed,
        "t-repo-detach",
        awaken_session_configuration_contract::SessionInit {
            workspace_id: host.run_assembly.local_workspace().into(),
            agent_id: "agent".into(),
            delegate_ids: Vec::new(),
            tools: None,
            resource_revision: 0,
            resources: desired,
            model: None,
            runtime: None,
            environment: session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
        },
    )
    .await
    .unwrap();
    let realization = run_prepared_session_messages(
        &managed,
        "agent",
        "t-repo-detach",
        vec![Message::text(
            MessageId("repo-before".into()),
            Role::User,
            "observe repository",
        )],
    )
    .await;
    if !repository_path_fidelity {
        let error = match realization {
            Err(error) => error,
            Ok(_) => panic!(
                "a Namespace provider without path fidelity must reject Repository realization"
            ),
        };
        assert!(
            error
                .message
                .contains("one sandbox-absolute workspace path"),
            "RD0 capability rejection must explain the shared-path invariant: {}",
            error.message
        );
        assert!(
            host.realization_executor
                .environments()
                .session_environment("t-repo-detach")
                .await
                .expect("installed Sessions")
                .is_none(),
            "RD0 rejection happens before a resident environment or checkout is exposed"
        );
        return;
    }
    realization.expect("RD1 path-fidelitous Namespace realizes the Repository");
    let environment = host
        .realization_executor
        .environments()
        .session_environment("t-repo-detach")
        .await
        .expect("installed Sessions")
        .expect("resident environment");
    let before_manifest = host
        .realization_executor
        .staging()
        .thread_resource_manifest("t-repo-detach")
        .expect("installed Sessions")
        .expect("installed manifest");
    let handle = environment.lifecycle().handle();
    let checkout = fixture
        .path()
        .join("sandboxes/t-repo-detach/workspace/live-repo");
    assert_eq!(
        std::fs::read(checkout.join("README.md")).unwrap(),
        b"resident repository",
        "RD1 create-time checkout is physically present"
    );

    managed
        .realization_executor
        .apply_session_inputs(
            "t-repo-detach",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                crate::session_resource::ResolvedSessionResources::default(),
            ),
        )
        .await
        .expect("detach repository");
    assert!(
        !checkout.exists(),
        "RD1 detached checkout must not remain readable"
    );
    assert_eq!(
        host.run_assembly
            .session_environment_handle("t-repo-detach")
            .await,
        Some(handle),
        "RD1 mutation retains the one Session environment"
    );
}

/// Causes: a live Workdir environment exists and the replacement manifest adds
/// a read-only File. Constraint: Workdir provides lexical containment but cannot
/// enforce mount immutability. Effect/rule W1: reject before changing the staged
/// manifest, resident files, or cached context.
#[tokio::test]
async fn applying_readonly_file_to_live_workdir_fails_closed_without_partial_projection() {
    // Decision rule: exercise the read-only collision branch and require both the
    // explicit failure and unchanged live projection effects documented below.

    let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_deployment_config::SandboxTier::Local;
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral_with_deployment(
            Arc::new(OkModel),
            "stub",
            deployment,
        )
        .await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    host.session_runs
        .run(
            None,
            "t-local-attach",
            vec![Message::text(MessageId("initial".into()), Role::User, "hi")],
        )
        .await
        .expect("first Run");
    let environment = host
        .realization_executor
        .environments()
        .session_environment("t-local-attach")
        .await
        .expect("installed Sessions")
        .expect("live Workdir environment");
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            host.run_assembly.local_workspace(),
            "data.txt".into(),
            "text/plain".into(),
            b"must-remain-unmounted",
        )
        .await
        .expect("create File")
        .id;
    let before_manifest = host
        .realization_executor
        .staging()
        .thread_resource_manifest("t-local-attach")
        .expect("installed Sessions")
        .unwrap_or_else(|| {
            crate::session_resource::SessionResourceManifest::at_revision(
                host.run_assembly.local_workspace(),
                0,
                crate::session_resource::ResolvedSessionResources::default(),
            )
        });
    let before = host.realization_executor.sandbox_spec("t-local-attach");
    let attached = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/data.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let error = managed
        .realization_executor
        .apply_session_inputs(
            "t-local-attach",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                attached,
            ),
        )
        .await
        .expect_err("Workdir cannot admit an official read-only File copy");
    assert!(error.message.contains("does not enforce read-only"));
    assert_eq!(
        host.realization_executor.sandbox_spec("t-local-attach"),
        before
    );
    assert!(
        environment
            .sandbox_environment()
            .list_workspace_files("mnt/session/uploads")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run("t-local-attach", |run| run.context.is_some())
            .expect("installed Sessions")
            .unwrap_or(false),
        "rejected replacement leaves the cached runtime intact"
    );
}

/// Committed-query cause/effect graph after execution provisioning fails:
/// C1 a read-only frozen File plus a CPU limit is staged; C2 Workdir cannot
/// enforce read-only mounts or limits;
/// C3 a side-effect-free reservation projection occurs; C4 no runtime/environment
/// becomes resident; C5 a committed-state GET follows. C1+C2+C3 cause E1 the
/// reservation to reject before any physical effect. C1+C2
/// cause E2 executable Run construction to fail. C4+C5 cause E3 the query to open only
/// committed truth, return the empty page, and leave the execution environment
/// absent instead of retrying the failing provisioning. The canonical prepare
/// kernel validates reservation mounts before limits. Direct Run construction
/// independently rejects the unsupported CPU limit; neither edge may provision
/// an Environment (both requested guarantees remain unchanged).
///
/// | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
/// |---|---|---|---|---|---|---|
/// | Q1 | T | T | T | T | F | E1 reservation denied/no env |
/// | Q2 | T | T | F | T | F | E2 direct Run denied/no env |
/// | Q3 | T | T | F | T | T | E3 query succeeds/no env |
#[tokio::test]
async fn committed_queries_do_not_provision_a_failed_session_environment() {
    // Managed physical row: real SQLite Root + controlled live lease + the
    // production binding sink precede first I/O. Installation itself has no
    // provider effect; the original denial/lazy/coordination assertions below
    // retain their causes and cannot borrow a fabricated Authorized result.
    let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = awaken_deployment_config::SandboxTier::Local;
    let roots = ManagedSessionRootFixture::new().await;
    let host = (crate::host::with_test_resource_validation(
        roots.configure(
            crate::SessionExecutorBuilder::ephemeral_with_deployment(
                Arc::new(OkModel),
                "stub",
                deployment,
            )
            .await,
        ),
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            host.run_assembly.local_workspace(),
            "query.txt".into(),
            "text/plain".into(),
            b"read-only",
        )
        .await
        .expect("create File")
        .id;
    roots
        .install_complete_test_session(
            &managed,
            "t-query-after-provisioning-denial",
            awaken_session_configuration_contract::SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: effective_resources(vec![TestInput {
                    kind: "file".into(),
                    id: file_id,
                    mount_path: "/query.txt".into(),
                    access: ResourceAccess::ReadOnly,
                    instructions: None,
                    initial_branch: None,
                    initial_commit: None,
                }]),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({"limits": {"cpu_millis": 1}}),
                ),
            },
        )
        .await
        .expect("stage frozen Session");

    let reservation_error = match host
        .run_assembly
        .ctx_for_session_coordination("t-query-after-provisioning-denial", Some("assistant"))
        .await
    {
        Ok(_) => panic!("Q1 reservation must reject unsupported read-only mounts"),
        Err(error) => error,
    };
    assert!(
        reservation_error
            .message
            .contains("backend does not enforce read-only"),
        "Q1: {reservation_error}"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment("t-query-after-provisioning-denial")
            .await
            .expect("installed Sessions")
            .is_none(),
        "Q1 reservation preflight has no physical effect"
    );

    let error = match run_prepared_session_messages(
        &managed,
        "assistant",
        "t-query-after-provisioning-denial",
        vec![Message::text(
            MessageId("denied".into()),
            Role::User,
            "must not reach inference",
        )],
    )
    .await
    {
        Ok(_) => panic!("Workdir must reject unsupported resource limits"),
        Err(error) => error,
    };
    assert!(
        error
            .message
            .contains("resource limits requested but backend cannot enforce them"),
        "Q2: {error}"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment("t-query-after-provisioning-denial")
            .await
            .expect("installed Sessions")
            .is_none(),
        "Q2 failed provisioning must not publish an environment"
    );

    let feed = host
        .thread_reads
        .commit_for_read("t-query-after-provisioning-denial")
        .await
        .expect("committed query must not retry Sandbox provisioning");
    let page = awaken_agent_contract::RunLifecycleFeed::events_after(
        feed.as_ref(),
        awaken_agent_contract::RunLifecycleCursor(0),
        100,
    )
    .await
    .expect("read empty committed lifecycle page");
    assert!(page.events.is_empty(), "Q3 inference never committed a Run");
    assert!(
        host.realization_executor
            .environments()
            .session_environment("t-query-after-provisioning-denial")
            .await
            .expect("installed Sessions")
            .is_none(),
        "Q3 committed query remains free of environment side effects"
    );
}

/// Cause-effect graph for the sole Environment projection:
///
/// C1 exact frozen network fact is Unrestricted / Allowlist / None (O constraint)
///  -> C2 install one Environment fingerprint
///  -> C3 ignore any retained `sandbox.network` field
///  -> E1 Native spec carries the exact frozen network
///  -> E2 Workdir convenience flag exists iff E1 is restricted.
///
/// | Rule | C1 | sandbox.network | E1 | E2 deny flag |
/// |---|---|---|---|---|
/// | N1 | Unrestricted | none | Unrestricted | F |
/// | N2 | Allowlist | conflicting None | exact Allowlist | T |
/// | N3 | None | conflicting Unrestricted | None | T |
#[tokio::test]
async fn frozen_environment_network_follows_the_decision_table() {
    use awaken_session_configuration_contract::SessionInit;
    let host = (crate::host::with_test_resource_validation(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await,
    ))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_with_resource_source(host.clone());
    let denies = |spec: awaken_provisioning_contract::SandboxSpec| spec.deny_tool_egress;
    let rules = [
        (
            "N1",
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
            awaken_provisioning_contract::NetworkPolicy::Unrestricted,
            false,
        ),
        (
            "N2",
            awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example".into()],
            },
            serde_json::json!({"network": {"mode": "none"}}),
            awaken_provisioning_contract::NetworkPolicy::Unrestricted,
            true,
        ),
        (
            "N3",
            awaken_session_configuration_contract::SessionNetworkPolicy::None,
            serde_json::json!({"network": {"mode": "unrestricted"}}),
            awaken_provisioning_contract::NetworkPolicy::Unrestricted,
            true,
        ),
    ];
    for (id, network, sandbox, expected_network, expected_denial) in rules {
        let thread = format!("environment-{id}");
        managed
            .run_assembly
            .install_test_session_init(
                &thread,
                SessionInit {
                    workspace_id: host.run_assembly.local_workspace().into(),
                    agent_id: "a".into(),
                    delegate_ids: Vec::new(),
                    tools: None,
                    resource_revision: 0,
                    resources: Default::default(),
                    model: None,
                    runtime: None,
                    environment: session_environment(network, sandbox),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{id}: {error}"));
        let spec = host.realization_executor.sandbox_spec(&thread);
        assert_eq!(spec.network, expected_network, "{id}");
        assert_eq!(denies(spec), expected_denial, "{id}");
    }
}

/// The frozen Environment reaches the sandbox spec through one projection. Isolation
/// and limits come from the network-free sandbox blob, while the distinct network fact
/// remains authoritative even if a retained blob contains a conflicting legacy field.
#[tokio::test]
async fn install_test_session_init_overlays_the_environment_sandbox_onto_the_spec() {
    // Managed physical row: real SQLite Root + controlled live lease + the
    // production binding sink precede first I/O. Installation itself has no
    // provider effect; the original denial/lazy/coordination assertions below
    // retain their causes and cannot borrow a fabricated Authorized result.
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_provisioning_contract::{IsolationClass, NetworkPolicy};
    use awaken_session_configuration_contract::SessionInit;
    let roots = ManagedSessionRootFixture::new().await;
    let host = roots
        .configure(crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    install_test_session_application(&host);
    let managed = host.clone();

    let init = SessionInit {
        workspace_id: "ws".into(),
        agent_id: "a".into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.github.com".into()],
            },
            serde_json::json!({
            "isolation": "namespace",
            "network": { "mode": "unrestricted" },
            "limits": { "cpu_millis": 2000, "memory_bytes": 4294967296u64 }
            }),
        ),
    };
    roots
        .install_complete_test_session(&managed, "t-sb", init)
        .await
        .unwrap();

    let spec = host.realization_executor.sandbox_spec("t-sb");
    assert_eq!(
        spec.isolation,
        IsolationClass::Namespace,
        "env isolation overlaid"
    );
    assert_eq!(
        spec.network,
        NetworkPolicy::Unrestricted,
        "Workdir does not claim strict allowlist enforcement"
    );
    assert!(
        spec.deny_tool_egress,
        "the Workdir tool wrapper retains the frozen restriction intent"
    );
    assert_eq!(spec.limits.cpu_millis, Some(2000));
    assert_eq!(spec.limits.memory_bytes, Some(4_294_967_296));

    // Workspace/resource cause-effect rules: W1 a nonempty Resource manifest
    // already carries its Workspace through staging; W2 an empty manifest must
    // still retain SessionInit.workspace_id. Both effects select the same
    // workspace-scoped immutable Agent publication; neither may fall back to
    // the process platform workspace.
    //
    // | Rule | resources | Session workspace | retained lookup scope |
    // | W1 | nonempty | ws | ws |
    // | W2 | empty | ws | ws |
    // A session with no override keeps the host default (Workdir, no limits).
    let bare = SessionInit {
        workspace_id: "ws".into(),
        agent_id: "a".into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    };
    roots
        .install_complete_test_session(&managed, "t-bare", bare)
        .await
        .unwrap();
    assert_eq!(
        host.capabilities
            .registered_thread_workspace("t-bare")
            .expect("healthy registry")
            .as_deref(),
        Some("ws"),
        "W2"
    );
    let context = host
        .run_assembly
        .ctx_for("t-bare", None)
        .await
        .expect("W2 runtime context")
        .context(&RunId("t-bare-attempt".into()))
        .expect("W2 readable foreground record");
    assert_eq!(
        context
            .execution_scope
            .as_ref()
            .map(|scope| scope.0.as_str()),
        Some("ws"),
        "W2 Session ownership enters the one attempt context"
    );
    assert_eq!(
        host.realization_executor.sandbox_spec("t-bare").isolation,
        IsolationClass::Workdir
    );
    assert!(
        !host
            .realization_executor
            .sandbox_spec("t-bare")
            .limits
            .is_set()
    );
}

/// Cause/effect design: C1 eager Session preparation freezes configuration; C2 no
/// Run has requested a context; C3 the first Run requests one. Effects: E1 C1+C2
/// leaves the physical environment absent; E2 C1+C3 materializes it and waits for
/// readiness before execution. Decision table: L1=C1+C2=>E1; L2=C1+C3=>E2.
#[tokio::test]
async fn install_test_session_init_is_lazy_and_first_run_materializes_the_environment() {
    // Managed physical row: real SQLite Root + controlled live lease + the
    // production binding sink precede first I/O. Installation itself has no
    // provider effect; the original denial/lazy/coordination assertions below
    // retain their causes and cannot borrow a fabricated Authorized result.
    // Constraint/Invariant: Session preparation records intent but only the first
    // executable Run may create the Environment. Decision rule: observe zero live
    // effects after prepare, then exactly one materialization on first Run.
    use awaken_session_configuration_contract::SessionInit;
    let roots = ManagedSessionRootFixture::new().await;
    let host = roots
        .configure(crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    install_test_session_application(&host);
    let managed = host.clone();
    roots
        .install_complete_test_session(
            &managed,
            "lazy-environment",
            SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .unwrap();

    assert!(
        host.realization_executor
            .environments()
            .session_environment("lazy-environment")
            .await
            .expect("installed Sessions")
            .is_none(),
        "preparation must not create a Hand/Sandbox"
    );
    run_prepared_session_messages(
        &managed,
        "assistant",
        "lazy-environment",
        vec![Message::text(
            MessageId("lazy-user".into()),
            Role::User,
            "hello",
        )],
    )
    .await
    .expect("first Run waits for environment readiness");
    assert!(
        host.realization_executor
            .environments()
            .session_environment("lazy-environment")
            .await
            .expect("installed Sessions")
            .is_some()
    );
}

/// Cause/effect graph: C1 the Host is Coordinator-only; C2 the frozen
/// Environment is eager; C3 context construction is needed to serialize a Run;
/// C4 the exact delivered Skill includes filesystem support. C1 dominates C2
/// and assigns C4 realization to the claimed Worker: E1 construct the dispatch
/// context and Skill registry, E2 retain the frozen Environment snapshot, E3
/// allocate and write no physical sandbox. The local-pool materialization row is
/// covered by `replacing_a_manifest_removes_the_old_delivered_skill_tree_immediately`.
///
/// | Rule | Coordinator-only | Provisioning | Filesystem Skill | Context/registry | Physical environment |
/// |---|---|---|---|---|---|
/// | D1 | yes | eager | yes | built | absent |
/// | D2 | no | eager | yes | built | resident + files |
/// | D3 | any | on_tool_use | no | inference only | absent |
#[tokio::test]
async fn coordinator_dispatch_context_never_materializes_an_eager_environment() {
    // Constraint/Invariant: Coordinator-side dispatch composition is effect-free;
    // only the claimed Worker may realize an Environment. Decision rule: build
    // the dispatch context and require the zero-materialization effects below.
    use awaken_session_configuration_contract::SessionInit;
    use awaken_skill_store::{SkillBundleFile, SkillVersion, bundle_sha256};
    let mut host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
    host.execution.local = crate::LocalExecution::RemoteWorkersOnly;
    let host = (host).build().expect("validated Host dependencies");

    install_test_session_application(&host);
    host.clone()
        .run_assembly
        .install_test_session_init(
            "coordinator-dispatch-only",
            SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .expect("D1 installs frozen dispatch facts");
    let files = vec![
        SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: files\ndescription: inspect files\n---\nRead the bundled guide."
                .to_vec(),
            executable: false,
        },
        SkillBundleFile {
            path: "references/guide.md".into(),
            content: b"guide".to_vec(),
            executable: false,
        },
    ];
    let hash = bundle_sha256(&files);
    host.run_assembly
        .installed_sessions
        .realizations()
        .update_realization("coordinator-dispatch-only", |r| {
            r.staging_mut().skills = Some(vec![SkillVersion {
                id: "skver_files_1".into(),
                skill_id: "files".into(),
                version: 1,
                name: "files".into(),
                description: "inspect files".into(),
                directory: "/skills/files".into(),
                bundle_sha256: hash,
                files,
                created_unix_nanos: 0,
            }]);
        })
        .expect("installed Sessions");

    let context = host
        .run_assembly
        .ctx_for("coordinator-dispatch-only", Some("assistant"))
        .await
        .expect("D1 builds a sandbox-free dispatch context");
    assert!(context.skill_registry.is_some(), "D1/E1");
    assert!(
        host.realization_executor
            .environments()
            .session_environment("coordinator-dispatch-only")
            .await
            .expect("installed Sessions")
            .is_none(),
        "D1/E3"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read("coordinator-dispatch-only", |installed| installed
                .inputs
                .environment_snapshot
                .is_some())
            .expect("installed Sessions")
            .unwrap_or(false),
        "D1/E2"
    );
}

/// Cause/effect graph: C1 the Host is Coordinator-only; C2 immutable
/// provisioning is BackendOwned; C3 placement is Local or Worker; C4 the
/// frozen layout is valid or structurally conflicting; C5 the caller installs
/// Dispatch or physical Realization facts; C6 a trusted-host provider is
/// present. Effects: E1 a Worker admission/Dispatch performs structural
/// validation without requesting the Coordinator's provider; E2 a structural
/// conflict is rejected before frozen facts; E3 Local/physical validation
/// requires the exact provider; E4 a claimed Worker with that provider passes;
/// E5 no Coordinator environment is created. Create and persisted Resource
/// updates share the prospective port, so P1-P3 own both entry points.
///
/// | Rule | Placement/stage | Provider | Layout | Effect |
/// |---|---|---|---|---|
/// | P1 | Worker prospective | absent | valid | E1+E5 |
/// | P2 | Worker prospective | absent | conflict | E2+E5 |
/// | P3 | Local prospective | absent | valid | E3 |
/// | P4 | Worker Dispatch | absent | valid | E1+E5 |
/// | P5 | Worker Dispatch | absent | conflict | E2+E5 |
/// | P6 | Worker physical install | absent | valid | E3 |
/// | P7 | Worker physical install | present | valid | E4 |
/// | B1 | Coordinator context | absent | valid | E1+E5 |
/// | B2 | local context | absent | valid | E3 |
///
/// The contract-owned path kernel remains the sole structural authority; this
/// test owns only the placement-to-validation-scope decision.
#[tokio::test]
async fn coordinator_defers_backend_owned_environment_to_the_claimed_worker() {
    // The absent provider in B1/B2 is the physical Environment provider. The
    // exact ACP executor factory remains installed so this test isolates the
    // Coordinator-versus-Worker placement decision.
    let mut dependencies = crate::acp_backend::with_fixture_acp(
        crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await,
    );
    dependencies.execution.local = crate::LocalExecution::RemoteWorkersOnly;
    let host = (dependencies).build().expect("validated Host dependencies");
    let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
        awaken_runtime_contract::resolved::ModelBinding::new("local", "", "acp:codex"),
        awaken_agent_contract::credential::CredentialRef {
            id: "codex-login".into(),
            revision: 1,
        },
        awaken_runtime_contract::resolved::BackendModelSelection::Default,
        "codex-test",
        "sha256:codex-test",
        Default::default(),
    )
    .expect("coherent backend-owned candidate");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("local-codex")
        .resolved_model(candidate.clone())
        .build();

    let context = host
        .run_assembly
        .ctx_for_snapshot(
            "coordinator-backend-owned",
            Some("local-codex"),
            Some(snapshot.clone()),
        )
        .await
        .expect("B1 builds the dispatch context without a trusted-host provider");

    assert!(context.env.is_none(), "B1/E2");
    assert!(
        host.realization_executor
            .environments()
            .session_environment("coordinator-backend-owned")
            .await
            .expect("installed Sessions")
            .is_none(),
        "B1/E2"
    );

    let model_override = || awaken_session_configuration_contract::SessionModelOverride {
        publication: Some(Box::new(
            awaken_session_configuration_contract::SessionModelPublication {
                primary: candidate.clone(),
                candidates: Vec::new(),
            },
        )),
        inference: Default::default(),
    };
    let mount = || awaken_provisioning_contract::MountRequirement {
        mount_id: "project".into(),
        source: awaken_provisioning_contract::MountSource::Inline {
            contents: "project".into(),
        },
        mount_path: "/workspace/project".into(),
        access: awaken_provisioning_contract::MountAccess::ReadOnly,
        lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
        required: true,
    };
    let repository_binding = awaken_resource_contract::InputBinding {
        binding_id: awaken_resource_contract::BindingId::from("repo"),
        target: awaken_resource_contract::InputResourceId::Repository(
            awaken_resource_contract::RepositoryId::from("repo"),
        ),
        mount_path: "/workspace/project/repository".into(),
        access: awaken_resource_contract::ResourceAccess::ReadWrite,
        instructions: None,
    };
    let layout = |runtime_placement, mounts, resources| {
        awaken_session_configuration_contract::SessionSandboxLayout {
            workspace_id: host.run_assembly.local_workspace().into(),
            agent_id: "local-codex".into(),
            agent_revision: None,
            runtime_placement,
            model_override: Some(model_override()),
            runtime: Some("acp:codex".into()),
            mounts,
            env: Vec::new(),
            environment: on_tool_use_environment(),
            resources,
        }
    };
    let managed = host.clone();
    managed
        .realization_executor
        .validate_session_sandbox_layout(
            "worker-prospective-valid",
            &layout(
                awaken_session_types::SessionRuntimePlacement::Worker,
                Vec::new(),
                Vec::new(),
            ),
        )
        .expect("P1 Worker admission needs no Coordinator provider");
    let conflict = managed
        .realization_executor
        .validate_session_sandbox_layout(
            "worker-prospective-conflict",
            &layout(
                awaken_session_types::SessionRuntimePlacement::Worker,
                vec![mount()],
                vec![repository_binding],
            ),
        )
        .expect_err("P2 structural conflict remains fail-closed");
    assert!(conflict.to_string().contains("overlaps"), "P2: {conflict}");
    let local_error = managed
        .realization_executor
        .validate_session_sandbox_layout(
            "local-prospective-valid",
            &layout(
                awaken_session_types::SessionRuntimePlacement::Local,
                Vec::new(),
                Vec::new(),
            ),
        )
        .expect_err("P3 Local admission requires its exact provider");
    assert!(
        local_error
            .to_string()
            .contains("BackendOwned provisioning requires a trusted-host Session provider"),
        "P3: {local_error}"
    );

    let projection = |mounts, resources| {
        let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment: on_tool_use_environment(),
                runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
                mcp_authoring: Default::default(),
                agent_id: "local-codex".into(),
                agent_revision: None,
                model: candidate.binding().identity().1.to_owned(),
                model_override: Some(model_override()),
                runtime: Some(candidate.binding().identity().2.to_owned()),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts,
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        );
        awaken_session_realization_contract::FrozenSessionProjection {
            workspace_id: host.run_assembly.local_workspace().into(),
            revision: awaken_session_types::SessionRevision(1),
            baseline,
            agent_publication: None,
            environment_materialization: None,
            environment: Default::default(),
            resource_revision: 0,
            resources,
            previous_resource_manifest: Some(
                crate::session_resource::SessionResourceManifest::new(
                    host.run_assembly.local_workspace(),
                    crate::session_resource::ResolvedSessionResources::default(),
                ),
            ),
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
        }
    };
    host.run_assembly
        .installed_sessions
        .install_dispatch_frozen_session_projection(
            (
                &host.run_assembly.realization_executor,
                &host.run_assembly.mcp,
            ),
            "worker-dispatch-valid",
            projection(
                Vec::new(),
                crate::session_resource::ResolvedSessionResources::default(),
            ),
        )
        .await
        .expect("P4 Worker Dispatch needs no Coordinator provider");
    assert!(
        host.realization_executor
            .environments()
            .session_environment("worker-dispatch-valid")
            .await
            .expect("installed Sessions")
            .is_none(),
        "P4/E5"
    );
    let dispatch_conflict = host
        .run_assembly
        .installed_sessions
        .install_dispatch_frozen_session_projection(
            (
                &host.run_assembly.realization_executor,
                &host.run_assembly.mcp,
            ),
            "worker-dispatch-conflict",
            projection(
                vec![mount()],
                effective_resources(vec![TestInput {
                    kind: "github_repository".into(),
                    id: "https://example.invalid/repo.git".into(),
                    mount_path: "/workspace/project/repository".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadWrite,
                    instructions: None,
                    initial_branch: None,
                    initial_commit: None,
                }]),
            ),
        )
        .await
        .expect_err("P5 Dispatch rejects a structural conflict");
    assert!(
        dispatch_conflict.message.contains("overlaps"),
        "P5: {dispatch_conflict:?}"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read("worker-dispatch-conflict", |installed| installed
                .inputs
                .baseline
                .is_none())
            .expect("installed Sessions")
            .unwrap_or(true),
        "P5/E2"
    );

    let physical_without_provider =
        (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
            .build()
            .expect("validated Host dependencies");
    let realization_error = physical_without_provider
        .run_assembly
        .installed_sessions
        .install_frozen_session_projection(
            (
                &physical_without_provider.run_assembly.realization_executor,
                &physical_without_provider.run_assembly.mcp,
            ),
            "worker-realization-missing-provider",
            projection(
                Vec::new(),
                crate::session_resource::ResolvedSessionResources::default(),
            ),
            None,
            true,
            None,
        )
        .await
        .expect_err("P6 physical install requires the exact provider");
    assert!(
        realization_error
            .message
            .contains("BackendOwned provisioning requires a trusted-host Session provider"),
        "P6: {realization_error:?}"
    );
    let mut physical = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
    physical.env.backend_owned_session_provider = Some(
        awaken_session_environment::SandboxEnvironmentProvider::workdir_with_agent_stderr(
            std::env::temp_dir().join("awaken-worker-layout-provider"),
            false,
        )
        .into_session_provider(),
    );
    let physical = (physical).build().expect("validated Host dependencies");
    physical
        .run_assembly
        .installed_sessions
        .install_frozen_session_projection(
            (
                &physical.run_assembly.realization_executor,
                &physical.run_assembly.mcp,
            ),
            "worker-realization-valid",
            projection(
                Vec::new(),
                crate::session_resource::ResolvedSessionResources::default(),
            ),
            None,
            true,
            None,
        )
        .await
        .expect("P7 claimed Worker validates its exact trusted provider");

    let local = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let error = match local
        .run_assembly
        .ctx_for_snapshot("local-backend-owned", Some("local-codex"), Some(snapshot))
        .await
    {
        Ok(_) => panic!("B2 rejects execution without a trusted-host provider"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("BackendOwned provisioning requires a trusted-host Session provider"),
        "B2: {error}"
    );
    assert!(
        local
            .realization_executor
            .environments()
            .session_environment("local-backend-owned")
            .await
            .expect("installed Sessions")
            .is_none(),
        "B2 environment"
    );
}

#[tokio::test]
async fn frozen_environment_selects_trusted_host_exactly() {
    // Cause/effect graph: C1 host_access={trusted_host,isolated}; C2 requested
    // isolation={workdir,namespace}; C3 local Workdir provider={present,absent};
    // C4 provisioning={HostExecutor,BackendOwned}. Effects: E1 trusted_host
    // selects the Workdir provider even for HostExecutor; E2 absence fails
    // closed; E3 isolated Namespace never falls back to host identity for a
    // BackendOwned Agent. Decision rules: T1=trusted+workdir+present=>E1;
    // T2=trusted+workdir+absent=>E2; I1=isolated+namespace+BackendOwned=>E3.
    // Constraints: the frozen Environment projection is the sole selector and
    // provider strength cannot substitute for the exact trust decision.
    let namespace_root = tempfile::tempdir().unwrap();
    let trusted_root = tempfile::tempdir().unwrap();
    let mut host = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
    host.env.trusted_host = crate::TrustedHostPolicy::Admitted;
    host.env.session_provider =
        awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
            namespace_root.path(),
            false,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .into_session_provider();
    host.env.backend_owned_session_provider = Some(
        awaken_session_environment::SandboxEnvironmentProvider::
            trusted_host_workdir_with_agent_stderr(trusted_root.path(), false).into_session_provider(),
    );
    let host = (host).build().expect("validated Host dependencies");

    let mut trusted = session_environment(
        awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        serde_json::json!({
            "isolation": "workdir",
            "host_access": "trusted_host"
        }),
    );
    trusted.self_hosted = true;
    host.run_assembly
        .install_environment_projection("T1", &trusted)
        .unwrap();
    let selected = host
        .realization_executor
        .environments()
        .session_environment_provider_for_thread(
            "T1",
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        )
        .unwrap();
    assert!(selected.realizer().supports_host_identity(), "T1/E1");

    let mut absent = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
    absent.env.trusted_host = crate::TrustedHostPolicy::Admitted;
    absent.env.session_provider =
        awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
            namespace_root.path(),
            false,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .into_session_provider();
    let absent = (absent).build().expect("validated Host dependencies");
    absent
        .run_assembly
        .install_environment_projection("T2", &trusted)
        .unwrap();
    let error = match absent
        .realization_executor
        .environments()
        .session_environment_provider_for_thread(
            "T2",
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        ) {
        Ok(_) => panic!("T2/E2"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("local Workdir provider"),
        "T2/E2"
    );

    let mut isolated = session_environment(
        awaken_session_configuration_contract::SessionNetworkPolicy::None,
        serde_json::json!({"isolation": "namespace"}),
    );
    isolated.self_hosted = true;
    host.run_assembly
        .install_environment_projection("I1", &isolated)
        .unwrap();
    let backend_owned =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
            awaken_runtime_contract::resolved::ModelBinding::new("local", "", "acp:codex"),
            awaken_agent_contract::credential::CredentialRef {
                id: "codex-login".into(),
                revision: 1,
            },
            awaken_runtime_contract::resolved::BackendModelSelection::Default,
            "codex",
            "sha256:codex",
            Default::default(),
        )
        .unwrap();
    let error = match host
        .realization_executor
        .environments()
        .session_environment_provider_for_thread("I1", backend_owned.provisioning())
    {
        Ok(_) => panic!("I1/E3"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("trusted_host Workdir"), "I1/E3");
}

/// L1: `on_tool_use` means inference alone must not allocate a Sandbox.
#[tokio::test]
async fn on_tool_use_text_only_run_keeps_the_environment_absent() {
    // Test design. Causes: C1 a text-only Run invokes no environment-requiring
    // tool. Effects: E1 no Environment is created or persisted. Constraint/
    // Invariant: tool demand, not Run existence, owns lazy materialization.
    // Decision rule: complete the text-only partition and require E1.
    use awaken_session_configuration_contract::SessionInit;
    let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    install_test_session_application(&host);
    let managed = host.clone();
    managed
        .run_assembly
        .install_test_session_init(
            "deferred-text",
            SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

    run_prepared_session_messages(
        &managed,
        "assistant",
        "deferred-text",
        vec![Message::text(MessageId("u1".into()), Role::User, "hello")],
    )
    .await
    .unwrap();
    assert!(
        host.realization_executor
            .environments()
            .session_environment("deferred-text")
            .await
            .expect("installed Sessions")
            .is_none()
    );
}

/// Delegation/provisioning cause-effect graph: C1=`on_tool_use`; C2=the exact
/// publication contains a delegate; C3=this Host executes locally; C4 a Managed
/// Session has its required coordination authority. Effects:
/// E1=plain inference without C2 stays deferred; E2=C1+C2+C3 creates one
/// Session Environment before the delegation service is exposed; E3=a
/// Coordinator-only Host remains environment-free because the claimed Worker
/// owns E2; E4=!C4 fails closed before any environment mutation; E5 a second
/// local coordination binding is rejected instead of replacing the first.
/// Decision rows L1, L9, and D1 cover E1, E2, and E3 respectively. L9 first
/// exercises !C4=>E4, then C4=>E2, and C4+duplicate=>E5.
#[tokio::test]
async fn on_tool_use_published_delegate_forces_one_eager_environment() {
    // Managed physical row: real SQLite Root + controlled live lease + the
    // production binding sink precede first I/O. Installation itself has no
    // provider effect; the original denial/lazy/coordination assertions below
    // retain their causes and cannot borrow a fabricated Authorized result.
    // Constraint/Invariant: the published delegate requirement is frozen before
    // execution and may materialize exactly one Environment. Decision rule:
    // execute the delegate-demand partition and require one eager realization.
    use awaken_runtime_contract::StaticPublishedAgentSnapshots;
    use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
    use awaken_runtime_contract::snapshot::AgentId;
    use awaken_session_configuration_contract::SessionInit;

    let child = awaken_runtime_contract::ExecutableAgentSnapshot::builder("child")
        .model(test_model_binding())
        .build();
    let parent = awaken_runtime_contract::ExecutableAgentSnapshot::builder("parent")
        .model(test_model_binding())
        .agent_bindings(AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("child".into()),
                source_revision: None,
                recursive_self: false,
            }],
            ..Default::default()
        })
        .build();
    let publications = StaticPublishedAgentSnapshots::try_new([parent, child])
        .expect("one authoritative publication catalog");
    let roots = ManagedSessionRootFixture::new().await;
    let host = roots
        .configure(
            crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
                .await
                .with_agent_publications(Arc::new(publications)),
        )
        .build()
        .expect("validated Host dependencies");
    let managed = host.clone();
    roots
        .install_complete_test_session(
            &managed,
            "deferred-delegate",
            SessionInit {
                workspace_id: host.run_assembly.local_workspace().into(),
                agent_id: "parent".into(),
                delegate_ids: vec!["child".into()],
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect("prepare exact publication");

    let missing = match host
        .run_assembly
        .ctx_for("deferred-delegate", Some("parent"))
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("L9/E4 missing coordination authority must fail closed"),
    };
    assert!(
        missing
            .to_string()
            .contains("no Session application authority"),
        "L9/E4: {missing}"
    );
    assert!(
        host.realization_executor
            .environments()
            .session_environment("deferred-delegate")
            .await
            .expect("installed Sessions")
            .is_none(),
        "L9/E4 no environment side effect"
    );

    let coordination: Arc<dyn awaken_session_execution_contract::SessionAgentCoordination> =
        Arc::new(RejectingSessionAgentCoordination);
    managed
        .session_application_bindings()
        .bind_agent_coordination(coordination.clone())
        .expect("L9 one coordination application");
    assert_eq!(
        managed
            .session_application_bindings()
            .bind_agent_coordination(coordination.clone()),
        Err(crate::SessionApplicationBindingError::AlreadyBound),
        "L9/E5"
    );

    let context = host
        .run_assembly
        .ctx_for("deferred-delegate", Some("parent"))
        .await
        .expect("L9 delegate context");
    assert!(context.env.is_some(), "L9/E2 runtime environment");
    assert!(
        host.realization_executor
            .environments()
            .session_environment("deferred-delegate")
            .await
            .expect("installed Sessions")
            .is_some(),
        "L9/E2 single Session owner"
    );
}

#[tokio::test]
async fn model_request_gate_follows_the_session_dispatch_decision_table() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_session_configuration_contract::SessionInit;

    // Cause/effect graph: C1 is the canonical `session_dispatch` projection;
    // C2 is an installed Session application endpoint. E1 installs the one
    // per-request gate; E2 omits it; E3 rejects construction before execution.
    // Direct protocol Threads never gain C1 merely because the process also
    // serves Managed Sessions. Managed primary and child Threads both carry C1.
    //
    // | Rule | C1 | C2 | Effect |
    // | G1   | no | no | E2     |
    // | G2   | no | yes| E2     |
    // | G3   | yes| yes| E1     |
    // | G4   | yes| no | E3     |
    // | G5   | inherited child of G3 | yes | E1 with the same authority |
    let direct = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let _direct_runtime = direct.clone();
    let direct_context = direct
        .run_assembly
        .ctx_for("gate-direct-absent", None)
        .await
        .unwrap();
    assert!(
        direct_context.attempt_context.model_request_gate.is_none(),
        "G1"
    );

    let coordination: Arc<dyn awaken_session_execution_contract::SessionAgentCoordination> =
        Arc::new(RejectingSessionAgentCoordination);
    let direct_with_endpoint =
        (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
            .build()
            .expect("validated Host dependencies");
    direct_with_endpoint
        .clone()
        .session_application_bindings()
        .bind_agent_coordination(coordination.clone())
        .unwrap();
    let direct_context = direct_with_endpoint
        .run_assembly
        .ctx_for("gate-direct-present", None)
        .await
        .unwrap();
    assert!(
        direct_context.attempt_context.model_request_gate.is_none(),
        "G2"
    );

    let session_init = || SessionInit {
        workspace_id: "default".into(),
        agent_id: "assistant".into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: None,
        runtime: None,
        environment: on_tool_use_environment(),
    };
    let managed = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    let managed_runtime = managed.clone();
    managed_runtime
        .run_assembly
        .install_test_session_init("gate-managed-present", session_init())
        .await
        .unwrap();
    managed_runtime
        .session_application_bindings()
        .bind_agent_coordination(coordination.clone())
        .unwrap();
    let managed_context = managed
        .run_assembly
        .ctx_for("gate-managed-present", None)
        .await
        .unwrap();
    assert!(
        managed_context.attempt_context.model_request_gate.is_some(),
        "G3"
    );
    let managed_gate = managed_context
        .attempt_context
        .model_request_gate
        .as_ref()
        .expect("G5 root gate")
        .clone();
    let managed_child = managed_context.attempt_context.for_child_run();
    assert!(
        Arc::ptr_eq(
            managed_child
                .model_request_gate
                .as_ref()
                .expect("G5 child gate"),
            &managed_gate,
        ),
        "G5 a synchronous child inherits the exact Session request authority"
    );

    let missing = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .expect("validated Host dependencies");
    missing
        .clone()
        .run_assembly
        .install_test_session_init("gate-managed-absent", session_init())
        .await
        .unwrap();
    let error = match missing
        .run_assembly
        .ctx_for("gate-managed-absent", None)
        .await
    {
        Ok(_) => panic!("G4 must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("no Session application authority"),
        "G4: {error}"
    );
}
