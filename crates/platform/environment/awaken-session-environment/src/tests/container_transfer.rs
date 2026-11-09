//! Container repository/file transfer, hand rebinding, and restore selection.

use super::*;

#[cfg(feature = "docker-tests")]
// Container Repository transfer rules: C1 a non-default Agent work branch
// has one committed change -> publish that exact branch without changing
// the base branch; C2 replay the unchanged container state -> no second
// push; C3 outputs and authored Skills remain independently harvestable.
#[tokio::test]
#[ignore = "real production Sandbox image supplied by the required substrate gate"]
async fn docker_anonymous_repository_and_artifact_readback_use_one_sandbox() {
    // R0 missing image => qualification fails before physical effects. The
    // ordinary developer profile reports this row as ignored, never as a pass.
    // R1 both anonymous bare origin and worktree live in the admitted Sandbox:
    // every seed/clone/publish/readback command uses its one process port. This
    // row proves real anonymous Git/filesystem effects. Its name states that
    // consumer boundary because the former token claim supplied no credential.
    let image = std::env::var("AWAKEN_TEST_SESSION_IMAGE")
        .expect("required Docker Session qualification needs its current Sandbox image");
    let clock = awaken_time::system_clock();

    let runtime =
        Arc::new(awaken_sandbox_container::docker::DockerRuntime::connect_local(8080).unwrap());
    let provider = Arc::new(
        awaken_sandbox_container::ContainerProvider::new(runtime, image).with_clock(clock.clone()),
    );
    let mut docker_spec = spec();
    docker_spec.scope = format!("host-repo-real-{}", std::process::id());
    docker_spec.outputs_path = "/mnt/session/outputs".into();
    let environment = SandboxEnvironmentProvider::container_with_hand_idle_and_residency(
        provider,
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::ZERO,
        awaken_session_environment_contract::ContainerHandResidency::AttachedExec,
        clock,
    )
    .into_provider_roles()
    .realizer()
    .create_unattached(&docker_spec)
    .await
    .unwrap();
    let remote = "/workspace/remote.git";
    let mut seed = pc::Command::new([
        "sh",
        "-c",
        concat!(
            "set -eu; git init -q --bare --initial-branch=main /workspace/remote.git; ",
            "git init -q --initial-branch=main /workspace/seed; ",
            "git -C /workspace/seed config user.name seed; ",
            "git -C /workspace/seed config user.email seed@example.invalid; ",
            "printf base > /workspace/seed/README.md; ",
            "git -C /workspace/seed add README.md; ",
            "git -C /workspace/seed commit -qm base; ",
            "git -C /workspace/seed push -q /workspace/remote.git HEAD 2>/dev/null; ",
            "git --git-dir=/workspace/remote.git rev-parse refs/heads/main"
        ),
    ]);
    seed.stdio = pc::Stdio::Piped;
    let (seed_process, mut seed_channel) =
        environment.agent_process().spawn_agent(seed).await.unwrap();
    let mut base_commit = String::new();
    seed_channel.read_to_string(&mut base_commit).await.unwrap();
    assert_eq!(seed_process.wait().await.unwrap().code, Some(0), "R1 seed");
    let base_commit = base_commit.trim();
    assert_eq!(base_commit.len(), 40, "R1 exact seeded base commit");
    let repository_plan = pc::RepositoryRealizationPlan {
        repository_id: "repo".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: remote.into(),
        transport_url: remote.into(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };
    pc::RepositoryRealizer::realize_repository(
        environment.repositories().as_ref(),
        &repository_plan,
        None,
        awaken_provisioning_contract::RepositoryRealizationIntent::Initialize,
    )
    .await
    .unwrap();
    environment
        .agent_process()
        .materialize_inline("/workspace/.mnt/live.txt", b"live")
        .await
        .unwrap();
    assert_eq!(
        environment
            .sandbox_environment()
            .list_workspace_files(".mnt")
            .await
            .unwrap(),
        vec![("live.txt".into(), b"live".to_vec())]
    );
    environment
        .projection()
        .remove_projection_path(".mnt/live.txt")
        .await
        .unwrap();
    assert!(
        environment
            .sandbox_environment()
            .list_workspace_files(".mnt")
            .await
            .unwrap()
            .is_empty()
    );

    let change = environment
        .sandbox_environment()
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
                "mkdir -p -- \"$1/nested\" && ",
                "printf '\\000\\377' > \"$1/nested/result.bin\" && ",
                "mkdir -p /workspace/skills/authored && ",
                "printf '%s' '---\ndescription: authored\n---\nbody' > ",
                "/workspace/skills/authored/SKILL.md"
            ),
            "fixture-write",
            docker_spec.outputs_path.as_str(),
        ]))
        .await
        .unwrap();
    assert_eq!(change.wait().await.unwrap().code, Some(0));
    environment.skills().register_skill_dir("skills");
    environment.skills().refresh_skills().await.unwrap();
    let skills = environment.skills().scan_skill_dir("skills").unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "authored");
    let commit = environment
        .sandbox_environment()
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
        .projection()
        .remove_projection_path(".mnt/commit.txt")
        .await
        .unwrap();
    let first = pc::RepositoryRealizer::publish_repository(
        environment.repositories().as_ref(),
        &repository_plan,
        &expectation,
        None,
    )
    .await
    .unwrap();
    let replay = pc::RepositoryRealizer::publish_repository(
        environment.repositories().as_ref(),
        &repository_plan,
        &expectation,
        None,
    )
    .await
    .unwrap();
    assert_eq!(first, replay, "C1/C2 canonical receipt");
    // Artifact cause/effect rule: one regular output file → one canonical
    // content-addressed Artifact and binary-safe read through the same port.
    let artifacts = environment.harvest().capture_artifacts().await.unwrap();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].metadata.path.ends_with("nested/result.bin"));
    assert_eq!(artifacts[0].metadata.id, artifacts[0].metadata.content_hash);
    assert_eq!(artifacts[0].bytes, vec![0, 0xff]);
    let mut observe = pc::Command::new([
        "sh",
        "-c",
        concat!(
            "set -eu; git --git-dir=/workspace/remote.git show-ref --verify refs/heads/awf/work >/dev/null; ",
            "git --git-dir=/workspace/remote.git rev-parse refs/heads/main; ",
            "git --git-dir=/workspace/remote.git rev-parse refs/heads/awf/work; ",
            "git --git-dir=/workspace/remote.git rev-list --count --all"
        ),
    ]);
    observe.stdio = pc::Stdio::Piped;
    let (process, mut channel) = environment
        .agent_process()
        .spawn_agent(observe)
        .await
        .unwrap();
    let mut observation = String::new();
    channel.read_to_string(&mut observation).await.unwrap();
    assert_eq!(
        process.wait().await.unwrap().code,
        Some(0),
        "C1/C2 remote readback"
    );
    assert_eq!(
        observation.lines().collect::<Vec<_>>(),
        [base_commit, expectation.commit.as_str(), "2"],
        "C1 base unchanged and exact work branch; C2 replay adds no commit"
    );
    environment.lifecycle().dispose().await.unwrap();
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
    assert!(provider.restoration().restorable().is_err(), "C1");
    let container = SandboxEnvironmentProvider::container(
        provider,
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    )
    .into_provider_roles();
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
        .terminal()
        .restore(&spec(), &request, &store, pc::SandboxEffect::Unattached)
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
