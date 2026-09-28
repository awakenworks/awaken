//! Container repository/file transfer, hand rebinding, and restore selection.

use super::*;

#[cfg(feature = "docker-tests")]
// Container Repository transfer rules: C1 a non-default Agent work branch
// has one committed change -> publish that exact branch without changing
// the base branch; C2 replay the unchanged container state -> no second
// push; C3 outputs and authored Skills remain independently harvestable.
#[tokio::test]
async fn docker_environment_transfers_repo_and_harvests_files_without_exposing_token() {
    let Ok(image) = std::env::var("AWAKEN_TEST_SESSION_IMAGE") else {
        eprintln!("skipping: AWAKEN_TEST_SESSION_IMAGE is not set");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let remote = temp.path().join("remote.git");
    let seed = temp.path().join("seed");
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let status = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
    git(
        temp.path(),
        &["clone", remote.to_str().unwrap(), seed.to_str().unwrap()],
    );
    git(&seed, &["config", "user.name", "seed"]);
    git(&seed, &["config", "user.email", "seed@example.invalid"]);
    std::fs::write(seed.join("README.md"), "base").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-m", "base"]);
    git(&seed, &["push", "-u", "origin", "HEAD"]);

    let runtime =
        Arc::new(awaken_sandbox_container::docker::DockerRuntime::connect_local(8080).unwrap());
    let provider = Arc::new(awaken_sandbox_container::ContainerProvider::new(
        runtime, image,
    ));
    let mut docker_spec = spec();
    docker_spec.scope = format!("host-repo-real-{}", std::process::id());
    docker_spec.outputs_path = "/mnt/session/outputs".into();
    let environment = SessionEnvironmentProvider::container(
        provider,
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .create(&docker_spec)
    .await
    .unwrap();
    let repository_plan = pc::RepositoryRealizationPlan {
        repository_id: "repo".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: remote.to_string_lossy().into_owned(),
        transport_url: remote.to_string_lossy().into_owned(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };
    pc::RepositoryRealizer::realize_repository(&environment, &repository_plan, None)
        .await
        .unwrap();
    environment
        .materialize_inline("/workspace/.mnt/live.txt", b"live")
        .await
        .unwrap();
    assert_eq!(
        environment.list_workspace_files(".mnt").await.unwrap(),
        vec![("live.txt".into(), b"live".to_vec())]
    );
    environment
        .remove_projection_path(".mnt/live.txt")
        .await
        .unwrap();
    assert!(
        environment
            .list_workspace_files(".mnt")
            .await
            .unwrap()
            .is_empty()
    );

    let change = environment
        .sandbox()
        .spawn(pc::Command::new([
            "sh",
            "-c",
            concat!(
                "git -C /workspace/repo config user.name agent && ",
                "git -C /workspace/repo config user.email agent@example.invalid && ",
                "git -C /workspace/repo checkout -b awf/work && ",
                "printf changed > /workspace/repo/README.md && ",
                "git -C /workspace/repo add README.md && ",
                "git -C /workspace/repo commit -m changed && ",
                "git -C /workspace/repo rev-parse HEAD > /workspace/.mnt/commit.txt && ",
                "mkdir -p /workspace/outputs/nested && ",
                "printf '\\000\\377' > /workspace/outputs/nested/result.bin && ",
                "mkdir -p /workspace/skills/authored && ",
                "printf '%s' '---\ndescription: authored\n---\nbody' > ",
                "/workspace/skills/authored/SKILL.md"
            ),
        ]))
        .await
        .unwrap();
    assert_eq!(change.wait().await.unwrap().code, Some(0));
    environment.register_skill_dir("skills");
    environment.refresh_skills().await.unwrap();
    let skills = environment.scan_skill_dir("skills").unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "authored");
    let commit = environment
        .list_workspace_files(".mnt")
        .await
        .unwrap()
        .into_iter()
        .find_map(|(path, bytes)| (path == "commit.txt").then_some(bytes))
        .expect("container exported its exact commit coordinate");
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "awf/work".into(),
        commit: String::from_utf8(commit).unwrap().trim().into(),
        expected_prior_commit: None,
    };
    environment
        .remove_projection_path(".mnt/commit.txt")
        .await
        .unwrap();
    let first = pc::RepositoryRealizer::publish_repository(
        &environment,
        &repository_plan,
        &expectation,
        None,
    )
    .await
    .unwrap();
    let replay = pc::RepositoryRealizer::publish_repository(
        &environment,
        &repository_plan,
        &expectation,
        None,
    )
    .await
    .unwrap();
    assert_eq!(first, replay, "C1/C2 canonical receipt");
    // Artifact cause/effect rule: one regular output file → one canonical
    // content-addressed Artifact and binary-safe read through the same port.
    let artifacts = environment.capture_artifacts().await.unwrap();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].metadata.path.ends_with("nested/result.bin"));
    assert_eq!(artifacts[0].metadata.id, artifacts[0].metadata.content_hash);
    assert_eq!(artifacts[0].bytes, vec![0, 0xff]);
    environment.dispose().await.unwrap();

    git(
        temp.path(),
        &[
            "--git-dir",
            remote.to_str().unwrap(),
            "show-ref",
            "--verify",
            "refs/heads/awf/work",
        ],
    );
    let count = std::process::Command::new("git")
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-list",
            "--count",
            "--all",
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "2");
}

#[tokio::test]
async fn revoked_durable_container_rebinds_hand_without_reconstructing_its_owner() {
    use crate::host::{
        eager_environment, empty_frozen_projection_for_snapshot, managed_test_host, test_activation,
    };
    use awaken_session_execution_contract::SessionRuntime as _;

    // Causes: C1 exact durable Retiring owner after revocation; C2 typed provider
    // observation unavailable/Ready; C3 attached/resident Hand; C4 repeated
    // revocation; C5 foreign handle/provider. Effects: E1 retain the original
    // owner on failure; E2 same physical Arc, fresh wrapper and usable Hand;
    // E3 no provider adopt/create/dispose, preserving original cleanup/attempt
    // participants; E4 reject mismatched rebinding without a process launch.
    // Rules: W1 C1+unavailable=>E1; W2 C1+Ready+(each C3)+C4=>E2/E3;
    // W3 C5=>E4. Real Docker probe additionally tests CurrentAttemptOnly proof,
    // authenticated Control retirement, new Run admission and physical cleanup.
    for residency in [
        crate::ContainerHandResidency::AttachedExec,
        crate::ContainerHandResidency::Resident,
    ] {
        let fake = Arc::new(FakeContainerProvider::default());
        let provider = SessionEnvironmentProvider::container_with_capacity_hand_idle_and_residency(
            fake.clone(),
            None,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "awaken-sandbox",
            std::time::Duration::ZERO,
            residency,
            awaken_lease_contract::system_clock(),
        );
        let thread = "session-container";
        let host = Arc::new(crate::SharedHost::new(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        ));
        let managed = managed_test_host(host.clone());
        let activation = test_activation(thread, "rebind-run");
        let mut projection = empty_frozen_projection_for_snapshot(
            "workspace",
            eager_environment(),
            &activation.snapshot,
        );
        projection.agent_publication = Some(activation.snapshot.clone());
        managed
            .realization()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "RealizationRuntime is unsupported by this runtime",
                )
            })
            .expect("runtime role")
            .install_session_projection(
                thread,
                projection.clone(),
                awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
            )
            .await
            .unwrap();
        let spec = host.sandbox_spec_for_provider(thread, &provider);
        let original = Arc::new(provider.create(&spec).await.unwrap());
        let handle = original.handle();
        let binding = serde_json::to_string(&handle).unwrap();
        let generation = awaken_session_state_contract::SandboxGeneration::new(
            &awaken_session_types::SessionId::from(thread),
            1,
            u64::MAX,
            "environment",
            "image",
        );
        let identity = crate::session_slot::BoundSessionEnvironmentIdentity::Durable {
            effect_id: "create-effect".into(),
            generation: generation.clone(),
        };
        projection.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: binding.clone(),
            effect_id: Some("create-effect".into()),
            generation: Some(generation),
            idle_since_unix_ms: None,
        };
        host.install_session_environment_owner_projection(
            thread,
            "workspace",
            &projection.environment,
        )
        .unwrap();
        let candidate = host
            .begin_session_environment_adoption(thread, original.clone())
            .unwrap();
        host.publish_prepared_session_environment(thread, &candidate, identity)
            .unwrap();
        let SessionEnvironment::Container {
            sandbox: physical, ..
        } = original.as_ref()
        else {
            unreachable!()
        };
        let mut previous = original.clone();
        for turn in 0..2 {
            let call = ToolCall {
                call_id: format!("before-revoke-{turn}"),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            };
            previous.tool_executor().invoke(&call).await.unwrap();
            {
                let lifecycle = host
                    .session_slots
                    .read(thread, |slot| slot.lifecycle.clone())
                    .unwrap();
                let _lifecycle = lifecycle.lock().await;
                host.retire_session_environment_for_revocation(thread)
                    .await
                    .unwrap();
            }
            assert!(
                previous.tool_executor().invoke(&call).await.is_err(),
                "W2 old Hand stays closed"
            );
            // W1/W2 new admitted work reinstalls the aggregate-owned Resource
            // transition through the production projection port. Publication
            // must execute its ordinary reconciliation, never a test-only ACK.
            managed
                .realization()
                .ok_or_else(|| {
                    awaken_session_error_contract::RunError::unavailable(
                        "RealizationRuntime is unsupported by this runtime",
                    )
                })
                .expect("runtime role")
                .install_session_projection(
                    thread,
                    projection.clone(),
                    awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
                )
                .await
                .unwrap();
            fake.shared.lock().unwrap().remove("__ready_observation");
            assert!(
                host.adopt_bound_session_environment(
                    thread,
                    Some(&binding),
                    &provider,
                    None,
                    false
                )
                .await
                .is_err(),
                "W1"
            );
            let retained = host
                .session_slots
                .read(thread, |slot| {
                    slot.environment_owner.terminal_bound_environment()
                })
                .flatten()
                .unwrap();
            assert!(
                Arc::ptr_eq(&retained.environment, &previous),
                "W1 exact owner retained"
            );
            fake.shared
                .lock()
                .unwrap()
                .insert("__ready_observation".into(), Vec::new());
            host.adopt_bound_session_environment(thread, Some(&binding), &provider, None, false)
                .await
                .unwrap();
            let rebound = host
                .session_slots
                .read(thread, |slot| {
                    slot.environment_owner.terminal_bound_environment()
                })
                .flatten()
                .unwrap()
                .environment;
            assert!(!Arc::ptr_eq(&previous, &rebound), "W2 fresh wrapper");
            assert!(
                !Arc::ptr_eq(&previous.tool_executor(), &rebound.tool_executor()),
                "W2 fresh Hand"
            );
            let SessionEnvironment::Container { sandbox, .. } = rebound.as_ref() else {
                unreachable!()
            };
            assert!(Arc::ptr_eq(physical, sandbox), "W2 same physical owner");
            assert_eq!(rebound.handle(), handle, "W2 exact durable binding");
            assert_eq!(fake.creates.load(Ordering::SeqCst), 1, "W2 no recreate");
            assert_eq!(
                fake.adopts.load(Ordering::SeqCst),
                0,
                "W2 no reconstructed participants"
            );
            assert_eq!(fake.disposals.load(Ordering::SeqCst), 0, "W2 no disposal");
            rebound
                .tool_executor()
                .invoke(&call)
                .await
                .expect("W2 fresh Hand is usable");
            previous = rebound;
        }
        let mut foreign = handle.clone();
        foreign.sandbox_id.push_str("-foreign");
        assert!(
            provider
                .rebind_retained(
                    &spec,
                    &foreign,
                    &previous,
                    crate::session_environment::ProviderEffect::Unattached
                )
                .await
                .is_err(),
            "W3 foreign handle"
        );
        let directory = tempfile::tempdir().unwrap();
        assert!(
            SessionEnvironmentProvider::workdir(directory.path())
                .rebind_retained(
                    &spec,
                    &handle,
                    &previous,
                    crate::session_environment::ProviderEffect::Unattached
                )
                .await
                .is_err(),
            "W3 foreign provider"
        );
        previous.stop_bound_processes().await.unwrap();
    }
}

/// A checkpoint store that counts reads so a test can prove the selection
/// edge refused before any checkpoint byte was fetched.
#[derive(Default)]
struct CountingCheckpointStore {
    gets: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl pc::SandboxCheckpointStore for CountingCheckpointStore {
    async fn put(
        &self,
        _metadata: &pc::CheckpointObjectMetadata,
        _bytes: Vec<u8>,
    ) -> Result<pc::StoredCheckpointObject, pc::SandboxError> {
        Err(pc::SandboxError::unsupported("counting store never stores"))
    }
    async fn get(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(pc::SandboxError::unsupported("counting store never reads"))
    }
    async fn delete(&self, _id: &str) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

// Container restore capability ladder (ADR-0081): C1 the container provider
// advertises no `restorable` rung. R1 restore is refused as a typed
// Unsupported at selection, so a composition root learns the missing rung
// from the error class; R2 no checkpoint byte is read before that refusal;
// R3 the refusal is the selection edge's own, not a provider default that a
// warm pool or decorator could silently forward.
#[tokio::test]
async fn container_restore_is_refused_at_selection_when_the_provider_has_no_restorable_rung() {
    let provider = Arc::new(FakeContainerProvider::default());
    assert!(provider.restorable().is_none(), "C1");
    let container = SessionEnvironmentProvider::container(
        provider,
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    );
    let store = CountingCheckpointStore::default();
    let request = pc::SandboxRestoreRequest {
        workspace_id: "workspace".into(),
        session_id: "session".into(),
        effect_id: "restore-effect".into(),
        generation_id: "generation".into(),
        checkpoint: pc::SandboxCheckpointRef {
            id: "checkpoint".into(),
            format: "portable-test-v1".into(),
            digest: "digest".into(),
            size_bytes: 7,
            created_at_unix_ms: 10,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: "environment".into(),
            base_image_fingerprint: "base".into(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: "suspend-effect".into(),
        },
    };
    let error = container
        .restore(&spec(), &request, &store)
        .await
        .expect_err("R1 refused");
    assert!(
        matches!(error, pc::SandboxError::Unsupported(_)),
        "R1 typed variant: {error}"
    );
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "R2"
    );
    assert!(
        error.to_string().contains("checkpoint restore"),
        "R3 names the rung: {error}"
    );
}
