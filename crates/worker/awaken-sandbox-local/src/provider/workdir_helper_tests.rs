use super::*;
use crate::git_transport::git_stdout;
use awaken_provisioning_contract::{
    IsolationClass, NetworkPolicy, ResourceLimits, Sandbox, SandboxSpec,
};
use std::future::IntoFuture;

pub(crate) fn workdir_spec(scope: &str, deny_egress: bool) -> SandboxSpec {
    SandboxSpec {
        process_tree_supervision: false,
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
async fn trusted_host_realizes_readonly_resource_as_advisory_access() {
    // Cause/effect graph: C1 provider={ordinary Workdir,trusted host}; C2 one
    // read-only managed File mount. E1 ordinary Workdir rejects an unenforceable
    // guarantee; E2 trusted host realizes the bytes while retaining the typed
    // read-only intent. The two rows exhaust C1 for fixed C2.
    let tmp = tempfile::tempdir().unwrap();
    let mut ordinary_spec = workdir_spec("ordinary-readonly", false);
    ordinary_spec.mounts.push(pc::MountRequirement {
        mount_id: "input".into(),
        source: pc::MountSource::Inline {
            contents: "managed resource".into(),
        },
        mount_path: "/workspace/input.txt".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    });
    assert!(
        LocalProvider::new(tmp.path().join("ordinary"))
            .create_sandbox(&ordinary_spec)
            .await
            .is_err(),
        "E1"
    );

    let mut trusted_spec = ordinary_spec;
    trusted_spec.scope = "trusted-readonly".into();
    let sandbox = LocalProvider::trusted_host(tmp.path().join("trusted"))
        .create_sandbox(&trusted_spec)
        .await
        .expect("E2 trusted host accepts advisory read-only access");
    assert_eq!(
        std::fs::read_to_string(sandbox.root.root().join("workspace/input.txt")).unwrap(),
        "managed resource",
        "E2"
    );
    assert_eq!(sandbox.realized()[0].access, pc::MountAccess::ReadOnly);
    // L1 ordinary + late RO => reject before bytes; L2 trusted + late RO =>
    // copied bytes; L3 a create-time participant or its ancestor => reject.
    // This extends the same trust table across create and live attach.
    let ordinary = LocalProvider::new(tmp.path().join("ordinary-late"))
        .create_sandbox(&workdir_spec("ordinary-late", false))
        .await
        .unwrap();
    let mut late = trusted_spec.mounts[0].clone();
    late.mount_id = "late".into();
    late.mount_path = "/workspace/late.txt".into();
    assert!(ordinary.attach(late.clone()).await.is_err(), "L1");
    assert!(!ordinary.root.root().join("workspace/late.txt").exists());
    assert_eq!(
        sandbox.attach(late.clone()).await.unwrap().realization,
        pc::Realization::Copy,
        "L2"
    );
    for path in [
        "/workspace/input.txt",
        "/workspace",
        "/workspace/input.txt/child",
    ] {
        late.mount_path = path.into();
        assert!(sandbox.attach(late.clone()).await.is_err(), "L3 {path}");
    }
    assert_eq!(
        std::fs::read(sandbox.root.root().join("workspace/input.txt")).unwrap(),
        b"managed resource"
    );
}

#[tokio::test]
async fn creation_resource_responses_require_current_authority_before_writing() {
    // R1-R60: Local/Namespace x File present/File optional absent/Secret x
    // past/future Control timestamps x revoke/replace/exhaust/renew/cancel.
    // Entry denial -> no root or resolution. Loss during actual Pending ->
    // neither returned bytes nor an optional empty file nor Ready is published.
    // Renewal -> success. Exact admitted retry writes once; Ready replay does
    // not resolve again. Synthetic credentials only, not local credential E2E.
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    struct PausedSource {
        calls: AtomicUsize,
        gate: tokio::sync::Semaphore,
        absent: bool,
    }
    impl PausedSource {
        async fn resolve(&self) -> Option<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _permit =
                tokio::time::timeout(std::time::Duration::from_secs(30), self.gate.acquire())
                    .await
                    .expect("timed out waiting for self.gate.acquire()")
                    .unwrap();
            (!self.absent).then(|| b"synthetic-resource".to_vec())
        }
    }
    #[async_trait]
    impl pc::BlobSource for PausedSource {
        async fn get(&self, _: &str) -> Option<Vec<u8>> {
            self.resolve().await
        }
    }
    #[async_trait]
    impl pc::SecretBroker for PausedSource {
        async fn materialize(&self, _: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Ok(self.resolve().await.unwrap())
        }
        async fn materialize_process(&self, _: &str) -> Result<Vec<u8>, pc::SandboxError> {
            unreachable!("file-only fixture")
        }
        async fn write_back(
            &self,
            _target: pc::SecretWritebackTarget<'_>,
            _: Vec<u8>,
        ) -> Result<(), pc::SandboxError> {
            unreachable!("read-only fixture")
        }
    }
    for namespace in [false, true] {
        for kind in ["file", "absent", "secret"] {
            for expiry in [100, u64::MAX - 1] {
                for change in ["revoke", "replace", "exhaust", "renew", "cancel"] {
                    let tmp = tempfile::tempdir().unwrap();
                    let source = Arc::new(PausedSource {
                        calls: AtomicUsize::new(0),
                        gate: tokio::sync::Semaphore::new(0),
                        absent: kind == "absent",
                    });
                    let provider: Box<dyn pc::SandboxProvider> = if namespace {
                        Box::new(
                            crate::NamespaceProvider::new(tmp.path())
                                .with_blob_source(source.clone())
                                .with_secret_broker(source.clone()),
                        )
                    } else {
                        Box::new(
                            LocalProvider::trusted_host(tmp.path())
                                .with_blob_source(source.clone())
                                .with_secret_broker(source.clone()),
                        )
                    };
                    let mut spec = workdir_spec("resource-proof", false);
                    if namespace {
                        spec.isolation = pc::IsolationClass::Namespace;
                    }
                    spec.mounts.push(pc::MountRequirement {
                        mount_id: "resource".into(),
                        mount_path: "/workspace/resource".into(),
                        source: if kind == "secret" {
                            pc::MountSource::Secret {
                                reference: "synthetic".into(),
                                content_hash: None,
                            }
                        } else {
                            pc::MountSource::File {
                                file_id: "synthetic".into(),
                                content_hash: None,
                            }
                        },
                        access: pc::MountAccess::ReadOnly,
                        lifetime: pc::MountLifetime::Session,
                        required: kind != "absent",
                    });
                    let fence =
                        pc::SandboxEffectFence::new("create", "owner", "boot", 1, expiry).unwrap();
                    let authority = pc::test_support::EffectAuthority::new(&fence);
                    let root = tmp.path().join("resource-proof");
                    assert!(
                        provider
                            .claim_for_effect(
                                &spec,
                                &fence,
                                &pc::test_support::EffectAuthority::denied()
                            )
                            .await
                            .is_err()
                    );
                    assert!(!root.exists());
                    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
                    let mut pending = Box::pin(
                        provider
                            .claim_for_effect(&spec, &fence, &authority)
                            .into_future(),
                    );
                    std::future::poll_fn(|cx| {
                        assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
                    match change {
                        "revoke" => *authority.current.lock().unwrap() = None,
                        "replace" => {
                            authority.current.lock().unwrap().as_mut().unwrap().0.epoch += 1
                        }
                        "exhaust" => {
                            authority.current.lock().unwrap().as_mut().unwrap().1 = Duration::ZERO
                        }
                        "renew" => {
                            authority
                                .current
                                .lock()
                                .unwrap()
                                .as_mut()
                                .unwrap()
                                .0
                                .expires_at_unix_ms += 1
                        }
                        _ => {}
                    }
                    if change == "cancel" {
                        drop(pending);
                    } else {
                        source.gate.add_permits(1);
                        assert_eq!(
                            pending.await.is_ok(),
                            change == "renew",
                            "{namespace}/{kind}/{expiry}/{change}"
                        );
                    }
                    let file = root.join("workspace/resource");
                    assert_eq!(
                        file.exists(),
                        change == "renew",
                        "response is not mutation authority"
                    );
                    *authority.current.lock().unwrap() =
                        Some((fence.clone(), Duration::from_secs(1)));
                    if change == "cancel" {
                        source.gate.add_permits(1);
                    }
                    let replay = provider
                        .claim_for_effect(&spec, &fence, &authority)
                        .await
                        .unwrap();
                    assert_eq!(
                        std::fs::read(&file).unwrap(),
                        if kind == "absent" {
                            b"".as_slice()
                        } else {
                            b"synthetic-resource".as_slice()
                        }
                    );
                    assert_eq!(
                        source.calls.load(Ordering::SeqCst),
                        if change == "renew" { 1 } else { 2 }
                    );
                    assert_eq!(
                        provider
                            .claim_for_effect(&spec, &fence, &authority)
                            .await
                            .unwrap()
                            .handle(),
                        replay.handle()
                    );
                    assert_eq!(
                        source.calls.load(Ordering::SeqCst),
                        if change == "renew" { 1 } else { 2 }
                    );
                }
            }
        }
    }
}

pub(crate) struct TeardownProbe {
    pub(crate) teardowns: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) drops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) failures: usize,
    pub(crate) wait_until_ms: Option<u64>,
    pub(crate) response_gate: Option<std::sync::Arc<tokio::sync::Semaphore>>,
}

impl Drop for TeardownProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl pc::MemoryMount for TeardownProbe {
    fn realization(&self) -> pc::Realization {
        pc::Realization::Fuse
    }

    async fn teardown(&self) -> Result<(), pc::SandboxError> {
        let call = self
            .teardowns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(deadline) = self.wait_until_ms {
            wait_until_wall_expiry(deadline).await;
        }
        if let Some(gate) = &self.response_gate {
            let _permit = gate.acquire().await.unwrap();
        }
        if call < self.failures {
            Err(pc::SandboxError::backend(
                "teardown",
                "injected teardown failure",
            ))
        } else {
            Ok(())
        }
    }
}

struct FailedCreationMounter {
    mount_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    teardowns: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    drops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    failures: usize,
}

#[async_trait::async_trait]
impl pc::MemoryMounter for FailedCreationMounter {
    async fn mount(
        &self,
        _store_id: &str,
        host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> pc::MemoryMountOutcome {
        let result: Result<Box<dyn pc::MemoryMount>, pc::SandboxError> = (|| {
            let call = self
                .mount_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 1 {
                return Err(pc::SandboxError::backend(
                    "mount",
                    "injected later mount failure",
                ));
            }
            if call != 0 {
                return Err(pc::SandboxError::backend(
                    "mount",
                    format!("unexpected Memory mount call {call}"),
                ));
            }
            std::fs::create_dir_all(host_path)
                .map_err(|error| pc::SandboxError::backend("mount", error))?;
            Ok(Box::new(TeardownProbe {
                teardowns: self.teardowns.clone(),
                drops: self.drops.clone(),
                failures: self.failures,
                wait_until_ms: None,
                response_gate: None,
            }) as Box<dyn pc::MemoryMount>)
        })();
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
}

#[tokio::test]
async fn failed_creation_teardown_retains_guards_past_temporary_sandbox_drop() {
    // Public creation compensation table: Local/Namespace x current/legacy x
    // teardown succeeds/fails. All eight rows use the same creation call path.
    // Success drops the guard and removes the exact root; failure retains both.
    // Ownerless-rollback decision row: C1 Memory mount succeeds; C2 a later
    // frozen mount fails; C3 Memory teardown fails. R1 C1+C2 invokes teardown;
    // R2 C3 keeps the complete guard set in the live Provider's root owner,
    // so returning Err and dropping the temporary
    // wrapper cannot call MemoryMount::drop on the still-live participant.
    for namespace in [false, true] {
        for current in [false, true] {
            for failures in [0, 1] {
                let tmp = tempfile::tempdir().unwrap();
                let mount_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let teardowns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mounter = std::sync::Arc::new(FailedCreationMounter {
                    mount_calls: mount_calls.clone(),
                    teardowns: teardowns.clone(),
                    drops: drops.clone(),
                    failures,
                });
                let provider: Box<dyn pc::SandboxProvider> = if namespace {
                    Box::new(crate::NamespaceProvider::new(tmp.path()).with_memory_mounter(mounter))
                } else {
                    Box::new(LocalProvider::new(tmp.path()).with_memory_mounter(mounter))
                };
                let mut spec = workdir_spec("failed-creation-memory-retention", false);
                if namespace {
                    spec.isolation = pc::IsolationClass::Namespace;
                }
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
                let effect =
                    pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
                let result = if current {
                    provider
                        .claim_for_effect(
                            &spec,
                            &effect,
                            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                                &effect,
                            ),
                        )
                        .await
                } else {
                    let __fx = pc::test_support::fixture_effect("fixture-create");
                    provider.claim_for_effect(&spec, &__fx.0, &__fx.1).await
                };
                let cause = result.err().expect("R1 later mount fails");
                assert!(cause.to_string().contains("injected later mount failure"));
                assert_eq!(
                    crate::sandbox_dir(tmp.path(), &spec.scope).exists(),
                    failures != 0,
                    "R2 only successful compensation removes the root"
                );
                assert_eq!(
                    mount_calls.load(std::sync::atomic::Ordering::SeqCst),
                    2,
                    "C1+C2"
                );
                assert_eq!(teardowns.load(std::sync::atomic::Ordering::SeqCst), 1, "R1");
                assert_eq!(
                    drops.load(std::sync::atomic::Ordering::SeqCst),
                    usize::from(failures == 0),
                    "R2 temporary Sandbox already left scope"
                );
            }
        }
    }
}

#[tokio::test]
async fn ready_create_replay_projects_the_receipt_without_rematerializing() {
    // Create-replay cause/effect table: C1 marker is Creating/Ready; C2
    // retry effect is exact/drifted; C3 current authority is admitted/denied;
    // C4 assertion timestamp is original/historical. R1 Creating requires
    // current authorization, writes once, and publishes its receipt.
    // R2 Ready+exact+admitted (either C4) returns the same V2 handle and
    // preserves Agent bytes; R3 drift or denied current authority rejects.
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
        .create_sandbox_for_effect(
            &spec,
            &effect,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&effect),
        )
        .await
        .expect("R1");
    let first_handle = pc::Sandbox::handle(&first);
    std::fs::write(first.workspace_path().join("receipt.txt"), b"after-ready").unwrap();

    let replay = provider
        .create_sandbox_for_effect(
            &spec,
            &effect,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&effect),
        )
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
            &pc::test_support::EffectAuthority::new(&effect),
        )
        .await
        .expect("R2 historical assertion with current proof");
    assert_eq!(
        pc::Sandbox::handle(&expired_replay),
        first_handle,
        "R2 expired"
    );
    assert!(
        provider
            .create_sandbox_for_effect(
                &spec,
                &effect,
                None,
                &pc::test_support::EffectAuthority::denied(),
            )
            .await
            .is_err(),
        "R3 receipt does not replace current authority"
    );
    assert!(
        provider
            .create_sandbox_for_effect(
                &spec,
                &pc::SandboxEffectFence::new("drifted", "owner", "runtime", 1, u64::MAX,).unwrap(),
                None,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &pc::SandboxEffectFence::new("drifted", "owner", "runtime", 1, u64::MAX,)
                        .unwrap()
                )
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
pub(super) struct CheckpointStore {
    pub(super) objects: std::sync::Mutex<HashMap<String, StoredTestCheckpoint>>,
    pub(super) gets: std::sync::atomic::AtomicUsize,
    pub(super) puts: std::sync::atomic::AtomicUsize,
    pub(super) deletes: std::sync::atomic::AtomicUsize,
    pub(super) pause: std::sync::Mutex<Option<(CheckpointPause, Arc<tokio::sync::Notify>)>>,
    pub(super) paused: tokio::sync::Notify,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckpointPause {
    AfterPut,
    AfterDelete,
}

impl CheckpointStore {
    async fn pause_at(&self, at: CheckpointPause) {
        let gate = self
            .pause
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(selected, _)| *selected == at)
            .map(|(_, gate)| gate.clone());
        if let Some(gate) = gate {
            self.paused.notify_one();
            gate.notified().await;
        }
    }
}

#[derive(Clone)]
pub(super) struct StoredTestCheckpoint {
    metadata: pc::CheckpointObjectMetadata,
    pub(super) bytes: Vec<u8>,
    receipt: pc::StoredCheckpointObject,
}

#[async_trait]
impl pc::SandboxCheckpointStore for CheckpointStore {
    async fn put(
        &self,
        metadata: &pc::CheckpointObjectMetadata,
        bytes: Vec<u8>,
    ) -> Result<pc::StoredCheckpointObject, pc::SandboxError> {
        self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        let result = {
            let mut objects = self.objects.lock().unwrap();
            match objects.get(&receipt.id) {
                Some(stored) if stored.metadata == *metadata && stored.bytes == bytes => {
                    Ok(stored.receipt.clone())
                }
                Some(_) => Err(pc::SandboxError::conflict(
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
        };
        self.pause_at(CheckpointPause::AfterPut).await;
        result
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.objects
            .lock()
            .unwrap()
            .get(id)
            .map(|stored| stored.bytes.clone())
            .ok_or_else(|| pc::SandboxError::not_found("checkpoint object not found"))
    }

    async fn delete(&self, id: &str) -> Result<(), pc::SandboxError> {
        self.deletes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.objects.lock().unwrap().remove(id);
        self.pause_at(CheckpointPause::AfterDelete).await;
        Ok(())
    }
}

pub(super) fn checkpoint_request_for(session_id: &str) -> pc::SandboxCheckpointRequest {
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
    // which removes the exact root without repeating Memory reconciliation. C6
    // the durable payload contains non-default base env, egress denial, and a
    // continuation exclusion. M6 cold reconstruction re-emits that exact handle;
    // omission or spec-derived drift in any payload field is a test failure.
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let mut spec = workdir_spec("terminal-memory-copy", true);
    spec.env.push(pc::EnvVar {
        name: "TERMINAL_RECOVERY".into(),
        value: pc::EnvValue::Inline {
            value: "exact".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });
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
    creation
        .recorded_admission(|held, admitted| held.prepare_root(admitted))
        .unwrap();
    std::fs::create_dir(root.join("memory")).unwrap();
    std::fs::write(root.join("memory/value"), b"candidate").unwrap();
    let evidence = pc::MemoryMaterializationEvidence::new(
        "store",
        "/memory",
        vec![pc::MemoryMaterializationHead {
            path: "value".into(),
            id: "head".into(),
            version: 1,
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
    let realization = creation
        .recorded_admission(|held, admitted| held.complete(&completion, admitted))
        .unwrap();
    let payload = crate::LocalSandboxHandleV2 {
        previous: crate::LocalSandboxHandleV1 {
            outputs_path: spec.outputs_path.clone(),
            base_env: spec.env.clone(),
            continuation_excluded_paths: vec!["memory".into()],
            deny_tool_egress: true,
        },
        realization_fingerprint: fingerprint,
        effect_fence: create.clone(),
        physical_incarnation: realization.physical_incarnation().to_owned(),
        owned_paths: vec!["/memory".into()],
    };
    let missing = pc::SandboxHandle::local_v2(spec.scope.clone(), payload.clone());
    assert!(
        provider
            .prepare_terminal_sandbox_for_effect(
                &spec,
                Some(&missing),
                None,
                &terminal,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal)
            )
            .await
            .is_err(),
        "M2 missing evidence"
    );
    assert_eq!(
        pc::SandboxProvider::observe(&provider, &missing, pc::SandboxObservationBasis::Unattached)
            .await
            .unwrap(),
        pc::SandboxObservation::Ready,
        "M2 zero marker/root mutation"
    );

    let handle = pc::SandboxHandle::local_v2(spec.scope.clone(), payload)
        .with_memory_materializations(vec![evidence.clone()])
        .unwrap();
    let cold = provider
        .prepare_terminal_sandbox_for_effect(
            &spec,
            Some(&handle),
            None,
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
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
    assert_eq!(pc::Sandbox::handle(&cold), handle, "C6/M6 exact handle");

    assert!(
        cold.prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal)
        )
        .await
        .is_err(),
        "M3 provider preparation is fenced until Host reconciliation"
    );
    assert!(root.is_dir(), "M3 failed removal retains exact root");

    // Model Host CAS failure: the wrapper can vanish, but the marker and
    // root must remain retryable.
    drop(cold);
    assert!(root.is_dir(), "M3 root retained");
    let replay = provider
        .prepare_terminal_sandbox_for_effect(
            &spec,
            Some(&handle),
            None,
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
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
            version: 1,
            content_sha256: "different".into(),
        }],
    )
    .unwrap();
    assert!(
        pc::Sandbox::acknowledge_memory_reconciliation(
            &replay,
            &terminal,
            &[mismatched],
            &pc::test_support::EffectAuthority::new(&terminal)
        )
        .await
        .is_err(),
        "M3 mismatched evidence"
    );
    assert!(
        pc::Sandbox::acknowledge_memory_reconciliation(
            &replay,
            &stale,
            std::slice::from_ref(&evidence),
            &pc::test_support::EffectAuthority::new(&stale),
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
        &pc::test_support::EffectAuthority::new(&terminal),
    )
    .await
    .expect("M1 exact ack");
    pc::Sandbox::acknowledge_memory_reconciliation(
        &replay,
        &terminal,
        std::slice::from_ref(&evidence),
        &pc::test_support::EffectAuthority::new(&terminal),
    )
    .await
    .expect("M1 response-loss replay");
    let prepared = replay
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("M3 exact preparation");
    let replayed = replay
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("M3 preparation response-loss replay");
    assert_eq!(prepared, terminal, "M3 exact durable predecessor");
    assert_eq!(replayed, terminal, "M3 response-loss predecessor");
    assert!(root.is_dir(), "M3 preparation has zero physical effect");
    pc::Sandbox::acknowledge_memory_reconciliation(
        &replay,
        &successor,
        std::slice::from_ref(&evidence),
        &pc::test_support::EffectAuthority::new(&successor),
    )
    .await
    .expect("M4 same-lease successor");
    assert!(
        pc::Sandbox::acknowledge_memory_reconciliation(
            &replay,
            &foreign,
            std::slice::from_ref(&evidence),
            &pc::test_support::EffectAuthority::new(&foreign),
        )
        .await
        .is_err(),
        "M4 foreign lease"
    );
    assert!(root.is_dir(), "M4 rejected effects are non-destructive");
    let renewed_replay = replay
        .prepare_disposal_for_effect(
            &successor,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&successor),
        )
        .await
        .expect("M4 successor preparation");
    assert_eq!(renewed_replay, terminal, "M4 C/D CAS orders share C");
    assert!(
        replay
            .prepare_disposal_for_effect(
                &stale,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&stale)
            )
            .await
            .is_err(),
        "M4 shorter retry"
    );
    assert!(
        replay
            .prepare_disposal_for_effect(
                &foreign,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&foreign)
            )
            .await
            .is_err(),
        "M4 foreign retry"
    );
    assert!(root.is_dir(), "M4 preparation cannot dispose the root");
    let disposal = crate::test_disposal_authorization_for_current(&terminal, &successor);
    replay
        .dispose_for_effect(
            &disposal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal.effect_fence(),
            ),
        )
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

pub(super) fn wall_time_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

pub(super) async fn wait_until_wall_expiry(deadline: u64) {
    // Exercise the provider's existing real wall-clock gate, not Tokio's virtual
    // timer. Timeout fails the test if this host clock regresses/stalls.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let remaining = deadline.saturating_sub(wall_time_ms());
            if remaining == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(remaining.min(100))).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn terminal_checkpoint_cancellation_reuses_the_original_guard_and_upload_journal() {
    // C1 resident={Ready,Removing}; C2 paused after={put,delete} committed but
    // response not delivered; C3 second prepare/dispose attempts; C4 caller
    // cancelled. Four rows C1 x C2: C3 cannot cross the held slot; C4 retains
    // Removing guard (or Ready WAL); exact retry reuses the original journal,
    // removes the object and allows ordinary authorized physical disposal.
    for prepared in [false, true] {
        for at in [CheckpointPause::AfterPut, CheckpointPause::AfterDelete] {
            let tmp = tempfile::tempdir().unwrap();
            let provider = LocalProvider::new(tmp.path());
            let spec = workdir_spec("cancelled-terminal-checkpoint", false);
            let create =
                pc::SandboxEffectFence::new("create", "owner", "runtime", 1, u64::MAX).unwrap();
            let terminal =
                pc::SandboxEffectFence::new("terminal", "owner", "runtime", 1, u64::MAX).unwrap();
            let suspend =
                pc::SandboxEffectFence::new("suspend", "owner", "runtime", 1, u64::MAX).unwrap();
            let sandbox = Arc::new(
                provider
                    .create_sandbox_for_effect(
                        &spec,
                        &create,
                        None,
                        &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
                    )
                    .await
                    .unwrap(),
            );
            if prepared {
                sandbox
                    .prepare_disposal_for_effect(
                        &terminal,
                        &awaken_provisioning_contract::test_support::EffectAuthority::new(
                            &terminal,
                        ),
                    )
                    .await
                    .unwrap();
            }
            let store = Arc::new(CheckpointStore::default());
            *store.pause.lock().unwrap() = Some((at, Arc::new(tokio::sync::Notify::new())));
            let first = tokio::spawn({
                let sandbox = sandbox.clone();
                let store = store.clone();
                let terminal = terminal.clone();
                let suspend = suspend.clone();
                async move {
                    sandbox
                        .cleanup_checkpoint_for_terminal(
                            &checkpoint_request(),
                            store.as_ref(),
                            &suspend,
                            &terminal,
                            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                                &terminal,
                            ),
                        )
                        .await
                }
            });
            store.paused.notified().await;
            assert!(
                sandbox
                    .prepare_disposal_for_effect(
                        &terminal,
                        &awaken_provisioning_contract::test_support::EffectAuthority::new(
                            &terminal
                        )
                    )
                    .await
                    .is_err(),
                "C3 preparation is busy"
            );
            let authorization = crate::test_disposal_authorization(&terminal);
            let disposal_authority =
                awaken_provisioning_contract::test_support::EffectAuthority::new(
                    authorization.effect_fence(),
                );
            let mut disposal =
                Box::pin(sandbox.dispose_for_effect(&authorization, &disposal_authority));
            tokio::select! {
                biased;
                result = &mut disposal => panic!("C3 concurrent cleanup escaped slot: {result:?}"),
                () = std::future::ready(()) => {}
            }
            drop(disposal);
            first.abort();
            assert!(first.await.unwrap_err().is_cancelled());
            assert_eq!(
                sandbox.terminal_removal.lock().await.is_some(),
                prepared,
                "C4 preserves resident state"
            );
            *store.pause.lock().unwrap() = None;
            sandbox
                .cleanup_checkpoint_for_terminal(
                    &checkpoint_request(),
                    store.as_ref(),
                    &suspend,
                    &terminal,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
                )
                .await
                .unwrap();
            assert!(store.objects.lock().unwrap().is_empty());
            sandbox
                .prepare_disposal_for_effect(
                    &terminal,
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
                )
                .await
                .unwrap();
            sandbox
                .dispose_for_effect(
                    &crate::test_disposal_authorization(&terminal),
                    &awaken_provisioning_contract::test_support::EffectAuthority::new(
                        crate::test_disposal_authorization(&terminal).effect_fence(),
                    ),
                )
                .await
                .unwrap();
        }
    }
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
        .create_sandbox_for_effect(
            &spec,
            &create,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
        )
        .await
        .unwrap();
    std::fs::write(sandbox.workspace_path().join("mutable"), b"state").unwrap();
    let store = CheckpointStore::default();
    let request = checkpoint_request();

    assert!(
        sandbox.create_checkpoint(&request, &store).await.is_err(),
        "Rule H current realization rejects an unfenced upload"
    );
    sandbox
        .cleanup_checkpoint_for_terminal(
            &request,
            &store,
            &suspend,
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("Rule H");
    assert!(store.objects.lock().unwrap().is_empty(), "Rule H delete");
    assert_eq!(
        pc::Sandbox::status(&sandbox).await.unwrap(),
        pc::SandboxStatus::Ready,
        "Rule H keeps the live resident Ready until dispose"
    );
    sandbox
        .cleanup_checkpoint_for_terminal(
            &request,
            &store,
            &suspend,
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("Rule H delete response-loss replay");
    sandbox
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("Rule H source-dependent preparation has zero physical effect");
    sandbox
        .dispose_for_effect(
            &crate::test_disposal_authorization(&terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                crate::test_disposal_authorization(&terminal).effect_fence(),
            ),
        )
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
        .create_sandbox_for_effect(
            &spec,
            &create,
            None,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&create),
        )
        .await
        .unwrap();
    std::fs::write(sandbox.workspace_path().join("mutable"), b"state").unwrap();
    let handle = pc::Sandbox::handle(&sandbox);
    let terminal_sandbox = provider
        .prepare_terminal_sandbox_for_effect(
            &spec,
            Some(&handle),
            None,
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .unwrap()
        .expect("R1 Removing participant");
    let store = CheckpointStore::default();
    let request = checkpoint_request();
    let wrong_suspend =
        pc::SandboxEffectFence::new("other-suspend", "owner", "runtime", 1, u64::MAX).unwrap();
    assert!(
        terminal_sandbox
            .cleanup_checkpoint_for_terminal(
                &request,
                &store,
                &wrong_suspend,
                &terminal,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal)
            )
            .await
            .is_err(),
        "R4 input drift"
    );
    // Exhaust current proof while keeping successor order valid; an old
    // regressing timestamp would test identity rejection instead of liveness.
    let exhausted = awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal);
    exhausted.current.lock().unwrap().as_mut().unwrap().1 = std::time::Duration::ZERO;
    assert!(
        terminal_sandbox
            .cleanup_checkpoint_for_terminal(&request, &store, &suspend, &terminal, &exhausted)
            .await
            .is_err(),
        "R4 exhausted terminal authority"
    );
    terminal_sandbox
        .cleanup_checkpoint_for_terminal(
            &request,
            &store,
            &suspend,
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("R1");
    terminal_sandbox
        .cleanup_checkpoint_for_terminal(
            &request,
            &store,
            &suspend,
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("R2");
    assert!(store.objects.lock().unwrap().is_empty(), "R2");
    terminal_sandbox
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("R1 durable preparation precedes physical disposal");
    terminal_sandbox
        .dispose_for_effect(
            &crate::test_disposal_authorization(&terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                crate::test_disposal_authorization(&terminal).effect_fence(),
            ),
        )
        .await
        .expect("R1 finish");
}

/// A creation this authority abandoned must be reclaimable; a live or foreign
/// root must not be.
///
/// The legacy (unfenced) creation path is what Agent-Run execution reaches:
/// `root_execution`/`child_execution` call `create_sandbox` on a `Fresh`
/// `LocalProvider` with the child RunId as the scope. Before the durable
/// creation record existed, a cancellation after the root was published left no
/// evidence at all, and the next attempt's absence check refused that scope
/// forever — a wedge, not a leak.
///
/// Causes: C1 the first attempt is dropped after its root exists and before it
/// completes; C2 the retry runs on a *different* provider value, so only
/// on-disk evidence can carry the decision; C3 the root at that scope came from
/// a creation that completed; C4 the root was made by someone else.
/// Effects: E1 the attempt realizes a sandbox; E2 the attempt is refused.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect |
/// | R1   | T  | T  | F  | F  | E1 — the abandonment is this authority's own |
/// | R2   | F  | T  | T  | F  | E2 — a completed root is in use |
/// | R3   | F  | T  | F  | T  | E2 — a root with no creation record is foreign |
#[tokio::test]
async fn an_abandoned_legacy_creation_is_reclaimed_while_a_live_or_foreign_root_is_refused() {
    /// Pends forever inside `realize_mount`, which runs strictly after the
    /// creation guard published the sandbox root. Dropping the caller there is
    /// exactly the cancellation the production path has no compensation for.
    struct NeverResolves;
    #[async_trait::async_trait]
    impl pc::BlobSource for NeverResolves {
        async fn get(&self, _: &str) -> Option<Vec<u8>> {
            std::future::pending().await
        }
    }

    fn expect_refusal(outcome: Result<LocalSandbox, pc::SandboxError>) -> String {
        match outcome {
            Ok(_) => panic!("creation was admitted where it had to be refused"),
            Err(error) => error.to_string(),
        }
    }

    fn seeded_spec(scope: &str) -> SandboxSpec {
        let mut spec = workdir_spec(scope, false);
        spec.mounts.push(pc::MountRequirement {
            mount_id: "seed".into(),
            mount_path: "/workspace/seed".into(),
            source: pc::MountSource::File {
                file_id: "synthetic".into(),
                content_hash: None,
            },
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Session,
            required: true,
        });
        spec
    }

    // R1 — abandon mid-flight, then retry the same scope on a fresh provider.
    let tmp = tempfile::tempdir().unwrap();
    let spec = seeded_spec("wedge");
    let root = tmp.path().join("wedge");
    {
        let stalled = LocalProvider::new(tmp.path()).with_blob_source(Arc::new(NeverResolves));
        let mut creation = Box::pin(stalled.create_sandbox(&spec));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::future::Future::poll(creation.as_mut(), &mut cx).is_pending(),
            "the fixture must stall the creation inside the mount, not settle it"
        );
        assert!(
            root.is_dir(),
            "C1 requires the abandoned attempt to have published its root already"
        );
        drop(creation);
    }
    let retry = LocalProvider::new(tmp.path()).with_blob("synthetic", b"seed bytes".to_vec());
    let reclaimed = retry
        .create_sandbox(&spec)
        .await
        .expect("R1 a retry at an abandoned scope must make progress");
    assert_eq!(
        std::fs::read(root.join("workspace/seed")).unwrap(),
        b"seed bytes",
        "R1 the reclaimed root is realized from scratch, not half-inherited"
    );
    drop(reclaimed);

    // R2 — the same scope is now a completed root, and creation is refused.
    let error = expect_refusal(retry.create_sandbox(&spec).await);
    assert!(
        error.contains("legacy sandbox root"),
        "R2 a completed root stays refused: {error}"
    );

    // R3 — a root nothing here created carries no record and stays refused.
    let foreign = tempfile::tempdir().unwrap();
    std::fs::create_dir(foreign.path().join("outsider")).unwrap();
    let provider = LocalProvider::new(foreign.path());
    let error = expect_refusal(
        provider
            .create_sandbox(&workdir_spec("outsider", false))
            .await,
    );
    assert!(
        error.contains("legacy sandbox root"),
        "R3 a foreign root stays refused: {error}"
    );
}
