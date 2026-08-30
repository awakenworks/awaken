use super::*;
use crate::git_transport::git_stdout;
use awaken_provisioning_contract::{
    IsolationClass, NetworkPolicy, ResourceLimits, Sandbox, SandboxSpec,
};

fn workdir_spec(scope: &str, deny_egress: bool) -> SandboxSpec {
    SandboxSpec {
        scope: scope.into(),
        isolation: IsolationClass::Workdir,
        mounts: Vec::new(),
        env: Vec::new(),
        // The Workdir tier admits only unrestricted network (it cannot enforce
        // isolation); egress denial for the rooted bash tool rides `extra`.
        packages: Default::default(),
        network: NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: ResourceLimits::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: deny_egress,
    }
}

fn git(cwd: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(cwd)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

struct FailedCreationMount {
    teardowns: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    drops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for FailedCreationMount {
    fn drop(&mut self) {
        self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl pc::MemoryMount for FailedCreationMount {
    fn realization(&self) -> pc::Realization {
        pc::Realization::Fuse
    }

    async fn teardown(&self) -> Result<(), pc::SandboxError> {
        self.teardowns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(pc::SandboxError::new("injected teardown failure"))
    }
}

struct FailedCreationMounter {
    mount_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    teardowns: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    drops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl pc::MemoryMounter for FailedCreationMounter {
    async fn mount(
        &self,
        _store_id: &str,
        host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> Result<Box<dyn pc::MemoryMount>, pc::SandboxError> {
        let call = self
            .mount_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call == 1 {
            return Err(pc::SandboxError::new("injected later mount failure"));
        }
        if call != 0 {
            return Err(pc::SandboxError::new(format!(
                "unexpected Memory mount call {call}"
            )));
        }
        std::fs::create_dir_all(host_path)
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        Ok(Box::new(FailedCreationMount {
            teardowns: self.teardowns.clone(),
            drops: self.drops.clone(),
        }))
    }
}

#[tokio::test]
async fn failed_creation_teardown_retains_guards_past_temporary_sandbox_drop() {
    // Ownerless-rollback decision row: C1 Memory mount succeeds; C2 a later
    // frozen mount fails; C3 Memory teardown fails. R1 C1+C2 invokes teardown;
    // R2 C3 transfers the complete guard set out of the temporary LocalSandbox
    // and retains it for process lifetime, so returning Err and dropping that
    // wrapper cannot call MemoryMount::drop on the still-live participant.
    let tmp = tempfile::tempdir().unwrap();
    let mount_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let teardowns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = LocalProvider::new(tmp.path()).with_memory_mounter(std::sync::Arc::new(
        FailedCreationMounter {
            mount_calls: mount_calls.clone(),
            teardowns: teardowns.clone(),
            drops: drops.clone(),
        },
    ));
    let mut spec = workdir_spec("failed-creation-memory-retention", false);
    spec.mounts = vec![
        pc::MountRequirement {
            mount_id: "memory".into(),
            source: pc::MountSource::MemoryStore {
                store_id: "store".into(),
                materialization_reference: None,
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            },
            mount_path: "/mnt/memory/test".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Session,
            required: true,
        },
        pc::MountRequirement {
            mount_id: "later-memory".into(),
            source: pc::MountSource::MemoryStore {
                store_id: "later-store".into(),
                materialization_reference: None,
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            },
            mount_path: "/mnt/memory/later".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Session,
            required: true,
        },
    ];
    let effect = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    assert!(
        provider
            .create_sandbox_for_effect(&spec, &effect, None)
            .await
            .is_err(),
        "R1"
    );
    assert_eq!(
        mount_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "C1+C2"
    );
    assert_eq!(teardowns.load(std::sync::atomic::Ordering::SeqCst), 1, "R1");
    assert_eq!(
        drops.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "R2 temporary Sandbox already left scope"
    );
}

#[tokio::test]
async fn ready_create_replay_projects_the_receipt_without_rematerializing() {
    // Create-replay cause/effect table: C1 marker is Creating/Ready; C2
    // retry effect is exact/drifted; C3 lease is live/expired. R1 Creating
    // requires live authorization, writes once, and publishes its receipt;
    // R2 Ready+exact (either C3) is read-only, returns the same V2 handle,
    // and preserves bytes changed after Ready; R3 drift rejects.
    // The preserved mutation distinguishes receipt projection from a second
    // Inline resolution/write, which would restore `declared`.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let mut spec = workdir_spec("local-ready-receipt", false);
    spec.mounts.push(pc::MountRequirement {
        mount_id: "inline".into(),
        source: pc::MountSource::Inline {
            contents: "declared".into(),
        },
        mount_path: "/receipt.txt".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    let effect = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    let first = provider
        .create_sandbox_for_effect(&spec, &effect, None)
        .await
        .expect("R1");
    let first_handle = pc::Sandbox::handle(&first);
    std::fs::write(first.workspace_path().join("receipt.txt"), b"after-ready").unwrap();

    let replay = provider
        .create_sandbox_for_effect(&spec, &effect, None)
        .await
        .expect("R2");
    assert_eq!(
        pc::Sandbox::handle(&replay),
        first_handle,
        "R2 exact receipt"
    );
    assert_eq!(
        std::fs::read(replay.workspace_path().join("receipt.txt")).unwrap(),
        b"after-ready",
        "R2 zero rematerialization"
    );
    let expired_replay = provider
        .create_sandbox_for_effect(
            &spec,
            &pc::SandboxEffectFence::new("create", "owner", "runtime", 1, 0).unwrap(),
            None,
        )
        .await
        .expect("R2 read-only replay ignores lease expiry");
    assert_eq!(
        pc::Sandbox::handle(&expired_replay),
        first_handle,
        "R2 expired"
    );
    assert!(
        provider
            .create_sandbox_for_effect(
                &spec,
                &pc::SandboxEffectFence::new("drifted", "owner", "runtime", 1, u64::MAX,).unwrap(),
                None,
            )
            .await
            .is_err(),
        "R3"
    );
}

#[tokio::test]
async fn rooted_tools_are_nonempty_and_egress_tracks_the_network_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());

    let open = provider
        .create_sandbox(&workdir_spec("t-open", false))
        .await
        .unwrap();
    assert!(!open.deny_egress);
    // The full built-in capability surface is composed as RawTools.
    assert!(!open.rooted_tools().is_empty());

    let closed = provider
        .create_sandbox(&workdir_spec("t-closed", true))
        .await
        .unwrap();
    assert!(closed.deny_egress);
    // Egress denial changes the tool wrapper, not the tool set's size.
    assert_eq!(open.rooted_tools().len(), closed.rooted_tools().len());
}

#[tokio::test]
async fn list_files_and_scan_skill_dir_read_the_realized_root() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let sandbox = provider
        .create_sandbox(&workdir_spec("t-scan", false))
        .await
        .unwrap();
    let root = sandbox.root.root().to_path_buf();

    // Output artifacts are collected as (logical_path, bytes), sorted, recursively.
    std::fs::create_dir_all(root.join("outputs/sub")).unwrap();
    std::fs::write(root.join("outputs/a.txt"), b"A").unwrap();
    std::fs::write(root.join("outputs/sub/b.txt"), b"B").unwrap();
    assert_eq!(
        sandbox.list_files("outputs").unwrap(),
        vec![
            ("a.txt".to_string(), b"A".to_vec()),
            ("sub/b.txt".to_string(), b"B".to_vec()),
        ]
    );

    // Managed Skill discovery decision table. C1 file is under the exact
    // `.claude/skills` root; C2 it has exactly one Skill directory level;
    // C3 its filename is `SKILL.md`. D1 C1+C2+C3 => discover one logical
    // Skill. D2 wrong root, D3 root-level file, D4 extra nesting, and D5
    // missing canonical filename => ignore. This owns Anthropic's repository
    // discovery shape while the host remains the Skill semantic owner.
    std::fs::create_dir_all(root.join(".claude/skills/greet")).unwrap();
    std::fs::write(root.join(".claude/skills/greet/SKILL.md"), "# greet").unwrap();
    std::fs::write(root.join(".claude/skills/SKILL.md"), "# root").unwrap();
    std::fs::create_dir_all(root.join(".claude/skills/nested/too-deep")).unwrap();
    std::fs::write(
        root.join(".claude/skills/nested/too-deep/SKILL.md"),
        "# nested",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(".claude/skills/missing")).unwrap();
    std::fs::write(root.join(".claude/skills/missing/skill.md"), "# wrong name").unwrap();
    std::fs::create_dir_all(root.join("skills/outside")).unwrap();
    std::fs::write(root.join("skills/outside/SKILL.md"), "# outside").unwrap();

    let skills = sandbox.scan_skill_dir(".claude/skills").unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "greet");
    assert_eq!(skills[0].dir, ".claude/skills/greet");
    assert_eq!(skills[0].content, "# greet");
}

/// Repository realization cause/effect decision table:
/// | Rule | Destination / Agent Git config | Frozen plan | Effect |
/// |---|---|---|---|
/// | R1 | absent | valid | build in a random stage, verify, and atomically publish without replacement |
/// | R2 | already realized | exact replay | succeed without replacing Agent state |
/// | R3 | complete repository | different remote/checkout | reject without modifying the tree |
/// | R3b | incomplete/non-Repository tree | any plan | reject and preserve every byte; reservation is not deletion authority |
/// | R4 | origin and `url.*.insteadOf` target attacker | exact authored remote plus branch/commit | push only the absent exact ref; attacker unchanged |
/// | R5 | R4 after successful push | same plan/coordinate | return the identical canonical receipt |
#[tokio::test]
async fn provision_clones_then_the_host_pushes_the_agents_own_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    // Seed a bare "remote" with one commit.
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    git(&seed, &["checkout", "-q", "-b", "main"]);
    git(&seed, &["config", "user.email", "seed@t"]);
    git(&seed, &["config", "user.name", "seed"]);
    std::fs::write(seed.join("README.md"), "hello").unwrap();
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    let bare = base.join("remote.git");
    git(
        base,
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let attacker = base.join("attacker.git");
    git(
        base,
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            attacker.to_str().unwrap(),
        ],
    );
    let seed_head = std::process::Command::new("git")
        .current_dir(&seed)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let seed_head = String::from_utf8(seed_head.stdout)
        .unwrap()
        .trim()
        .to_owned();

    let provider = LocalProvider::new(base.join("envs"));
    let sandbox = provider
        .create_sandbox(&workdir_spec("t-repo", false))
        .await
        .unwrap();
    let root = sandbox.root.root().to_path_buf();
    let plan = pc::RepositoryRealizationPlan {
        repository_id: "repo-1".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: bare.to_string_lossy().into_owned(),
        transport_url: bare.to_string_lossy().into_owned(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };

    pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None)
        .await
        .unwrap();
    let repo_dir = root.join("workspace/repo");
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("README.md")).unwrap(),
        "hello"
    );
    assert!(
        std::fs::read_dir(root.join("workspace"))
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                !name.starts_with(".awaken-repository-") || !name.ends_with(".stage")
            }),
        "R1 successful publish leaves no stage name"
    );
    std::fs::write(repo_dir.join("PRESERVED"), "agent state").unwrap();
    pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None)
        .await
        .expect("R2 exact replay is idempotent");
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("PRESERVED")).unwrap(),
        "agent state",
        "R2 preserves the existing working tree"
    );
    let conflicting = pc::RepositoryRealizationPlan {
        source_remote_url: base.join("different.git").to_string_lossy().into_owned(),
        transport_url: base.join("different.git").to_string_lossy().into_owned(),
        ..plan.clone()
    };
    assert!(
        pc::RepositoryRealizer::realize_repository(&sandbox, &conflicting, None)
            .await
            .is_err(),
        "R3 conflicting realization fails closed"
    );
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("README.md")).unwrap(),
        "hello",
        "R3 does not replace the authoritative tree"
    );
    let incomplete_dir = root.join("workspace/incomplete");
    std::fs::create_dir(&incomplete_dir).unwrap();
    std::fs::write(incomplete_dir.join("PRESERVED"), "user bytes").unwrap();
    let incomplete = pc::RepositoryRealizationPlan {
        mount_path: "/workspace/incomplete".into(),
        ..plan.clone()
    };
    pc::RepositoryRealizer::realize_repository(&sandbox, &incomplete, None)
        .await
        .expect_err("R3b incomplete destination fails closed");
    assert_eq!(
        std::fs::read_to_string(incomplete_dir.join("PRESERVED")).unwrap(),
        "user bytes",
        "R3b incomplete destination is never deleted"
    );
    // Provision sets NO committer identity — that is the agent's to own.
    let cfg = std::process::Command::new("git")
        .current_dir(&repo_dir)
        .args(["config", "--local", "user.name"])
        .output()
        .unwrap();
    assert!(
        cfg.stdout.is_empty(),
        "provision must not set a committer identity"
    );

    // Nothing authored yet: an explicit exact seed coordinate observes the
    // already-current remote and returns its canonical receipt.
    let initial_branch = git_stdout(Some(&repo_dir), &["symbolic-ref", "--short", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    let seed_expectation = pc::RepositoryPublicationExpectation {
        branch: initial_branch,
        commit: seed_head.clone(),
        expected_prior_commit: None,
    };
    let seed_receipt =
        pc::RepositoryRealizer::publish_repository(&sandbox, &plan, &seed_expectation, None)
            .await
            .unwrap();
    seed_receipt.verify(&plan, &seed_expectation).unwrap();

    // The AGENT configures its own identity and authors a commit in the jail — a clean
    // working tree afterwards (it committed everything), which the OLD harvest would have
    // wrongly skipped. The host then only pushes.
    git(&repo_dir, &["config", "user.email", "hermes@agent.local"]);
    git(&repo_dir, &["config", "user.name", "Hermes"]);
    git(&repo_dir, &["checkout", "-b", "awf/work"]);
    std::fs::write(repo_dir.join("NEW.txt"), "agent").unwrap();
    git(&repo_dir, &["add", "-A"]);
    git(&repo_dir, &["commit", "-q", "-m", "agent: add NEW.txt"]);
    let agent_commit = git_stdout(Some(&repo_dir), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "awf/work".into(),
        commit: agent_commit,
        expected_prior_commit: None,
    };

    // The Agent owns this config and may point both the named remote and an
    // `insteadOf` rewrite at an attacker. Publication must ignore both and
    // consume only the immutable plan URL passed by the host.
    let attacker_url = attacker.to_string_lossy().into_owned();
    let frozen_url = plan.transport_url.clone();
    git(&repo_dir, &["remote", "set-url", "origin", &attacker_url]);
    let rewrite_key = format!("url.{attacker_url}.insteadOf");
    git(&repo_dir, &["config", &rewrite_key, &frozen_url]);

    // R4 pushes the frozen target even though both Agent-authored mechanisms
    // select the attacker. R5 compares the exact target and becomes a no-op.
    let first = pc::RepositoryRealizer::publish_repository(&sandbox, &plan, &expectation, None)
        .await
        .expect("R4");
    let replay = pc::RepositoryRealizer::publish_repository(&sandbox, &plan, &expectation, None)
        .await
        .expect("R5");
    assert_eq!(first, replay, "R4/R5");

    let attacker_head = std::process::Command::new("git")
        .args([
            "--git-dir",
            attacker.to_str().unwrap(),
            "rev-parse",
            "refs/heads/main",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(attacker_head.stdout).unwrap().trim(),
        seed_head,
        "R4 attacker ref must remain unchanged"
    );

    // The bare remote carries the AGENT's commit — its own message and author, not a
    // canned harvest commit by a fake user.
    let log = std::process::Command::new("git")
        .current_dir(&bare)
        .args(["log", "-1", "refs/heads/awf/work", "--pretty=%an|%ae|%s"])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("agent: add NEW.txt"), "agent's message: {log}");
    assert!(log.contains("Hermes"), "agent's author name: {log}");
    assert!(
        log.contains("hermes@agent.local"),
        "agent's author email: {log}"
    );
}

#[tokio::test]
async fn repository_realizer_checks_out_the_exact_commit_pin() {
    // Cause graph: commit checkout in frozen config -> realization plan
    // -> clone tokenless origin -> detached checkout -> exact tree/HEAD.
    //
    // Decision table:
    // | Rule | Branch | Commit | Expected behavior |
    // | G1 | none | valid reachable SHA | detached exact SHA/tree |
    // | G2 | none | invalid SHA | fail realization, no fallback to HEAD |
    let tmp = tempfile::tempdir().unwrap();
    let seed = tmp.path().join("seed-commit");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    git(&seed, &["config", "user.email", "seed@t"]);
    git(&seed, &["config", "user.name", "seed"]);
    std::fs::write(seed.join("VERSION"), "one").unwrap();
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", "one"]);
    let first = std::process::Command::new("git")
        .current_dir(&seed)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let first = String::from_utf8(first.stdout).unwrap().trim().to_string();
    std::fs::write(seed.join("VERSION"), "two").unwrap();
    git(&seed, &["commit", "-q", "-am", "two"]);

    let provider = LocalProvider::new(tmp.path().join("envs"));
    let sandbox = provider
        .create_sandbox(&workdir_spec("exact-commit", false))
        .await
        .unwrap();
    let plan = pc::RepositoryRealizationPlan {
        repository_id: "repo-commit".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: seed.to_string_lossy().into_owned(),
        transport_url: seed.to_string_lossy().into_owned(),
        initial_branch: None,
        initial_commit: Some(first.clone()),
        access: pc::MountAccess::ReadOnly,
    };
    pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None)
        .await
        .unwrap();
    let realized = sandbox.root.root().join("workspace/repo");
    assert_eq!(
        std::fs::read_to_string(realized.join("VERSION")).unwrap(),
        "one"
    );
    let head = std::process::Command::new("git")
        .current_dir(&realized)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), first);

    let invalid = pc::RepositoryRealizationPlan {
        mount_path: "/workspace/invalid".into(),
        initial_commit: Some("0000000000000000000000000000000000000000".into()),
        ..plan
    };
    assert!(
        pc::RepositoryRealizer::realize_repository(&sandbox, &invalid, None)
            .await
            .is_err(),
        "G2 invalid commit fails instead of using the remote default HEAD"
    );
}

#[tokio::test]
async fn repository_realizer_rejects_a_jail_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let sandbox = provider
        .create_sandbox(&workdir_spec("t-escape", false))
        .await
        .unwrap();
    let plan = pc::RepositoryRealizationPlan {
        repository_id: "repo-escape".into(),
        mount_path: "../escape".into(),
        source_remote_url: "http://x".into(),
        transport_url: "http://x".into(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadOnly,
    };
    assert!(
        pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dynamic_inline_projection_handles_file_directory_and_missing_removal() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = LocalProvider::new(tmp.path())
        .create_sandbox(&workdir_spec("inline-lifecycle", false))
        .await
        .unwrap();

    sandbox
        .materialize_inline("nested/value", b"value")
        .unwrap();
    sandbox.remove_inline("nested/value").unwrap();
    sandbox
        .materialize_inline("nested/value", b"value")
        .unwrap();
    sandbox.remove_inline("nested").unwrap();
    sandbox.remove_inline("nested").unwrap();
}

#[derive(Default)]
struct CheckpointStore {
    objects: std::sync::Mutex<HashMap<String, StoredTestCheckpoint>>,
    gets: std::sync::atomic::AtomicUsize,
}

#[derive(Clone)]
struct StoredTestCheckpoint {
    metadata: pc::CheckpointObjectMetadata,
    bytes: Vec<u8>,
    receipt: pc::StoredCheckpointObject,
}

#[async_trait]
impl pc::SandboxCheckpointStore for CheckpointStore {
    async fn put(
        &self,
        metadata: &pc::CheckpointObjectMetadata,
        bytes: Vec<u8>,
    ) -> Result<pc::StoredCheckpointObject, pc::SandboxError> {
        let id = format!(
            "{}/{}/{}/{}",
            metadata.workspace_id,
            metadata.session_id,
            metadata.generation_id,
            metadata.suspend_effect_id
        );
        let digest = content_fingerprint(&bytes);
        let size_bytes = bytes.len() as u64;
        let receipt = pc::StoredCheckpointObject {
            id,
            digest,
            size_bytes,
        };
        let mut objects = self.objects.lock().unwrap();
        match objects.get(&receipt.id) {
            Some(stored) if stored.metadata == *metadata && stored.bytes == bytes => {
                Ok(stored.receipt.clone())
            }
            Some(_) => Err(err(
                "checkpoint logical key is already bound to different metadata or bytes",
            )),
            None => {
                objects.insert(
                    receipt.id.clone(),
                    StoredTestCheckpoint {
                        metadata: metadata.clone(),
                        bytes,
                        receipt: receipt.clone(),
                    },
                );
                Ok(receipt)
            }
        }
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.objects
            .lock()
            .unwrap()
            .get(id)
            .map(|stored| stored.bytes.clone())
            .ok_or_else(|| err("checkpoint object not found"))
    }

    async fn delete(&self, id: &str) -> Result<(), pc::SandboxError> {
        self.objects.lock().unwrap().remove(id);
        Ok(())
    }
}

fn checkpoint_request_for(session_id: &str) -> pc::SandboxCheckpointRequest {
    let generation = awaken_session_contract::SandboxGeneration::new(
        session_id,
        10,
        10_000,
        "environment",
        "base-image",
    );
    pc::SandboxCheckpointRequest {
        workspace_id: "workspace-a".into(),
        session_id: session_id.into(),
        generation_id: generation.id,
        environment_fingerprint: generation.environment_fingerprint,
        base_image_fingerprint: generation.base_image_fingerprint,
        effect_id: "suspend".into(),
        format: "awaken-fs-tar-v1".into(),
        created_at_unix_ms: 20,
        expires_at_unix_ms: 10_000,
        max_bytes: 1024 * 1024,
    }
}

fn checkpoint_request() -> pc::SandboxCheckpointRequest {
    checkpoint_request_for("checkpoint-session")
}

fn exact_restore_request(
    spec: &pc::SandboxSpec,
    checkpoint_request: &pc::SandboxCheckpointRequest,
    checkpoint: &pc::SandboxCheckpointRef,
    operation: &str,
) -> pc::SandboxRestoreRequest {
    pc::SandboxRestoreRequest {
        workspace_id: checkpoint_request.workspace_id.clone(),
        session_id: spec.scope.clone(),
        effect_id: format!("blake3:{}", content_fingerprint(operation.as_bytes())),
        generation_id: checkpoint_request.generation_id.clone(),
        checkpoint: checkpoint.clone(),
    }
}

#[tokio::test]
async fn checkpoint_store_exact_replay_decision_table_is_total() {
    use pc::SandboxCheckpointStore as _;

    // Participant/store cause-effect table: C1 logical key is new/existing;
    // C2 complete metadata is equal/drifted; C3 bytes are equal/drifted; C4
    // delete response is delivered/lost. R1 new binds one receipt; R2 exact
    // replay returns that receipt; R3 either metadata or byte drift rejects
    // without replacing the first object; R4 repeated delete converges.
    let store = CheckpointStore::default();
    let request = checkpoint_request();
    let metadata = pc::CheckpointObjectMetadata {
        workspace_id: request.workspace_id.clone(),
        session_id: request.session_id.clone(),
        generation_id: request.generation_id.clone(),
        suspend_effect_id: request.effect_id.clone(),
        created_at_unix_ms: request.created_at_unix_ms,
        expires_at_unix_ms: request.expires_at_unix_ms,
    };
    let first = store.put(&metadata, b"bytes".to_vec()).await.expect("R1");
    assert_eq!(
        store.put(&metadata, b"bytes".to_vec()).await.unwrap(),
        first,
        "R2"
    );
    let mut drifted = metadata.clone();
    drifted.expires_at_unix_ms -= 1;
    assert!(store.put(&drifted, b"bytes".to_vec()).await.is_err(), "R3");
    assert!(store.put(&metadata, b"other".to_vec()).await.is_err(), "R3");
    assert_eq!(store.get(&first.id).await.unwrap(), b"bytes", "R3");
    store.delete(&first.id).await.expect("R4");
    store.delete(&first.id).await.expect("R4");
}

#[tokio::test]
async fn terminal_memory_reconstruction_accepts_only_exact_copy_evidence() {
    // Cold terminal-Memory preparation/authorization table: C1 frozen mount is
    // copy-default/write-through; C2 current handle evidence is exact/missing/
    // extra/ambiguous; C3 ack uses the exact complete ordered evidence and a
    // live terminal effect / mismatched evidence / stale or foreign effect; C4
    // the aggregate has/has not durably accepted preparation before invoking
    // physical authorization. M1 copy+exact reconstructs one guard-free terminal
    // sandbox and exposes the original bytes/heads to the Host's sole CAS
    // reconciler. M2 invalid C1/C2 fails before marker/root mutation. M3 prepare
    // before exact ack fails closed; invalid ack has zero physical effect; exact
    // ack and prepare replay safely while the root remains present. C5 the C
    // preparation response is delivered/lost and a same-operation renewal D
    // retries before/after the C root CAS. M4 both CAS orders return the same
    // durable provider predecessor C; shorter/foreign input has zero marker
    // mutation. M5 only C4 permits the separate C->D physical-disposal port,
    // which removes the exact root without repeating Memory reconciliation.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let mut spec = workdir_spec("terminal-memory-copy", false);
    spec.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let create =
        pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX - 2).unwrap();
    let terminal =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX - 1).unwrap();
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec);
    let root = crate::sandbox_dir(tmp.path(), &spec.scope);
    let mut creation =
        crate::realization_marker::begin(&root, &fingerprint, &create, None, None).unwrap();
    creation.prepare_root().unwrap();
    std::fs::create_dir(root.join("memory")).unwrap();
    std::fs::write(root.join("memory/value"), b"candidate").unwrap();
    let evidence = pc::MemoryMaterializationEvidence::new(
        "store",
        "/memory",
        vec![pc::MemoryMaterializationHead {
            path: "value".into(),
            id: "head".into(),
            content_sha256: "digest".into(),
        }],
    )
    .unwrap();
    let completion = crate::realization_marker::RealizationCompletionReceipt::new(
        &[pc::RealizedMount {
            mount_id: "memory".into(),
            mount_path: "/memory".into(),
            access: pc::MountAccess::ReadWrite,
            realization: pc::Realization::Copy,
            content_hash: None,
        }],
        vec![evidence.clone()],
    )
    .unwrap();
    let realization = creation.complete(&completion).unwrap();
    let payload = pc::LocalSandboxHandleV2 {
        previous: pc::LocalSandboxHandleV1 {
            outputs_path: spec.outputs_path.clone(),
            base_env: spec.env.clone(),
            continuation_excluded_paths: Vec::new(),
            deny_tool_egress: false,
        },
        realization_fingerprint: fingerprint,
        effect_fence: create.clone(),
        physical_incarnation: realization.physical_incarnation().to_owned(),
        owned_paths: vec!["/memory".into()],
    };
    let missing = pc::SandboxHandle::local_v2(spec.scope.clone(), payload.clone());
    assert!(
        provider
            .prepare_terminal_sandbox_for_effect(&spec, Some(&missing), None, &terminal,)
            .await
            .is_err(),
        "M2 missing evidence"
    );
    assert_eq!(
        pc::SandboxProvider::observe(&provider, &missing)
            .await
            .unwrap(),
        pc::SandboxObservation::Ready,
        "M2 zero marker/root mutation"
    );

    let handle = pc::SandboxHandle::local_v2(spec.scope.clone(), payload)
        .with_memory_materializations(vec![evidence.clone()])
        .unwrap();
    let cold = provider
        .prepare_terminal_sandbox_for_effect(&spec, Some(&handle), None, &terminal)
        .await
        .unwrap()
        .expect("M1");
    assert!(cold.memory_mounts.lock().await.is_empty(), "M1/M3");
    assert_eq!(
        cold.list_files("/memory").unwrap(),
        vec![("value".into(), b"candidate".to_vec())],
        "M1 Host reconciliation reads the exact surviving copy"
    );
    assert_eq!(
        pc::Sandbox::handle(&cold)
            .memory_materializations()
            .unwrap()
            .unwrap(),
        std::slice::from_ref(&evidence),
        "M1"
    );

    assert!(
        cold.prepare_disposal_for_effect(&terminal).await.is_err(),
        "M3 provider preparation is fenced until Host reconciliation"
    );
    assert!(root.is_dir(), "M3 failed removal retains exact root");

    // Model Host CAS failure: the wrapper can vanish, but the marker and
    // root must remain retryable.
    drop(cold);
    assert!(root.is_dir(), "M3 root retained");
    let replay = provider
        .prepare_terminal_sandbox_for_effect(&spec, Some(&handle), None, &terminal)
        .await
        .unwrap()
        .expect("M3 exact replay");
    let stale = pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, 0).unwrap();
    let successor =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX).unwrap();
    let foreign =
        pc::SandboxEffectFence::new("foreign", "other-owner", "runtime", 1, u64::MAX).unwrap();
    let mismatched = pc::MemoryMaterializationEvidence::new(
        "store",
        "/memory",
        vec![pc::MemoryMaterializationHead {
            path: "value".into(),
            id: "head".into(),
            content_sha256: "different".into(),
        }],
    )
    .unwrap();
    assert!(
        pc::Sandbox::acknowledge_memory_reconciliation(&replay, &terminal, &[mismatched],)
            .await
            .is_err(),
        "M3 mismatched evidence"
    );
    assert!(
        pc::Sandbox::acknowledge_memory_reconciliation(
            &replay,
            &stale,
            std::slice::from_ref(&evidence),
        )
        .await
        .is_err(),
        "M3 stale fence"
    );
    assert!(root.is_dir(), "M3 invalid ack has zero physical effect");
    pc::Sandbox::acknowledge_memory_reconciliation(
        &replay,
        &terminal,
        std::slice::from_ref(&evidence),
    )
    .await
    .expect("M1 exact ack");
    pc::Sandbox::acknowledge_memory_reconciliation(
        &replay,
        &terminal,
        std::slice::from_ref(&evidence),
    )
    .await
    .expect("M1 response-loss replay");
    let prepared = replay
        .prepare_disposal_for_effect(&terminal)
        .await
        .expect("M3 exact preparation");
    let replayed = replay
        .prepare_disposal_for_effect(&terminal)
        .await
        .expect("M3 preparation response-loss replay");
    assert_eq!(prepared, terminal, "M3 exact durable predecessor");
    assert_eq!(replayed, terminal, "M3 response-loss predecessor");
    assert!(root.is_dir(), "M3 preparation has zero physical effect");
    pc::Sandbox::acknowledge_memory_reconciliation(
        &replay,
        &successor,
        std::slice::from_ref(&evidence),
    )
    .await
    .expect("M4 same-lease successor");
    assert!(
        pc::Sandbox::acknowledge_memory_reconciliation(
            &replay,
            &foreign,
            std::slice::from_ref(&evidence),
        )
        .await
        .is_err(),
        "M4 foreign lease"
    );
    assert!(root.is_dir(), "M4 rejected effects are non-destructive");
    let renewed_replay = replay
        .prepare_disposal_for_effect(&successor)
        .await
        .expect("M4 successor preparation");
    assert_eq!(renewed_replay, terminal, "M4 C/D CAS orders share C");
    assert!(
        replay.prepare_disposal_for_effect(&stale).await.is_err(),
        "M4 shorter retry"
    );
    assert!(
        replay.prepare_disposal_for_effect(&foreign).await.is_err(),
        "M4 foreign retry"
    );
    assert!(root.is_dir(), "M4 preparation cannot dispose the root");
    let disposal = crate::test_disposal_authorization_for_current(&terminal, &successor);
    replay
        .dispose_for_effect(&disposal)
        .await
        .expect("M5 aggregate-authorized physical disposal");

    let mut write_through = spec.clone();
    let pc::MountSource::MemoryStore {
        write_consistency, ..
    } = &mut write_through.mounts[0].source
    else {
        unreachable!()
    };
    *write_consistency = pc::MemoryWriteConsistency::WriteThroughRequired;
    assert!(
        crate::terminal_copy_materializations(&write_through, Some(&handle)).is_err(),
        "M2 write-through"
    );
    let mut ambiguous = spec.clone();
    ambiguous.mounts.push(ambiguous.mounts[0].clone());
    assert!(
        crate::terminal_copy_materializations(&ambiguous, Some(&handle)).is_err(),
        "M2 ambiguous"
    );
    let no_memory = workdir_spec("no-memory", false);
    assert!(
        crate::terminal_copy_materializations(&no_memory, Some(&handle)).is_err(),
        "M2 extra"
    );
}

#[tokio::test]
async fn terminal_checkpoint_cleanup_uses_the_hot_ready_guard() {
    // Hot/cold Upload terminal decision table: C1 resident is live
    // Ready/prepared Removing; C2 Suspend request and fence are exact/drifted/
    // absent through the legacy checkpoint port;
    // C3 terminal authorization is live/expired; C4 crash cut is before
    // pending/after pending/after put/after receipt/after delete response.
    // Rule H (this test): Ready + exact + live enters ReadyOperationGuard,
    // keeps the resident (and its Memory guards) live, and replays the one
    // checkpoint WAL through delete response loss. Rule C is exercised by
    // the following test through RemovalGuard. Both entrances share
    // CheckpointParticipantGuard transitions; drift/expiry yields zero next
    // mutation and no lifecycle phase is invented by checkpoint cleanup.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("hot-terminal-checkpoint", false);
    let create = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    let suspend = pc::SandboxEffectFence::new("suspend", "owner", "runtime", 1, u64::MAX).unwrap();
    let terminal =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX).unwrap();
    let sandbox = provider
        .create_sandbox_for_effect(&spec, &create, None)
        .await
        .unwrap();
    std::fs::write(sandbox.workspace_path().join("mutable"), b"state").unwrap();
    let store = CheckpointStore::default();
    let request = checkpoint_request();

    assert!(
        pc::Sandbox::checkpoint(&sandbox, &request, &store)
            .await
            .is_err(),
        "Rule H current realization rejects an unfenced upload"
    );
    sandbox
        .cleanup_checkpoint_for_terminal(&request, &store, &suspend, &terminal)
        .await
        .expect("Rule H");
    assert!(store.objects.lock().unwrap().is_empty(), "Rule H delete");
    assert_eq!(
        pc::Sandbox::status(&sandbox).await.unwrap(),
        pc::SandboxStatus::Ready,
        "Rule H keeps the live resident Ready until dispose"
    );
    sandbox
        .cleanup_checkpoint_for_terminal(&request, &store, &suspend, &terminal)
        .await
        .expect("Rule H delete response-loss replay");
    sandbox
        .prepare_disposal_for_effect(&terminal)
        .await
        .expect("Rule H source-dependent preparation has zero physical effect");
    sandbox
        .dispose_for_effect(&crate::test_disposal_authorization(&terminal))
        .await
        .expect("Rule H finish");
}

#[tokio::test]
async fn terminal_checkpoint_cleanup_uses_the_prepared_removal_guard() {
    // Rule C of the hot/cold table above: prepare has already moved
    // Ready->Removing, so exact replay uses that same held RemovalGuard for
    // snapshot->WAL->put->receipt->delete. Repeated cleanup after delete
    // response loss reuses the receipt; no Ready lock is reacquired. A
    // drifted Suspend identity or expired terminal authorization fails
    // before its next mutation and retains the guard/root for retry.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("terminal-checkpoint", false);
    let create = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    let suspend = pc::SandboxEffectFence::new("suspend", "owner", "runtime", 1, u64::MAX).unwrap();
    let terminal =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX).unwrap();
    let sandbox = provider
        .create_sandbox_for_effect(&spec, &create, None)
        .await
        .unwrap();
    std::fs::write(sandbox.workspace_path().join("mutable"), b"state").unwrap();
    let handle = pc::Sandbox::handle(&sandbox);
    let terminal_sandbox = provider
        .prepare_terminal_sandbox_for_effect(&spec, Some(&handle), None, &terminal)
        .await
        .unwrap()
        .expect("R1 Removing participant");
    let store = CheckpointStore::default();
    let request = checkpoint_request();
    let wrong_suspend =
        pc::SandboxEffectFence::new("other-suspend", "owner", "runtime", 1, u64::MAX).unwrap();
    assert!(
        terminal_sandbox
            .cleanup_checkpoint_for_terminal(&request, &store, &wrong_suspend, &terminal,)
            .await
            .is_err(),
        "R4 input drift"
    );
    let expired_terminal =
        pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, 0).unwrap();
    assert!(
        terminal_sandbox
            .cleanup_checkpoint_for_terminal(&request, &store, &suspend, &expired_terminal,)
            .await
            .is_err(),
        "R4 expired terminal"
    );
    terminal_sandbox
        .cleanup_checkpoint_for_terminal(&request, &store, &suspend, &terminal)
        .await
        .expect("R1");
    terminal_sandbox
        .cleanup_checkpoint_for_terminal(&request, &store, &suspend, &terminal)
        .await
        .expect("R2");
    assert!(store.objects.lock().unwrap().is_empty(), "R2");
    terminal_sandbox
        .prepare_disposal_for_effect(&terminal)
        .await
        .expect("R1 durable preparation precedes physical disposal");
    terminal_sandbox
        .dispose_for_effect(&crate::test_disposal_authorization(&terminal))
        .await
        .expect("R1 finish");
}

// Exact-restore fidelity table: C1=mutable nested file plus file/directory
// modes; C2=checkpoint bytes are durable; C3=source disposal completed through
// its existing owner; C4=the aggregate projects one exact restore request.
// R1 C1+C2+C3+C4 => the completed result carries request-bound evidence and its
// distinct provider target contains identical bytes and permission metadata.
// R2 exact target disposal, including response-loss replay, removes only that
// target. Directory modes are restored children-first so read-only parents
// cannot block extraction.
#[tokio::test]
async fn checkpoint_dispose_restore_preserves_mutable_filesystem() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("checkpoint-session", false);
    let sandbox = provider.create_sandbox(&spec).await.unwrap();
    let file = sandbox.workspace_path().join("workspace/bin/tool");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"mutable state").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::set_permissions(
        file.parent().unwrap(),
        std::fs::Permissions::from_mode(0o500),
    )
    .unwrap();
    let store = CheckpointStore::default();
    let checkpoint_request = checkpoint_request_for(&spec.scope);
    let receipt = sandbox
        .checkpoint(&checkpoint_request, &store)
        .await
        .unwrap();
    sandbox.dispose().await.unwrap();
    assert!(!sandbox.workspace_path().exists(), "source terminated");

    let request = exact_restore_request(&spec, &checkpoint_request, &receipt, "fidelity");
    let restored = pc::SandboxProvider::restore(&provider, &spec, &request, &store)
        .await
        .unwrap();
    let expected_evidence = request.evidence(&spec);
    assert_eq!(restored.evidence(), &expected_evidence, "R1 exact evidence");
    request
        .verify_handle(&spec, &pc::Sandbox::handle(restored.target().as_ref()))
        .expect("R1 completed target carries the exact durable evidence");
    let restored_root = restore_target::restoration_root(tmp.path(), &expected_evidence).unwrap();
    let restored_file = restored_root.join("workspace/bin/tool");
    assert_eq!(
        std::fs::read(&restored_file).unwrap(),
        b"mutable state",
        "R1 bytes"
    );
    assert_eq!(
        std::fs::metadata(&restored_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    assert_eq!(
        std::fs::metadata(restored_root.join("workspace/bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o500,
        "directory mode is applied after child extraction"
    );
    drop(restored);
    pc::SandboxProvider::dispose_restored(&provider, &spec, &request)
        .await
        .expect("R2 exact disposal");
    pc::SandboxProvider::dispose_restored(&provider, &spec, &request)
        .await
        .expect("R2 disposal response-loss replay");
    assert!(!restored_root.exists(), "R2 only the exact target is gone");
}

#[tokio::test]
async fn exact_restore_target_recovery_and_disposal_are_request_bound() {
    // Exact-target cause/effect table. C1 the source reaches durable disposal
    // preparation P before physical Disposal; C2 the exact target is absent or
    // already bound; C3 the provider process wrapper is original/fresh; C4 a
    // disposal request is exact/foreign; C5 request admission is valid/drifted.
    // T1 C1+C2(absent)+C5(valid) => Created without reading checkpoint bytes.
    // T2 C2(bound)+C3(fresh) => Recovered with the identical handle and no byte
    // read. T3 C4(foreign) leaves the exact target intact; T4 C5(drifted) fails
    // before I/O; T5 C4(exact) removes only that target and absent replay
    // succeeds. This replaces the removed marker/fence restore path rather than
    // preserving it as a compatibility track.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("removed-checkpoint-restore", false);
    let create = pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
    let suspend = pc::SandboxEffectFence::new("suspend", "owner", "runtime", 2, u64::MAX).unwrap();
    let terminal =
        pc::SandboxEffectFence::new("hibernate-terminal", "owner", "runtime", 3, u64::MAX).unwrap();
    let source = provider
        .create_sandbox_for_effect(&spec, &create, None)
        .await
        .unwrap();
    std::fs::write(source.workspace_path().join("value"), b"checkpoint").unwrap();
    let source_handle = pc::Sandbox::handle(&source);
    let store = CheckpointStore::default();
    let checkpoint_request = checkpoint_request_for(&spec.scope);
    let checkpoint = source
        .checkpoint_for_effect(&checkpoint_request, &store, &suspend)
        .await
        .expect("T1 checkpoint participant");
    drop(source);
    let terminal_sandbox = provider
        .prepare_terminal_sandbox_for_effect(&spec, Some(&source_handle), None, &terminal)
        .await
        .expect("T1 terminal prepare")
        .expect("T1 exact terminal participant");
    terminal_sandbox
        .prepare_disposal_for_effect(&terminal)
        .await
        .expect("T1 durable P precedes physical Disposal");
    terminal_sandbox
        .dispose_for_effect(&crate::test_disposal_authorization(&terminal))
        .await
        .expect("T1 physical Disposal");
    assert!(
        !crate::sandbox_dir(tmp.path(), "removed-checkpoint-restore").exists(),
        "T1 disposed source remains absent"
    );
    let request = exact_restore_request(&spec, &checkpoint_request, &checkpoint, "recovery");
    let evidence = request.evidence(&spec);
    let exact_root = restore_target::restoration_root(tmp.path(), &evidence).unwrap();
    let created = pc::SandboxProvider::acquire_restore(&provider, &spec, &request)
        .await
        .expect("T1 exact target creation");
    assert_eq!(
        created.disposition(),
        pc::SandboxRestoreTargetDisposition::Created,
        "T1"
    );
    let created_handle = pc::Sandbox::handle(created.target().as_ref());
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "T1 acquisition never reads checkpoint bytes"
    );
    drop(created);

    let fresh_provider = LocalProvider::new(tmp.path());
    let recovered = pc::SandboxProvider::acquire_restore(&fresh_provider, &spec, &request)
        .await
        .expect("T2 fresh provider recovery");
    assert_eq!(
        recovered.disposition(),
        pc::SandboxRestoreTargetDisposition::Recovered,
        "T2"
    );
    assert_eq!(
        pc::Sandbox::handle(recovered.target().as_ref()),
        created_handle,
        "T2 identical exact handle"
    );
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "T2 recovery remains byte-free"
    );

    let foreign = exact_restore_request(&spec, &checkpoint_request, &checkpoint, "foreign");
    pc::SandboxProvider::dispose_restored(&fresh_provider, &spec, &foreign)
        .await
        .expect("T3 foreign target is provably absent");
    assert!(exact_root.is_dir(), "T3 exact target remains");

    let mut drifted = request.clone();
    drifted.session_id = "different-session".into();
    assert!(
        pc::SandboxProvider::acquire_restore(&fresh_provider, &spec, &drifted)
            .await
            .is_err(),
        "T4 session drift fails admission"
    );
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "T4 zero store I/O"
    );
    assert!(exact_root.is_dir(), "T4 exact target remains");

    drop(recovered);
    pc::SandboxProvider::dispose_restored(&fresh_provider, &spec, &request)
        .await
        .expect("T5 exact disposal");
    pc::SandboxProvider::dispose_restored(&fresh_provider, &spec, &request)
        .await
        .expect("T5 disposal response-loss replay");
    assert!(!exact_root.exists(), "T5 exact target removed");
}

#[tokio::test]
async fn exact_restore_materialization_response_loss_is_idempotent() {
    // Restore-materialization table. C1 the exact target is absent/recovered;
    // C2 the completed provider response is delivered/lost before aggregate
    // publication; C3 request admission is exact/malformed. M1 exact first
    // execution materializes checkpoint bytes and returns request-bound
    // evidence. M2 response-loss replay recovers the same unpublished target,
    // revalidates/re-reads the durable object, and converges its checkpoint
    // entries to the same bytes and handle. M3 malformed identity rejects before
    // target/store I/O and preserves M1/M2. No post-publication Agent mutation is
    // permitted in the M2 window, so this test deliberately models only an
    // unpublished crash residue.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("fenced-restore", false);
    let source = provider.create_sandbox(&spec).await.unwrap();
    std::fs::write(source.workspace_path().join("value"), b"checkpoint").unwrap();
    let store = CheckpointStore::default();
    let checkpoint_request = checkpoint_request_for(&spec.scope);
    let receipt = source
        .checkpoint(&checkpoint_request, &store)
        .await
        .unwrap();
    source.dispose().await.unwrap();
    let request = exact_restore_request(&spec, &checkpoint_request, &receipt, "response-loss");
    let expected_evidence = request.evidence(&spec);
    let restored_root = restore_target::restoration_root(tmp.path(), &expected_evidence).unwrap();
    let restored = pc::SandboxProvider::restore(&provider, &spec, &request, &store)
        .await
        .expect("M1");
    assert_eq!(restored.evidence(), &expected_evidence, "M1 exact evidence");
    let first_handle = pc::Sandbox::handle(restored.target().as_ref());
    let gets_after_first = store.gets.load(std::sync::atomic::Ordering::Relaxed);
    drop(restored);

    std::fs::write(restored_root.join("value"), b"unpublished-crash-residue").unwrap();
    let replay = pc::SandboxProvider::restore(&provider, &spec, &request, &store)
        .await
        .expect("M2");
    assert_eq!(
        pc::Sandbox::handle(replay.target().as_ref()),
        first_handle,
        "M2 identical exact handle"
    );
    assert_eq!(replay.evidence(), &expected_evidence, "M2 exact evidence");
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        gets_after_first + 1,
        "M2 revalidates durable bytes before completion"
    );
    assert_eq!(
        std::fs::read(restored_root.join("value")).unwrap(),
        b"checkpoint",
        "M2 converges partial unpublished bytes"
    );

    let mut malformed = request.clone();
    malformed.effect_id = "blake3:00".into();
    let gets_before_reject = store.gets.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        pc::SandboxProvider::restore(&provider, &spec, &malformed, &store)
            .await
            .is_err(),
        "M3 malformed identity"
    );
    assert_eq!(
        store.gets.load(std::sync::atomic::Ordering::Relaxed),
        gets_before_reject,
        "M3 zero store I/O"
    );
    assert_eq!(
        std::fs::read(restored_root.join("value")).unwrap(),
        b"checkpoint",
        "M3 zero target write"
    );
    drop(replay);
    pc::SandboxProvider::dispose_restored(&provider, &spec, &request)
        .await
        .expect("M1/M2 exact cleanup");
    pc::SandboxProvider::dispose_restored(&provider, &spec, &request)
        .await
        .expect("M1/M2 cleanup response-loss replay");
}

// Corruption/recovery table. C1 the exact target is absent/recovered; C2 object
// bytes match/differ from the committed size or digest. F1 C2(different)
// reserves at most the exact provider target but returns no completed Sandbox
// and performs no checkpoint entry writes. F2 fixing the same durable object
// then replaying the same request recovers that target and completes. F3 exact
// disposal removes the incomplete/completed target and response-loss replay is
// an idempotent success. The durable checkpoint reference remains store-owned.
#[tokio::test]
async fn corrupt_checkpoint_is_rejected_before_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let spec = workdir_spec("checkpoint-session", false);
    let sandbox = provider.create_sandbox(&spec).await.unwrap();
    std::fs::write(sandbox.workspace_path().join("value"), b"original").unwrap();
    let store = CheckpointStore::default();
    let checkpoint_request = checkpoint_request_for(&spec.scope);
    let receipt = sandbox
        .checkpoint(&checkpoint_request, &store)
        .await
        .unwrap();
    sandbox.dispose().await.unwrap();
    let original = {
        let mut objects = store.objects.lock().unwrap();
        let stored = objects.get_mut(&receipt.id).unwrap();
        let original = stored.bytes.clone();
        stored.bytes = b"corrupt".to_vec();
        original
    };
    let request = exact_restore_request(&spec, &checkpoint_request, &receipt, "corruption");
    let evidence = request.evidence(&spec);
    let restored_root = restore_target::restoration_root(tmp.path(), &evidence).unwrap();
    assert!(
        pc::SandboxProvider::restore(&provider, &spec, &request, &store)
            .await
            .is_err(),
        "F1 corruption fails closed"
    );
    assert!(restored_root.is_dir(), "F1 exact target is retryable");
    assert!(
        !restored_root.join("value").exists(),
        "F1 no checkpoint entry write"
    );

    store
        .objects
        .lock()
        .unwrap()
        .get_mut(&receipt.id)
        .unwrap()
        .bytes = original;
    let recovered = pc::SandboxProvider::restore(&provider, &spec, &request, &store)
        .await
        .expect("F2 exact retry");
    assert_eq!(recovered.evidence(), &evidence, "F2 exact evidence");
    assert_eq!(
        std::fs::read(restored_root.join("value")).unwrap(),
        b"original",
        "F2 materialized bytes"
    );
    drop(recovered);
    pc::SandboxProvider::dispose_restored(&provider, &spec, &request)
        .await
        .expect("F3 exact disposal");
    pc::SandboxProvider::dispose_restored(&provider, &spec, &request)
        .await
        .expect("F3 disposal response-loss replay");
}
