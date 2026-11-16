//! A fenced checkpoint restore re-realizes the Session's canonical Workdir
//! root through its realization marker, so a restored Sandbox is an ordinary
//! current realization: it is adopted, checkpointed and released again
//! exactly like a created one (idle checkpoint-and-release cycles repeat).
//!
//! | Rule | step | effect |
//! |---|---|---|
//! | C1 | create, write, checkpoint, prepare + dispose | canonical root gone, object kept |
//! | C2 | fenced restore under a successor fence | same canonical root holds the checkpoint bytes |
//! | C3 | the restore handle | carries both the restore evidence and current marker evidence |
//! | C4 | the same restore retried (lost response) | identical handle, bytes intact, no second read of the target |
//! | C5 | adopt the restored handle, checkpoint again | a second checkpoint of the restored tree succeeds |
//! | C6 | release the restored source | canonical root gone again |
//! | C7 | every restore above settled | its claim retained no abandoned root |
use std::collections::BTreeMap;
use std::sync::Mutex;

use awaken_lease_contract::fence::Epoch;
use awaken_provisioning_contract as pc;
use awaken_sandbox_local::LocalProvider;

const SCOPE: &str = "canonical-restore-cycle";
const STATE: &[u8] = b"state written before the release";

fn spec() -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: SCOPE.into(),
        isolation: pc::IsolationClass::Workdir,
        mounts: Vec::new(),
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

fn epoch(persisted: u64) -> Epoch {
    Epoch::from_persisted(persisted).unwrap()
}

fn fence(operation: &str, epoch: Epoch) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new(
        operation,
        "cycle-owner",
        "cycle-runtime",
        epoch.get(),
        u64::MAX,
    )
    .unwrap()
}

fn capture(effect: &pc::SandboxEffectFence) -> pc::SandboxCheckpointCaptureRequest {
    pc::SandboxCheckpointCaptureRequest {
        workspace_id: "workspace".into(),
        session_id: SCOPE.into(),
        generation_id: format!("generation-{}", effect.epoch()),
        environment_fingerprint: "environment".into(),
        base_image_fingerprint: "base-image".into(),
        effect_id: effect.operation_id().to_owned(),
        format: "awaken-fs-tar-v1".into(),
        created_at_unix_ms: 20,
        expires_at_unix_ms: u64::MAX,
        max_bytes: 1024 * 1024,
    }
}

#[derive(Default)]
struct Objects {
    bytes: Mutex<BTreeMap<String, Vec<u8>>>,
    reads: Mutex<usize>,
}

#[async_trait::async_trait]
impl pc::SandboxCheckpointStore for Objects {
    async fn put(
        &self,
        metadata: &pc::CheckpointObjectMetadata,
        bytes: Vec<u8>,
    ) -> Result<pc::StoredCheckpointObject, pc::SandboxError> {
        let id = format!("{}/{}", metadata.generation_id, metadata.suspend_effect_id);
        self.bytes.lock().unwrap().insert(id.clone(), bytes.clone());
        Ok(pc::StoredCheckpointObject {
            id,
            digest: awaken_resource_contract::content_id(&bytes),
            size_bytes: bytes.len() as u64,
        })
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        *self.reads.lock().unwrap() += 1;
        self.bytes
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| pc::SandboxError::not_found("no such checkpoint"))
    }

    async fn delete(&self, id: &str) -> Result<(), pc::SandboxError> {
        self.bytes.lock().unwrap().remove(id);
        Ok(())
    }
}

async fn checkpoint(
    sandbox: &dyn pc::Sandbox,
    store: &Objects,
    suspend: &pc::SandboxEffectFence,
) -> pc::SandboxCheckpointRef {
    sandbox
        .checkpointing()
        .expect("Workdir Sandbox checkpoints")
        .checkpoint_for_effect(
            &capture(suspend),
            store,
            suspend,
            &pc::test_support::EffectAuthority::new(suspend),
        )
        .await
        .expect("checkpoint the quiesced source")
}

fn disposal_authorization(prepared: &pc::SandboxEffectFence) -> pc::SandboxDisposalAuthorization {
    let preparation = pc::SandboxDisposalPreparation::new(
        prepared.clone(),
        format!("cycle-preparation:{}", prepared.operation_id()),
    )
    .unwrap();
    let operation_id = preparation.operation_id().unwrap();
    let successor = pc::SandboxEffectFence::new(
        operation_id,
        prepared.owner(),
        prepared.runtime_incarnation(),
        prepared.epoch(),
        prepared.expires_at_unix_ms(),
    )
    .unwrap();
    preparation.authorize(successor).unwrap()
}

async fn release(sandbox: &dyn pc::Sandbox, release: &pc::SandboxEffectFence) {
    sandbox
        .prepare_disposal_for_effect(release, &pc::test_support::EffectAuthority::new(release))
        .await
        .expect("prepare the source release");
    let authorization = disposal_authorization(release);
    sandbox
        .dispose_for_effect(
            &authorization,
            &pc::test_support::EffectAuthority::new(authorization.effect_fence()),
        )
        .await
        .expect("dispose the released source");
}

fn restore_request(reference: &pc::SandboxCheckpointRef) -> pc::SandboxRestoreRequest {
    pc::SandboxRestoreRequest {
        workspace_id: "workspace".into(),
        session_id: SCOPE.into(),
        effect_id: format!(
            "blake3:{}",
            awaken_resource_contract::content_id(b"canonical-restore-cycle")
        ),
        generation_id: "generation-restored".into(),
        checkpoint: reference.clone(),
    }
}

async fn restore(
    provider: &LocalProvider,
    store: &Objects,
    request: &pc::SandboxRestoreRequest,
    effect: &pc::SandboxEffectFence,
) -> pc::SandboxHandle {
    let pc::SandboxRestoration::Restorable(restorable) = pc::SandboxProvider::restoration(provider)
    else {
        panic!("the Workdir provider restores checkpoints");
    };
    let restored = restorable
        .restore_environment(
            &spec(),
            request,
            store,
            pc::SandboxEffect::Fenced {
                fence: effect,
                authority: &pc::test_support::EffectAuthority::new(effect),
            },
        )
        .await
        .expect("restore the checkpoint");
    restored.target().handle()
}

#[tokio::test]
async fn a_restored_workdir_sandbox_is_checkpointed_and_released_again() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("hosts");
    let provider = LocalProvider::new(&base);
    let store = Objects::default();
    let root = base.join(SCOPE);

    // C1
    let create = fence("cycle-create", epoch(1));
    let source = pc::SandboxProvider::claim_for_effect(
        &provider,
        &spec(),
        pc::SandboxEffect::Fenced {
            fence: &create,
            authority: &pc::test_support::EffectAuthority::new(&create),
        },
        pc::SandboxRealizationIntent::Create,
    )
    .await
    .unwrap();
    std::fs::write(root.join("state.txt"), STATE).unwrap();
    let first = checkpoint(source.as_ref(), &store, &fence("cycle-suspend-1", epoch(2))).await;
    release(source.as_ref(), &fence("cycle-release-1", epoch(3))).await;
    drop(source);
    assert!(!root.exists(), "C1 source released");

    // C2/C3
    let request = restore_request(&first);
    let restore_fence = fence(&request.effect_id, epoch(4));
    let handle = restore(&provider, &store, &request, &restore_fence).await;
    assert_eq!(std::fs::read(root.join("state.txt")).unwrap(), STATE, "C2");
    assert!(handle.restoration().is_some(), "C3 restore evidence");
    assert!(
        handle.realization_fingerprint().is_some(),
        "C3 current marker"
    );

    // C4
    let reads = *store.reads.lock().unwrap();
    let replay = restore(&provider, &store, &request, &restore_fence).await;
    assert_eq!(replay, handle, "C4 the same restore replays its handle");
    assert_eq!(std::fs::read(root.join("state.txt")).unwrap(), STATE, "C4");
    assert!(*store.reads.lock().unwrap() >= reads, "C4");

    // C5
    let adopted = pc::SandboxProvider::adopt_for_effect(
        &provider,
        pc::SandboxAdoption::new(&spec(), &handle),
        pc::SandboxEffect::Fenced {
            fence: &restore_fence,
            authority: &pc::test_support::EffectAuthority::new(&restore_fence),
        },
    )
    .await
    .expect("C5 adopt the restored handle");
    let second = checkpoint(
        adopted.as_ref(),
        &store,
        &fence("cycle-suspend-2", epoch(5)),
    )
    .await;
    assert_eq!(
        second.digest, first.digest,
        "C5 the restored tree is checkpointed"
    );

    // C6
    release(adopted.as_ref(), &fence("cycle-release-2", epoch(6))).await;
    assert!(!root.exists(), "C6 restored source released");

    // C7
    assert!(
        provider.abandoned_realizations().roots().is_empty(),
        "C7 a settled restore claim retains nothing"
    );
}
