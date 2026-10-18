//! Retained Repository content is mutable; its initialization preference is not
//! a re-entry instruction. Exercise the production Resource projection owner.

use super::*;

#[tokio::test]
async fn adopted_repository_preserves_work_branch_commit_and_uncommitted_files() {
    // Causes: C1 the root already activated the exact Repository config;
    // C2 the retained Environment owns this path; C3 adoption re-enters the
    // complete Resource transition; C4 the Agent changed branch/HEAD and WIP.
    // Effects: E1 preserve branch, commit and dirty/untracked bytes; E2 verify
    // retained repository identity without cloning or resetting; E3 publish
    // the same active manifest. R1 C1+C2+C3+C4 -> E1+E2+E3. First-create retry,
    // foreign origin, missing path, symlink and unowned paths remain negative
    // provider rules; they must not be weakened to make this positive row pass.
    let temp = tempfile::tempdir().unwrap();
    let sandbox_root = temp.path().join("sandboxes");
    let provider = awaken_sandbox_local::NamespaceProvider::new(&sandbox_root);
    pc::SandboxProvider::probe_ready(&provider)
        .await
        .expect("Linux namespace substrate");
    let remote = temp.path().join("remote.git");
    let seed = temp.path().join("seed");
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(
        temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            remote.to_str().unwrap(),
        ],
    );
    git(
        temp.path(),
        &["clone", remote.to_str().unwrap(), seed.to_str().unwrap()],
    );
    git(&seed, &["config", "user.name", "seed"]);
    git(&seed, &["config", "user.email", "seed@example.invalid"]);
    std::fs::write(seed.join("README.md"), "base").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-m", "base"]);
    git(&seed, &["push", "origin", "main"]);

    let mut dependencies = crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await;
    dependencies.env.session_provider =
        awaken_session_environment::SandboxEnvironmentProvider::namespace_with_agent_stderr(
            sandbox_root,
            false,
            Arc::new(awaken_session_environment::test_support::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        )
        .into_session_provider();
    assert!(
        dependencies
            .env
            .session_provider
            .realizer()
            .capabilities()
            .path_fidelity
    );
    let host = crate::host::with_test_resource_validation(dependencies)
        .build()
        .unwrap();
    let managed = managed_with_resource_source(host.clone());
    managed
        .run_assembly
        .install_test_environment_binding_sink(Arc::new(BindingOrderSink {
            host: Arc::downgrade(&host),
            authorize_calls: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            observed_before_publish: AtomicBool::new(false),
            fail: false,
            require_realization: false,
            owned_session_id: None,
            committed_environment: None,
        }));
    let resources = effective_repository(
        "retained-repository",
        remote.to_str().unwrap(),
        "/workspace/repository",
        None,
    );
    let binding = resources.inputs()[0].binding_id.clone();
    let resources = resources
        .update_input(&binding, |input| {
            let crate::session_resource::ResolvedInputSource::Repository { config, .. } =
                &mut input.source
            else {
                unreachable!()
            };
            config.initial_branch = Some("main".into());
        })
        .unwrap();
    let thread = "repository-continuity";
    let workspace = host.run_assembly.local_workspace().to_string();
    let lease = crate::session_resource::SessionRealizationLease {
        owner: "continuity-worker".into(),
        runtime_incarnation: "continuity-worker/1".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms()
            .saturating_add(120_000),
    };
    let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment: session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
            runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model_override: None,
            model: "stub".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    let manifest = crate::session_resource::SessionResourceManifest::at_revision(
        &workspace,
        1,
        resources.clone(),
    );
    let projection = awaken_session_realization_contract::FrozenSessionProjection {
        workspace_id: workspace.clone(),
        revision: awaken_session_types::SessionRevision(1),
        baseline,
        agent_publication: None,
        environment_materialization: None,
        environment: Default::default(),
        resource_revision: 1,
        resources,
        previous_resource_manifest: Some(
            crate::session_resource::SessionResourceManifest::at_revision(
                &workspace,
                0,
                Default::default(),
            ),
        ),
        tools: Default::default(),
        mcp: Vec::new(),
        request_context: Vec::new(),
    };
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        managed.run_assembly.as_ref(),
        thread,
        projection,
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .unwrap();
    host.run_assembly
        .install_session_realization_lease(thread, lease.clone())
        .unwrap();
    run_prepared_session_messages(
        &managed,
        "agent",
        thread,
        vec![Message::text(
            MessageId("initialize".into()),
            Role::User,
            "prepare repository",
        )],
    )
    .await
    .unwrap();
    let environment = host
        .realization_executor
        .environments()
        .session_environment(thread)
        .await
        .expect("installed Sessions")
        .unwrap();
    assert!(
        environment
            .lifecycle()
            .handle()
            .owned_paths()
            .unwrap()
            .iter()
            .any(|p| p == "/workspace/repository")
    );
    let status = environment.sandbox_environment().run_test_command(pc::Command::new([
        "sh", "-c", concat!(
            "git -C repository config user.name agent && git -C repository config user.email agent@example.invalid && ",
            "git -C repository checkout -b awf/work && printf checkpoint > repository/README.md && ",
            "git -C repository add README.md && git -C repository commit -m checkpoint && ",
            "printf dirty > repository/README.md && printf untracked > repository/new.txt && ",
            "mkdir -p .continuity-proof && git -C repository rev-parse HEAD > .continuity-proof/before"
        ),
    ])).await.unwrap();
    assert_eq!(status.code, Some(0));
    // This is the same typed adoption cause used by the production provider
    // path: its durable handle/path evidence above is retained, while the
    // process-local Resource completion cache must be re-established.
    // Rebased fixture rule: cold adoption updates the canonical installed
    // Session view; a legacy registry or forwarding alias must not participate.
    host.run_assembly
        .installed_sessions
        .realizations()
        .update_realization(thread, |state| {
            state.staging_mut().reconciliation =
                crate::staged_resources::EnvironmentResourceReconciliation::Adopted;
        })
        .expect("installed Sessions");
    let transition =
        crate::session_resource::SessionResourceTransition::new(manifest.clone(), manifest.clone())
            .unwrap();
    managed
        .realization_executor
        .apply_session_inputs(thread, &transition, None, Some(&lease))
        .await
        .expect("R1 adoption must not replay the main-branch initialization");
    let status = environment.sandbox_environment().run_test_command(pc::Command::new([
        "sh", "-c", concat!(
            "test \"$(git -C repository symbolic-ref --short HEAD)\" = awf/work && ",
            "test \"$(git -C repository rev-parse HEAD)\" = \"$(cat .continuity-proof/before)\" && ",
            "test \"$(cat repository/README.md)\" = dirty && test \"$(cat repository/new.txt)\" = untracked"
        ),
    ])).await.unwrap();
    assert_eq!(status.code, Some(0), "R1 branch/commit/WIP preserved");
    assert_eq!(
        host.realization_executor
            .staging()
            .thread_resource_manifest(thread)
            .expect("installed Sessions"),
        Some(manifest)
    );
    // Retention provider rules: R2 an exact owned writable tree preserves
    // changed refs; R3 readonly mismatch, R4 foreign origin, R5 absent path,
    // R6 symlink refuse before clone/reset. All refusal rows preserve bytes.
    let plan = host
        .realization_executor
        .staging()
        .thread_repository_activations(thread)[0]
        .plan
        .clone();
    let mut readonly = plan.clone();
    readonly.access = pc::MountAccess::ReadOnly;
    assert!(
        environment
            .repositories()
            .realize_repository(&readonly, None, pc::RepositoryRealizationIntent::Retain)
            .await
            .is_err(),
        "R3 readonly ref remains exact"
    );
    let mut foreign = plan.clone();
    foreign.transport_url = "https://foreign.example.invalid/repository".into();
    assert!(
        environment
            .repositories()
            .realize_repository(&foreign, None, pc::RepositoryRealizationIntent::Retain)
            .await
            .is_err(),
        "R4 foreign origin"
    );
    let status = environment
        .sandbox_environment()
        .run_test_command(pc::Command::new([
            "sh",
            "-c",
            "mv repository retained-backup",
        ]))
        .await
        .unwrap();
    assert_eq!(status.code, Some(0));
    assert!(
        environment
            .repositories()
            .realize_repository(&plan, None, pc::RepositoryRealizationIntent::Retain)
            .await
            .is_err(),
        "R5 absence cannot recreate content"
    );
    let status = environment
        .sandbox_environment()
        .run_test_command(pc::Command::new([
            "sh",
            "-c",
            "test ! -e repository && ln -s retained-backup repository",
        ]))
        .await
        .unwrap();
    assert_eq!(status.code, Some(0), "R5 no hidden clone");
    assert!(
        environment
            .repositories()
            .realize_repository(&plan, None, pc::RepositoryRealizationIntent::Retain)
            .await
            .is_err(),
        "R6 symlink is not owned continuity"
    );
    let status = environment.sandbox_environment().run_test_command(pc::Command::new(["sh", "-c", "test -L repository && unlink repository && mv retained-backup repository && test \"$(cat repository/README.md)\" = dirty && test \"$(cat repository/new.txt)\" = untracked"])).await.unwrap();
    assert_eq!(status.code, Some(0), "R3-R6 preserve WIP");
    // This fixture has no Session aggregate disposal command. Stop its exact
    // process bindings; the owned temporary directory guard removes fixture
    // bytes after the Host drops, without inventing a cleanup receipt.
    environment
        .lifecycle()
        .stop_bound_processes()
        .await
        .unwrap();
}
