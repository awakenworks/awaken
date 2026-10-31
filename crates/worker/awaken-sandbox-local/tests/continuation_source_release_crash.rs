//! T19-C01 (源端数据责任): continuation checkpoint upload of a real Workdir
//! Sandbox runs in a child Host process that is KILLED at each production
//! boundary; a second Host adopts the persisted handle and retries.
//!
//! Oracles (groups-2.json order):
//! - O0 the source is not destroyed before data and cleanup responsibility
//!   (a durable checkpoint receipt) exist;
//! - O1 the retry keeps the original checkpoint and source operation.
//!
//! | Cut | Child state at kill | Second Host |
//! |---|---|---|
//! | K1 `before_put` | WAL may name the upload; no object | source intact; one upload; receipt |
//! | K2 `after_put` | object durable, response lost | source intact; same key and digest; one object |
//! | K3 `after_receipt` | receipt returned, never reported | source intact; cached receipt, zero uploads |
//! | then | source released only after the receipt | root removed; checkpoint object retained |
//! | NC | store `put` returns Err and the child finishes | harness refuses it as crash evidence |
//!
//! NC: an effect trait returning `Err` is not a production-stage crash. The
//! same harness run with that fault must be rejected (no marker, child exited).
use awaken_lease_contract::fence::Epoch;
use awaken_provisioning_contract as pc;
use awaken_sandbox_local::LocalProvider;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const CHILD: &str = "continuation_source_release_crash_child";
const BASE: &str = "AWAKEN_T19_C01_BASE";
const OBJECTS: &str = "AWAKEN_T19_C01_OBJECTS";
const HANDLE: &str = "AWAKEN_T19_C01_HANDLE";
const CUT: &str = "AWAKEN_T19_C01_CUT";
const MARKER: &str = "AWAKEN_T19_C01_MARKER";
const SCOPE: &str = "t19-c01-source";
const SOURCE_BYTES: &[u8] = b"t19 source filesystem";

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

fn fence(operation: &str, epoch: Epoch) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new(operation, "t19-owner", "t19-runtime", epoch.get(), u64::MAX)
        .unwrap()
}

fn epoch(value: u64) -> Epoch {
    Epoch::from_persisted(value).expect("a test epoch is nonzero")
}

fn create_fence() -> pc::SandboxEffectFence {
    fence("t19-create", epoch(1))
}

fn suspend_fence() -> pc::SandboxEffectFence {
    fence("t19-suspend", epoch(2))
}

fn capture_request() -> pc::SandboxCheckpointCaptureRequest {
    let generation = awaken_session_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(SCOPE),
        10,
        10_000,
        "environment",
        "base-image",
    );
    pc::SandboxCheckpointCaptureRequest {
        workspace_id: "workspace-t19".into(),
        session_id: SCOPE.into(),
        generation_id: generation.id().to_owned(),
        environment_fingerprint: generation.environment_fingerprint().to_owned(),
        base_image_fingerprint: generation.base_image_fingerprint().to_owned(),
        effect_id: suspend_fence().operation_id().to_owned(),
        format: "awaken-fs-tar-v1".into(),
        created_at_unix_ms: 20,
        expires_at_unix_ms: 10_000,
        max_bytes: 1024 * 1024,
    }
}

fn object_id(metadata: &pc::CheckpointObjectMetadata) -> String {
    format!(
        "{}/{}/{}/{}",
        metadata.workspace_id,
        metadata.session_id,
        metadata.generation_id,
        metadata.suspend_effect_id
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cut {
    None,
    BeforePut,
    AfterPut,
    Fail,
}

/// Checkpoint custody shared by both Host processes: one file per logical key.
struct DirectoryCheckpointStore {
    directory: PathBuf,
    cut: Cut,
    marker: Option<PathBuf>,
    puts: AtomicUsize,
}

impl DirectoryCheckpointStore {
    fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_owned(),
            cut: Cut::None,
            marker: None,
            puts: AtomicUsize::new(0),
        }
    }

    fn object(&self, id: &str) -> PathBuf {
        self.directory
            .join(blake3::hash(id.as_bytes()).to_hex().as_str())
    }

    async fn die_here(&self, at: &str) {
        publish_marker(self.marker.as_ref().expect("child marker"), at);
        std::future::pending::<()>().await;
    }
}

#[async_trait::async_trait]
impl pc::SandboxCheckpointStore for DirectoryCheckpointStore {
    async fn put(
        &self,
        metadata: &pc::CheckpointObjectMetadata,
        bytes: Vec<u8>,
    ) -> Result<pc::StoredCheckpointObject, pc::SandboxError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        match self.cut {
            Cut::BeforePut => self.die_here("before_put").await,
            Cut::Fail => {
                return Err(pc::SandboxError::backend(
                    "put",
                    "injected effect-trait failure",
                ));
            }
            Cut::None | Cut::AfterPut => {}
        }
        let id = object_id(metadata);
        let path = self.object(&id);
        match std::fs::read(&path) {
            Ok(existing) if existing != bytes => {
                return Err(pc::SandboxError::conflict(
                    "checkpoint key is already bound to different bytes",
                ));
            }
            Ok(_) => {}
            Err(_) => {
                let staged = path.with_extension("staged");
                std::fs::write(&staged, &bytes)
                    .and_then(|()| std::fs::rename(&staged, &path))
                    .map_err(|error| pc::SandboxError::backend("put", error))?;
            }
        }
        if self.cut == Cut::AfterPut {
            self.die_here("after_put").await;
        }
        Ok(pc::StoredCheckpointObject {
            id,
            digest: awaken_resource_contract::content_id(&bytes),
            size_bytes: bytes.len() as u64,
        })
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        std::fs::read(self.object(id)).map_err(|error| pc::SandboxError::backend("get", error))
    }

    async fn delete(&self, id: &str) -> Result<(), pc::SandboxError> {
        match std::fs::remove_file(self.object(id)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(pc::SandboxError::backend("delete", error))
            }
            _ => Ok(()),
        }
    }
}

fn publish_marker(marker: &Path, contents: &str) {
    let staged = marker.with_extension("staged");
    std::fs::write(&staged, contents).unwrap();
    std::fs::rename(staged, marker).unwrap();
}

fn objects(directory: &Path) -> Vec<PathBuf> {
    let mut objects = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_none())
        .collect::<Vec<_>>();
    objects.sort();
    objects
}

/// Parent-side crash harness: a crash is evidence only when the child reached
/// its marker and was then killed without graceful cleanup.
fn crash_child(environment: &[(&str, &Path)], cut: &str, marker: &Path) -> Result<String, String> {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD, "--ignored", "--nocapture"])
        .env(CUT, cut)
        .env(MARKER, marker)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    // awaken-allow(wait): the child is a separate OS process killed at its
    // marker; the marker file is its only cross-process signal, bounded above.
    while !marker.exists() {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("child exited with {status} before its marker"));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("child never reached its marker".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().map_err(|error| error.to_string())?;
    let status = child.wait().map_err(|error| error.to_string())?;
    if status.success() {
        return Err("child finished instead of being killed".into());
    }
    std::fs::read_to_string(marker).map_err(|error| error.to_string())
}

async fn adopt(base: &Path, handle: &pc::SandboxHandle) -> std::sync::Arc<dyn pc::Sandbox> {
    let create = create_fence();
    pc::SandboxProvider::adopt_for_effect(
        &LocalProvider::new(base),
        pc::SandboxAdoption::new(&spec(), handle),
        pc::SandboxEffect::Fenced {
            fence: &create,
            authority: &pc::test_support::EffectAuthority::new(&create),
        },
    )
    .await
    .expect("adopt the persisted source handle")
}

async fn checkpoint(
    sandbox: &dyn pc::Sandbox,
    store: &DirectoryCheckpointStore,
) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
    let suspend = suspend_fence();
    sandbox
        .checkpointing()
        .expect("Workdir Sandbox checkpoints")
        .checkpoint_for_effect(
            &capture_request(),
            store,
            &suspend,
            &pc::test_support::EffectAuthority::new(&suspend),
        )
        .await
}

fn disposal_authorization(prepared: &pc::SandboxEffectFence) -> pc::SandboxDisposalAuthorization {
    let preparation = pc::SandboxDisposalPreparation::new(
        prepared.clone(),
        format!("t19-preparation:{}", prepared.operation_id()),
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

#[tokio::test]
#[ignore = "private crash child launched only by the T19-C01 source-release table"]
async fn continuation_source_release_crash_child() {
    let variable = |name: &str| PathBuf::from(std::env::var_os(name).expect(name));
    let marker = variable(MARKER);
    let handle: pc::SandboxHandle =
        serde_json::from_slice(&std::fs::read(variable(HANDLE)).unwrap()).unwrap();
    let mut store = DirectoryCheckpointStore::new(&variable(OBJECTS));
    let cut = std::env::var(CUT).expect(CUT);
    store.cut = match cut.as_str() {
        "before_put" => Cut::BeforePut,
        "after_put" => Cut::AfterPut,
        "fail" => Cut::Fail,
        _ => Cut::None,
    };
    store.marker = Some(marker.clone());
    let sandbox = adopt(&variable(BASE), &handle).await;
    let result = checkpoint(sandbox.as_ref(), &store).await;
    if store.cut == Cut::Fail {
        // The graceful-error mutant: the effect returns Err and the Host
        // finishes normally. It never reaches a crash boundary.
        assert!(result.is_err(), "injected put failure surfaces as Err");
        return;
    }
    let reference = result.expect("child checkpoint");
    // Receipt in hand, never reported to the aggregate: die here.
    publish_marker(&marker, &serde_json::to_string(&reference).unwrap());
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn source_survives_every_upload_crash_and_the_retry_keeps_the_original_checkpoint() {
    for cut in ["before_put", "after_put", "after_receipt", "fail"] {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("hosts");
        let object_dir = tmp.path().join("objects");
        std::fs::create_dir_all(&object_dir).unwrap();

        // Host A realizes the source and persists its handle.
        let create = create_fence();
        let source = pc::SandboxProvider::claim_for_effect(
            &LocalProvider::new(&base),
            &spec(),
            pc::SandboxEffect::Fenced {
                fence: &create,
                authority: &pc::test_support::EffectAuthority::new(&create),
            },
            pc::SandboxRealizationIntent::Create,
        )
        .await
        .unwrap();
        let source_root = base.join(SCOPE);
        let source_file = source_root.join("state.txt");
        std::fs::write(&source_file, SOURCE_BYTES).unwrap();
        let handle_path = tmp.path().join("handle.json");
        std::fs::write(&handle_path, serde_json::to_vec(&source.handle()).unwrap()).unwrap();
        drop(source);

        let marker = tmp.path().join("marker");
        let crashed = tokio::task::spawn_blocking({
            let (base, object_dir, handle_path, marker) = (
                base.clone(),
                object_dir.clone(),
                handle_path.clone(),
                marker.clone(),
            );
            move || {
                crash_child(
                    &[
                        (BASE, base.as_path()),
                        (OBJECTS, object_dir.as_path()),
                        (HANDLE, handle_path.as_path()),
                    ],
                    cut,
                    &marker,
                )
            }
        })
        .await
        .unwrap();
        if cut == "fail" {
            // NC: the effect-trait error is not admitted as crash evidence.
            let refusal = crashed.expect_err("NC an Err-returning effect is not a crash");
            assert!(refusal.contains("before its marker"), "NC: {refusal}");
            assert!(objects(&object_dir).is_empty(), "NC no durable object");
            assert_eq!(std::fs::read(&source_file).unwrap(), SOURCE_BYTES);
            continue;
        }
        let crashed = crashed.unwrap_or_else(|error| panic!("{cut}: {error}"));

        // O0: whatever the crash boundary, the source is intact.
        assert_eq!(
            std::fs::read(&source_file).unwrap(),
            SOURCE_BYTES,
            "{cut}/O0 source survives the crashed upload"
        );
        assert_eq!(
            objects(&object_dir).len(),
            usize::from(cut != "before_put"),
            "{cut}: durable object boundary"
        );

        // Host B adopts the same source and retries the same operation.
        let handle: pc::SandboxHandle =
            serde_json::from_slice(&std::fs::read(&handle_path).unwrap()).unwrap();
        let sandbox = adopt(&base, &handle).await;
        let store = DirectoryCheckpointStore::new(&object_dir);
        let reference = checkpoint(sandbox.as_ref(), &store)
            .await
            .unwrap_or_else(|error| panic!("{cut}: exact retry: {error}"));

        // O1: the original checkpoint and source operation.
        assert_eq!(
            reference.suspend_effect_id,
            suspend_fence().operation_id(),
            "{cut}/O1 source operation"
        );
        let stored = objects(&object_dir);
        assert_eq!(stored.len(), 1, "{cut}/O1 exactly one checkpoint object");
        assert_eq!(
            awaken_resource_contract::content_id(&std::fs::read(&stored[0]).unwrap()),
            reference.digest,
            "{cut}/O1 the receipt names the durable bytes"
        );
        let uploads = store.puts.load(Ordering::SeqCst);
        if cut == "after_receipt" {
            let original: pc::SandboxCheckpointRef = serde_json::from_str(&crashed).unwrap();
            assert_eq!(reference, original, "{cut}/O1 the original receipt");
            assert_eq!(
                uploads, 0,
                "{cut}/O1 the recorded receipt is not re-uploaded"
            );
        } else {
            assert_eq!(uploads, 1, "{cut}/O1 one exact upload under the same key");
        }
        let replay = checkpoint(sandbox.as_ref(), &store).await.unwrap();
        assert_eq!(replay, reference, "{cut}/O1 receipt replay");
        assert_eq!(store.puts.load(Ordering::SeqCst), uploads, "{cut}/O1");
        assert_eq!(
            std::fs::read(&source_file).unwrap(),
            SOURCE_BYTES,
            "{cut}/O0 the receipt itself does not release the source"
        );

        // Source release after the receipt: the root goes, the data stays.
        let terminal = fence("t19-terminal", epoch(3));
        sandbox
            .prepare_disposal_for_effect(
                &terminal,
                &pc::test_support::EffectAuthority::new(&terminal),
            )
            .await
            .unwrap();
        let authorization = disposal_authorization(&terminal);
        sandbox
            .dispose_for_effect(
                &authorization,
                &pc::test_support::EffectAuthority::new(authorization.effect_fence()),
            )
            .await
            .unwrap();
        assert!(!source_root.exists(), "{cut}: source released last");
        assert_eq!(
            objects(&object_dir).len(),
            1,
            "{cut}/O0 data responsibility retained after release"
        );
    }
}
