//! T28-C01 (过期和错误身份), container unit: the real `ContainerProvider`
//! over the fake container runtime collaborator, with counting BlobSource,
//! SecretBroker and MemoryMounter collaborators.
//!
//! Oracles (groups-6.json order): O0 zero physical calls; O1 a stable
//! stale/expired class (`SandboxError::FenceRejected`); O2 no mount or material.
//!
//! | Row | Current authority vs asserted fence | Effect |
//! |---|---|---|
//! | S1 expired | same fence, zero remaining budget | FenceRejected; 0 runtime/blob/secret/mount calls |
//! | S2 incarnation | another runtime incarnation | same |
//! | S3 epoch | the reassigned successor epoch | same |
//! | S4 owner | another owner | same |
//! | C | current | one live container; staging and mount happened (non-vacuous) |
//! | NC | effects first, authority only at the receipt | FenceRejected, but the O0 oracle detects it |
use super::*;
use std::time::Duration;

#[derive(Default)]
struct Counting {
    blob_reads: AtomicUsize,
    secret_reads: AtomicUsize,
    mounts: AtomicUsize,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
struct Effects {
    blob_reads: usize,
    secret_reads: usize,
    mounts: usize,
    runtime_observations: u32,
    live_containers: usize,
    created: usize,
}

struct InertMount;

#[async_trait]
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

#[async_trait]
impl pc::BlobSource for Counting {
    async fn get(&self, _: &str) -> Option<Vec<u8>> {
        self.blob_reads.fetch_add(1, Ordering::SeqCst);
        Some(b"t28 container input".to_vec())
    }
}

#[async_trait]
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

#[async_trait]
impl pc::MemoryMounter for Counting {
    async fn mount(
        &self,
        _: &str,
        host_path: &std::path::Path,
        _: pc::MountAccess,
    ) -> pc::MemoryMountOutcome {
        self.mounts.fetch_add(1, Ordering::SeqCst);
        pc::MemoryMountOutcome::from_no_effect(
            std::fs::create_dir_all(host_path)
                .map(|()| Box::new(InertMount) as Box<dyn pc::MemoryMount>)
                .map_err(|error| pc::SandboxError::backend("mount", error)),
        )
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
        _: &str,
        _: &pc::MemoryMaterializationEvidence,
        _: &[(String, Vec<u8>)],
        _: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "recovery is outside this fixture",
        ))
    }
}

fn effects(runtime: &FakeRuntime, counting: &Counting) -> Effects {
    let state = runtime.st.lock().unwrap();
    Effects {
        blob_reads: counting.blob_reads.load(Ordering::SeqCst),
        secret_reads: counting.secret_reads.load(Ordering::SeqCst),
        mounts: counting.mounts.load(Ordering::SeqCst),
        runtime_observations: state.observations,
        live_containers: state.alive.values().filter(|alive| **alive).count(),
        created: state.created_binds.len(),
    }
}

fn asserted() -> SandboxEffectFence {
    SandboxEffectFence::new("t28-create", "worker", "boot", 3, u64::MAX - 1).unwrap()
}

fn stale_authority(row: &str) -> pc::test_support::EffectAuthority {
    let fence = asserted();
    let authority = pc::test_support::EffectAuthority::new(&fence);
    let current = match row {
        "expired" => (fence, Duration::ZERO),
        "incarnation" => (
            SandboxEffectFence::new("t28-create", "worker", "boot-restarted", 3, u64::MAX - 1)
                .unwrap(),
            Duration::from_secs(1),
        ),
        "epoch" => {
            let mut successor = fence;
            successor.reassign();
            (successor, Duration::from_secs(1))
        }
        "owner" => (
            fence
                .with_persisted(|w| w.owner = "other-worker".into())
                .unwrap(),
            Duration::from_secs(1),
        ),
        _ => unreachable!("{row}"),
    };
    *authority.current.lock().unwrap() = Some(current);
    authority
}

fn requested() -> pc::SandboxSpec {
    let mut requested = spec("t28-c01-container");
    // A credential mount makes the SecretBroker a creation-time effect, so the
    // stale rows' zero-credential-read oracle is not vacuous.
    requested.mounts.push(pc::MountRequirement {
        mount_id: "credential".into(),
        source: pc::MountSource::Secret {
            reference: "t28-secret".into(),
            content_hash: None,
        },
        mount_path: "/data/credential.json".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    requested.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "t28-store".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/mnt/memory/t28".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    requested
}

fn fixture() -> (
    Arc<FakeRuntime>,
    Arc<Counting>,
    ContainerProvider<FakeRuntime>,
) {
    let runtime = Arc::new(FakeRuntime::default());
    let counting = Arc::new(Counting::default());
    // No seeded blobs: `provider_without_broker` seeds the inputs, which
    // would bypass the counting BlobSource and make the blob oracle vacuous.
    let provider = base_provider(runtime.clone())
        .with_blob_source(counting.clone())
        .with_secret_broker(counting.clone());
    provider.install_memory_mounter(counting.clone());
    (runtime, counting, provider)
}

/// O0+O1+O2 for one rejected creation.
fn rejected_before_effect(
    result: Result<(), pc::SandboxError>,
    before: Effects,
    after: Effects,
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
    Ok(())
}

async fn claim(
    provider: &ContainerProvider<FakeRuntime>,
    authority: &pc::test_support::EffectAuthority,
) -> Result<(), pc::SandboxError> {
    let fence = asserted();
    SandboxProvider::claim_for_effect(
        provider,
        &requested(),
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &fence,
            authority,
        },
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .map(|_| ())
}

#[tokio::test]
async fn t28_c01_stale_identities_reject_before_runtime_staging_credential_or_mount() {
    for row in ["expired", "incarnation", "epoch", "owner"] {
        let (runtime, counting, provider) = fixture();
        let before = effects(&runtime, &counting);
        let result = claim(&provider, &stale_authority(row)).await;
        rejected_before_effect(result, before, effects(&runtime, &counting))
            .unwrap_or_else(|violation| panic!("{row}: {violation}"));
    }

    // C: the identical spec under the current fence reaches every effect.
    let (runtime, counting, provider) = fixture();
    claim(
        &provider,
        &pc::test_support::EffectAuthority::new(&asserted()),
    )
    .await
    .expect("C current fence creates");
    let created = effects(&runtime, &counting);
    assert_eq!(created.live_containers, 1, "C one live container");
    assert!(
        created.blob_reads > 0,
        "C staging read the input: {created:?}"
    );
    assert!(
        created.secret_reads > 0,
        "C the credential was materialized: {created:?}"
    );
    assert_eq!(
        created.mounts, 1,
        "C the Memory mount happened: {created:?}"
    );
}

#[tokio::test]
async fn t28_c01_authorization_checked_only_at_the_receipt_is_detected() {
    // NC: the same collaborators driven by a pipeline that stages, reads the
    // credential and mounts first, then checks current authority only before
    // its receipt. Classification alone would pass; the O0 oracle must not.
    let (runtime, counting, _provider) = fixture();
    let authority = stale_authority("expired");
    let before = effects(&runtime, &counting);
    let staging = tempfile::tempdir().unwrap();
    pc::BlobSource::get(counting.as_ref(), "file-1")
        .await
        .unwrap();
    pc::SecretBroker::materialize_process(counting.as_ref(), "broker://k")
        .await
        .unwrap();
    assert!(matches!(
        pc::MemoryMounter::mount(
            counting.as_ref(),
            "t28-store",
            &staging.path().join("memory"),
            pc::MountAccess::ReadWrite,
        )
        .await,
        pc::MemoryMountOutcome::Mounted(_)
    ));
    let result = pc::with_current_sandbox_effect(&asserted(), &authority, |_| Ok(()));
    assert!(
        matches!(result, Err(pc::SandboxError::FenceRejected(_))),
        "NC the receipt-time check still classifies the fence: {result:?}"
    );
    let violation = rejected_before_effect(result, before, effects(&runtime, &counting))
        .expect_err("NC a receipt-only authorization check must be detected");
    assert!(violation.contains("physical calls"), "NC: {violation}");
}
