//! T28-C01 (过期和错误身份), native Local provider: an expired fence, a wrong
//! runtime incarnation, a wrong epoch or a wrong owner is presented before the
//! physical creation, blob, credential and Memory-mount effects.
//!
//! Oracles (groups-6.json order):
//! - O0 zero physical calls (directory, blob read, credential read, mount);
//! - O1 a stable stale/expired classification (`SandboxError::FenceRejected`);
//! - O2 no mount or material is materialized.
//!
//! | Row | Current authority vs asserted fence | Create | Adopt of a live Sandbox |
//! |---|---|---|---|
//! | S1 expired | same fence, zero remaining budget | rejected, 0 effects | rejected, 0 new effects |
//! | S2 incarnation | another runtime incarnation | rejected, 0 effects | rejected, 0 new effects |
//! | S3 epoch | a later epoch | rejected, 0 effects | rejected, 0 new effects |
//! | S4 owner | another owner | rejected, 0 effects | rejected, 0 new effects |
//! | C | current | created, every effect once | - |
//! | NC | effects first, authority checked only at the receipt | FenceRejected, but O0 detects it |
use awaken_lease_contract::fence::Epoch;
use awaken_provisioning_contract as pc;
use awaken_sandbox_local::LocalProvider;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const SCOPE: &str = "t28-c01-native";

#[derive(Default)]
struct Counting {
    blob_reads: AtomicUsize,
    secret_reads: AtomicUsize,
    mounts: AtomicUsize,
    recoveries: AtomicUsize,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
struct Effects {
    blob_reads: usize,
    secret_reads: usize,
    mounts: usize,
    recoveries: usize,
}

impl Counting {
    fn effects(&self) -> Effects {
        Effects {
            blob_reads: self.blob_reads.load(Ordering::SeqCst),
            secret_reads: self.secret_reads.load(Ordering::SeqCst),
            mounts: self.mounts.load(Ordering::SeqCst),
            recoveries: self.recoveries.load(Ordering::SeqCst),
        }
    }
}

struct InertMount;

#[async_trait::async_trait]
impl pc::MemoryMount for InertMount {
    fn realization(&self) -> pc::Realization {
        pc::Realization::Copy
    }

    // A copy-backed mount must publish its original durable heads (here: an
    // empty store); without them the provider correctly refuses the mount.
    fn materialization_heads(&self) -> Option<Vec<pc::MemoryMaterializationHead>> {
        Some(Vec::new())
    }

    async fn teardown(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl pc::BlobSource for Counting {
    async fn get(&self, _: &str) -> Option<Vec<u8>> {
        self.blob_reads.fetch_add(1, Ordering::SeqCst);
        Some(b"t28 blob".to_vec())
    }
}

#[async_trait::async_trait]
impl pc::SecretBroker for Counting {
    async fn materialize(&self, _: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.secret_reads.fetch_add(1, Ordering::SeqCst);
        Ok(b"t28 synthetic credential".to_vec())
    }

    async fn materialize_process(&self, _: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.secret_reads.fetch_add(1, Ordering::SeqCst);
        Ok(b"t28 synthetic process credential".to_vec())
    }

    async fn write_back(
        &self,
        _: pc::SecretWritebackTarget<'_>,
        _: Vec<u8>,
    ) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported("read-only fixture"))
    }
}

#[async_trait::async_trait]
impl pc::MemoryMounter for Counting {
    async fn mount(&self, _: &str, host_path: &Path, _: pc::MountAccess) -> pc::MemoryMountOutcome {
        self.mounts.fetch_add(1, Ordering::SeqCst);
        if let Err(error) = std::fs::create_dir_all(host_path) {
            return pc::MemoryMountOutcome::NoEffect(pc::SandboxError::backend("mount", error));
        }
        pc::MemoryMountOutcome::Mounted(Box::new(InertMount))
    }

    async fn recover_unknown_mount(
        &self,
        _: &str,
        _: &Path,
        _: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        self.recoveries.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn reconcile_recovered_copy(
        &self,
        _: &str,
        _: &pc::MemoryMaterializationEvidence,
        _: &[(String, Vec<u8>)],
        _: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        self.recoveries.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn mount(
    id: &str,
    path: &str,
    source: pc::MountSource,
    access: pc::MountAccess,
) -> pc::MountRequirement {
    pc::MountRequirement {
        mount_id: id.into(),
        source,
        mount_path: path.into(),
        access,
        lifetime: pc::MountLifetime::Session,
        required: true,
    }
}

fn spec() -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: SCOPE.into(),
        isolation: pc::IsolationClass::Workdir,
        mounts: vec![
            mount(
                "blob",
                "/workspace/blob.txt",
                pc::MountSource::File {
                    file_id: "t28-file".into(),
                    content_hash: None,
                },
                pc::MountAccess::ReadOnly,
            ),
            mount(
                "credential",
                "/workspace/credential.json",
                pc::MountSource::Secret {
                    reference: "t28-secret".into(),
                    content_hash: None,
                },
                pc::MountAccess::ReadOnly,
            ),
            mount(
                "memory",
                "/mnt/memory/t28",
                pc::MountSource::MemoryStore {
                    store_id: "t28-store".into(),
                    materialization_reference: None,
                    write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
                },
                pc::MountAccess::ReadWrite,
            ),
        ],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

fn fence(owner: &str, runtime: &str, epoch: Epoch) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new("t28-create", owner, runtime, epoch.get(), u64::MAX - 1).unwrap()
}

fn epoch(value: u64) -> Epoch {
    Epoch::from_persisted(value).expect("a test epoch is nonzero")
}

fn asserted() -> pc::SandboxEffectFence {
    fence("t28-owner", "t28-runtime", epoch(3))
}

/// The current authority for each stale row; the asserted fence never changes.
fn stale_authority(row: &str) -> pc::test_support::EffectAuthority {
    let authority = pc::test_support::EffectAuthority::new(&asserted());
    {
        let mut current = authority.current.lock().unwrap();
        let (current_fence, remaining) = current.as_mut().unwrap();
        match row {
            "expired" => *remaining = Duration::ZERO,
            "incarnation" => *current_fence = fence("t28-owner", "t28-runtime-restarted", epoch(3)),
            "epoch" => *current_fence = fence("t28-owner", "t28-runtime", epoch(4)),
            "owner" => *current_fence = fence("t28-other-owner", "t28-runtime", epoch(3)),
            _ => unreachable!("{row}"),
        }
    }
    authority
}

fn provider(base: &Path, collaborators: &Arc<Counting>) -> LocalProvider {
    LocalProvider::trusted_host(base)
        .with_blob_source(collaborators.clone())
        .with_secret_broker(collaborators.clone())
        .with_memory_mounter(collaborators.clone())
}

/// O0+O1+O2 for one rejected effect.
fn rejected_before_effect(
    result: Result<(), pc::SandboxError>,
    before: Effects,
    after: Effects,
    root: &Path,
    root_existed: bool,
) -> Result<(), String> {
    match result {
        Err(pc::SandboxError::FenceRejected(_)) => {}
        Err(other) => return Err(format!("unstable classification: {other:?}")),
        Ok(()) => return Err("a stale fence was admitted".into()),
    }
    if after != before {
        return Err(format!(
            "physical calls under a stale fence: {before:?} -> {after:?}"
        ));
    }
    if root.exists() != root_existed {
        return Err("the Sandbox directory changed".into());
    }
    for materialized in [
        "workspace/blob.txt",
        "workspace/credential.json",
        "mnt/memory/t28",
    ] {
        if !root_existed && root.join(materialized).exists() {
            return Err(format!("{materialized} was materialized"));
        }
    }
    Ok(())
}

async fn create(
    provider: &LocalProvider,
    authority: &pc::test_support::EffectAuthority,
) -> Result<Arc<dyn pc::Sandbox>, pc::SandboxError> {
    let asserted = asserted();
    pc::SandboxProvider::claim_for_effect(
        provider,
        &spec(),
        pc::SandboxEffect::Fenced {
            fence: &asserted,
            authority,
        },
        pc::SandboxRealizationIntent::Create,
    )
    .await
}

#[tokio::test]
async fn stale_or_expired_fences_reject_before_every_native_effect() {
    for row in ["expired", "incarnation", "epoch", "owner"] {
        let tmp = tempfile::tempdir().unwrap();
        let collaborators = Arc::new(Counting::default());
        let provider = provider(tmp.path(), &collaborators);
        let root = tmp.path().join(SCOPE);

        // Create under the stale fence.
        let before = collaborators.effects();
        let result = create(&provider, &stale_authority(row)).await.map(|_| ());
        rejected_before_effect(result, before, collaborators.effects(), &root, false)
            .unwrap_or_else(|violation| panic!("{row}/create: {violation}"));

        // Control row: the current fence performs each effect exactly once.
        let current = pc::test_support::EffectAuthority::new(&asserted());
        let sandbox = create(&provider, &current)
            .await
            .unwrap_or_else(|error| panic!("{row}/C current create: {error}"));
        assert_eq!(
            collaborators.effects(),
            Effects {
                blob_reads: 1,
                secret_reads: 1,
                mounts: 1,
                recoveries: 0
            },
            "{row}/C each effect once under current authority"
        );
        assert_eq!(
            std::fs::read(root.join("workspace/blob.txt")).unwrap(),
            b"t28 blob"
        );
        let handle = sandbox.handle();
        drop(sandbox);

        // Adoption of the live Sandbox under the stale fence.
        let before = collaborators.effects();
        let asserted = asserted();
        let adoption = pc::SandboxProvider::adopt_for_effect(
            &provider,
            pc::SandboxAdoption::new(&spec(), &handle),
            pc::SandboxEffect::Fenced {
                fence: &asserted,
                authority: &stale_authority(row),
            },
        )
        .await
        .map(|_| ());
        rejected_before_effect(adoption, before, collaborators.effects(), &root, true)
            .unwrap_or_else(|violation| panic!("{row}/adopt: {violation}"));
        assert_eq!(
            std::fs::read(root.join("workspace/blob.txt")).unwrap(),
            b"t28 blob",
            "{row}/adopt leaves the live Sandbox untouched"
        );
    }
}

#[tokio::test]
async fn authorization_checked_only_at_the_receipt_is_detected() {
    // NC: the same collaborators driven by a pipeline that performs the
    // effects first and checks current authority only before publishing its
    // receipt. It still reports FenceRejected, so classification alone would
    // pass; the O0/O2 oracle must reject it.
    let tmp = tempfile::tempdir().unwrap();
    let collaborators = Arc::new(Counting::default());
    let root = tmp.path().join(SCOPE);
    let authority = stale_authority("expired");
    let before = collaborators.effects();

    let late_check = async {
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let blob = pc::BlobSource::get(collaborators.as_ref(), "t28-file")
            .await
            .unwrap();
        std::fs::write(root.join("workspace/blob.txt"), blob).unwrap();
        pc::SecretBroker::materialize(collaborators.as_ref(), "t28-secret")
            .await
            .unwrap();
        let mount = pc::MemoryMounter::mount(
            collaborators.as_ref(),
            "t28-store",
            &root.join("mnt/memory/t28"),
            pc::MountAccess::ReadWrite,
        )
        .await;
        assert!(matches!(mount, pc::MemoryMountOutcome::Mounted(_)));
        pc::with_current_sandbox_effect(&asserted(), &authority, |_| Ok(()))
    };
    let result = late_check.await;
    assert!(
        matches!(result, Err(pc::SandboxError::FenceRejected(_))),
        "NC the receipt-time check still classifies the fence: {result:?}"
    );
    let violation = rejected_before_effect(result, before, collaborators.effects(), &root, false)
        .expect_err("NC a receipt-only authorization check must be detected");
    assert!(violation.contains("physical calls"), "NC: {violation}");
}
