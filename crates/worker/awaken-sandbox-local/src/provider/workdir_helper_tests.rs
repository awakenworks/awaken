
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
        sandbox.list_files("outputs"),
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

    let skills = sandbox.scan_skill_dir(".claude/skills");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "greet");
    assert_eq!(skills[0].dir, ".claude/skills/greet");
    assert_eq!(skills[0].content, "# greet");
}

/// Repository realization cause/effect decision table:
/// | Rule | Destination / Agent Git config | Frozen plan | Effect |
/// |---|---|---|---|
/// | R1 | absent | valid | clone the exact repository |
/// | R2 | already realized | exact replay | succeed without replacing Agent state |
/// | R3 | occupied | different remote/checkout | reject without modifying the tree |
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
        mount_path: "workspace/repo".into(),
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
        mount_path: "workspace/repo".into(),
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
        mount_path: "workspace/invalid".into(),
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

fn restore_request(excluded_mounts: Vec<String>) -> pc::SandboxRestoreRequest {
    pc::SandboxRestoreRequest {
        workspace_id: "workspace-a".into(),
        session_id: "checkpoint-session".into(),
        effect_id: "blake3:0000000000000000000000000000000000000000000000000000000000000001".into(),
        generation_id: "generation-a".into(),
        checkpoint: pc::SandboxCheckpointRef {
            id: "checkpoint-a".into(),
            format: "portable-provider-owned-format".into(),
            digest: "sha256:checkpoint-a".into(),
            size_bytes: 7,
            created_at_unix_ms: 20,
            expires_at_unix_ms: 10_000,
            environment_fingerprint: "environment".into(),
            base_image_fingerprint: "base-image".into(),
            excluded_mounts,
            suspend_effect_id: "suspend".into(),
        },
    }
}

/*
 * Exact local-target decision table (R1-R4, R7-R8).
 * Causes: C1 no target; C2 exact evidence exists; C3 fresh provider; C4
 * concurrent acquisition; C5 source checkpoint unavailable; C6 provider
 * exclusion superset; C7 restored handle carries deny-tool-egress.
 * Effects: E1 reserve one directory/evidence sidecar; E2 return one handle;
 * E3 recover without checkpoint-byte access or process memory; E4 preserve
 * the exact exclusion list and security posture.
 * Rules: R1=C1=>E1; R2=C2=>E2; R3=C2+C3+C5=>E2+E3;
 * R4=C1+C4=>E1+E2; R7=C6+C7=>E4.
 */
#[tokio::test]
async fn exact_restore_target_replays_across_concurrent_and_fresh_local_providers() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = workdir_spec("checkpoint-session", true);
    spec.mounts.push(pc::MountRequirement {
        mount_id: "input-a".into(),
        source: pc::MountSource::Inline {
            contents: "authority-owned".into(),
        },
        mount_path: "/workspace/input-a".into(),
        access: pc::MountAccess::ReadWrite,
        required: true,
        lifetime: pc::MountLifetime::Session,
    });
    let exclusions = vec!["/runtime/config-home".into(), "/workspace/input-a".into()];
    let request = restore_request(exclusions.clone());
    let left_provider = LocalProvider::new(tmp.path());
    let right_provider = LocalProvider::new(tmp.path());

    let (left, right) = tokio::join!(
        pc::SandboxProvider::acquire_restore(&left_provider, &spec, &request),
        pc::SandboxProvider::acquire_restore(&right_provider, &spec, &request),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(left.disposition(), right.disposition(), "R1/R4");
    let left_handle = left.target().handle();
    let right_handle = right.target().handle();
    assert_eq!(left_handle, right_handle, "R2/E2");
    assert_eq!(
        left_handle
            .local_payload()
            .unwrap()
            .continuation_excluded_paths,
        exclusions,
        "R7/E4"
    );
    assert!(
        left_handle.local_payload().unwrap().deny_tool_egress,
        "R7/E4"
    );

    let evidence = request.evidence(&spec);
    let root = restore_target::restoration_root(tmp.path(), &evidence).unwrap();
    assert!(root.is_dir(), "R1/E1");
    let fresh_provider = LocalProvider::new(tmp.path());
    let replay = pc::SandboxProvider::acquire_restore(&fresh_provider, &spec, &request)
        .await
        .unwrap();
    assert_eq!(
        replay.disposition(),
        pc::SandboxRestoreTargetDisposition::Recovered,
        "R3/E3"
    );
    assert_eq!(replay.target().handle(), left_handle, "R3/E2");
    assert_eq!(
        fresh_provider
            .adopt_sandbox_with_spec(&spec, &left_handle)
            .await
            .unwrap()
            .handle(),
        left_handle,
        "R3/E2"
    );

    drop(left);
    drop(right);
    drop(replay);
    pc::SandboxProvider::dispose_restored(&fresh_provider, &spec, &request)
        .await
        .unwrap();
    assert!(!root.exists(), "R8 exact disposal");
}

/*
 * Local mismatch/orphan rules (R5-R6). C1 target evidence differs; C2 an
 * unfenced directory occupies the deterministic target; C3 exact evidence
 * points at a symlink instead of a physical provider directory. E1 reject
 * without replacing/deleting either object. R5=C1=>E1; R6=C2|C3=>E1.
 */
#[tokio::test]
async fn exact_restore_target_rejects_mismatched_evidence_and_unfenced_orphan() {
    let tmp = tempfile::tempdir().unwrap();
    let spec = workdir_spec("checkpoint-session", false);
    let provider = LocalProvider::new(tmp.path());
    let request = restore_request(Vec::new());
    let target = pc::SandboxProvider::acquire_restore(&provider, &spec, &request)
        .await
        .unwrap();
    let evidence = request.evidence(&spec);
    let root = restore_target::restoration_root(tmp.path(), &evidence).unwrap();
    let file_name = root.file_name().unwrap().to_string_lossy();
    let sidecar = root
        .parent()
        .unwrap()
        .join(format!(".{file_name}.awaken-restore-target.json"));
    let mut other = request.clone();
    other.generation_id = "generation-b".into();
    std::fs::write(
        &sidecar,
        serde_json::to_vec(&serde_json::json!({
            "evidence": other.evidence(&spec)
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(
        pc::SandboxProvider::acquire_restore(&provider, &spec, &request)
            .await
            .is_err(),
        "R5/E1"
    );
    assert!(root.exists(), "R5/E1 preserve target");

    drop(target);
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::remove_file(&sidecar).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    assert!(
        pc::SandboxProvider::acquire_restore(&provider, &spec, &request)
            .await
            .is_err(),
        "R6/E1"
    );
    assert!(root.exists(), "R6/E1 preserve orphan");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        std::fs::remove_dir_all(&root).unwrap();
        let outside = tmp.path().join("outside-provider-target");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), b"outside").unwrap();
        symlink(&outside, &root).unwrap();
        std::fs::write(
            &sidecar,
            serde_json::to_vec(&serde_json::json!({ "evidence": evidence })).unwrap(),
        )
        .unwrap();
        assert!(
            pc::SandboxProvider::acquire_restore(&provider, &spec, &request)
                .await
                .is_err(),
            "R6/C3/E1"
        );
        assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"outside");
    }
}
