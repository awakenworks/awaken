//! T19-C02 (目标恢复重试): the restore effect runs in a real child process that
//! is killed at a named boundary, then a fresh `LocalProvider` over the same
//! durable root retries the SAME committed restore request.
//!
//! Oracles (groups-2.json order):
//! - O0 the retry observes/continues the same restore operation;
//! - O1 nothing is restored twice (one physical target, converged bytes);
//! - O2 source checkpoint and target identity match exactly.
//!
//! | Cut | Child state at kill | Retry by a new provider |
//! |---|---|---|
//! | K1 `get` | target bound and created, zero entries written | Recovered; same target completes once |
//! | K2 `complete` | target complete, receipt never published | Recovered; same handle; still one target |
//! | NC | retry under a fresh effect id ("pick any new target") | Created a second target -> oracle rejects |
//!
//! The crash is a real `TerminateProcess`/`SIGKILL` of a child test process
//! after it publishes a marker; no effect trait is made to return an error.
use super::super::workdir_helper_tests::{checkpoint_request_for, workdir_spec};
use super::*;
use pc::Sandbox as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const CHILD: &str = "provider::restore_target::crash_tests::restore_crash_child";
const ROOT: &str = "AWAKEN_T19_C02_ROOT";
const STORE: &str = "AWAKEN_T19_C02_STORE";
const REFERENCE: &str = "AWAKEN_T19_C02_REFERENCE";
const CUT: &str = "AWAKEN_T19_C02_CUT";
const MARKER: &str = "AWAKEN_T19_C02_MARKER";
const SCOPE: &str = "t19-c02-restore";

/// Checkpoint custody shared by both processes: one file per logical key.
struct DirectoryCheckpointStore {
    directory: PathBuf,
    gets: AtomicUsize,
    /// Child-only: publish the marker inside `get` and never return.
    crash_inside_get: Option<PathBuf>,
}

impl DirectoryCheckpointStore {
    fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_owned(),
            gets: AtomicUsize::new(0),
            crash_inside_get: None,
        }
    }

    fn object(&self, id: &str) -> PathBuf {
        self.directory
            .join(blake3::hash(id.as_bytes()).to_hex().as_str())
    }
}

#[async_trait::async_trait]
impl pc::SandboxCheckpointStore for DirectoryCheckpointStore {
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
        std::fs::write(self.object(&id), &bytes)
            .map_err(|error| pc::SandboxError::backend("put", error))?;
        Ok(pc::StoredCheckpointObject {
            id,
            digest: awaken_resource_contract::content_id(&bytes),
            size_bytes: bytes.len() as u64,
        })
    }

    async fn get(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        if let Some(marker) = &self.crash_inside_get {
            publish_marker(marker, "inside-get");
            std::future::pending::<()>().await;
        }
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

fn restore_request(
    spec: &pc::SandboxSpec,
    checkpoint: &pc::SandboxCheckpointRef,
    operation: &str,
) -> pc::SandboxRestoreRequest {
    let capture = checkpoint_request_for(&spec.scope);
    pc::SandboxRestoreRequest {
        workspace_id: capture.workspace_id,
        session_id: spec.scope.clone(),
        effect_id: format!(
            "blake3:{}",
            awaken_resource_contract::content_id(operation.as_bytes())
        ),
        generation_id: capture.generation_id,
        checkpoint: checkpoint.clone(),
    }
}

fn disposal() -> pc::SandboxDisposalAuthorization {
    crate::test_disposal_authorization(
        &pc::SandboxEffectFence::new("t19-terminal", "owner", "runtime", 2, u64::MAX - 1).unwrap(),
    )
}

/// Every exact restoration binding the provider holds under `base`.
fn bound_targets(base: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(base)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".awaken-restore-target.json"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// O0+O1+O2 in one place: the retried result is the crashed operation's own
/// target, bound to the exact source checkpoint, and it is the only target.
fn continues_the_crashed_restore(
    base: &Path,
    spec: &pc::SandboxSpec,
    committed: &pc::SandboxRestoreRequest,
    handle: &pc::SandboxHandle,
) -> Result<(), String> {
    committed
        .verify_handle(spec, handle)
        .map_err(|error| format!("handle is not the committed restore operation: {error}"))?;
    let targets = bound_targets(base);
    if targets.len() != 1 {
        return Err(format!("{} restoration targets exist", targets.len()));
    }
    Ok(())
}

/// Parent-side crash runner: wait for the marker, then kill without cleanup.
fn run_crash_child(environment: &[(&str, &Path)], cut: &str, marker: &Path) -> String {
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
    let mut child = command.spawn().expect("spawn the restore crash child");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !marker.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("{cut}: crash child exited with {status} before its marker");
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{cut}: crash child never reached its marker");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "{cut}: the child was killed, not finished"
    );
    std::fs::read_to_string(marker).unwrap()
}

#[tokio::test]
#[ignore = "private crash child launched only by the T19-C02 restore table"]
async fn restore_crash_child() {
    let variable = |name: &str| PathBuf::from(std::env::var_os(name).expect(name));
    let base = variable(ROOT);
    let marker = variable(MARKER);
    let cut = std::env::var(CUT).expect(CUT);
    let checkpoint: pc::SandboxCheckpointRef =
        serde_json::from_slice(&std::fs::read(variable(REFERENCE)).unwrap()).unwrap();
    let spec = workdir_spec(SCOPE, false);
    let request = restore_request(&spec, &checkpoint, "t19-restore");
    let mut store = DirectoryCheckpointStore::new(&variable(STORE));
    if cut == "get" {
        store.crash_inside_get = Some(marker.clone());
    }
    let restored = pc::RestorableProvider::<std::sync::Arc<dyn pc::Sandbox>>::restore_environment(
        &LocalProvider::new(&base),
        &spec,
        &request,
        &store,
    )
    .await
    .expect("child restore");
    // The effect is complete; die before any receipt or root projection.
    publish_marker(
        &marker,
        &pc::Sandbox::handle(restored.target().as_ref()).sandbox_id,
    );
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn crashed_restore_is_continued_once_by_the_exact_committed_operation() {
    for cut in ["get", "complete"] {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("provider");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(&objects).unwrap();
        let spec = workdir_spec(SCOPE, false);
        let store = DirectoryCheckpointStore::new(&objects);

        // Source: one checkpoint object, then the source is gone.
        let provider = LocalProvider::new(&base);
        let source = provider.create_sandbox(&spec).await.unwrap();
        std::fs::create_dir_all(source.workspace_path().join("workspace")).unwrap();
        std::fs::write(
            source.workspace_path().join("workspace/value"),
            b"checkpoint-bytes",
        )
        .unwrap();
        let checkpoint = source
            .create_checkpoint(&checkpoint_request_for(&spec.scope), &store)
            .await
            .unwrap();
        source.dispose_unattached().await.unwrap();
        drop(provider);
        let reference = tmp.path().join("checkpoint.json");
        std::fs::write(&reference, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        let committed = restore_request(&spec, &checkpoint, "t19-restore");
        let evidence = committed.evidence(&spec);
        let target_root = restoration_root(&base, &evidence).unwrap();

        let marker = tmp.path().join("marker");
        let child_marker = tokio::task::spawn_blocking({
            let (base, objects, reference, marker) = (
                base.clone(),
                objects.clone(),
                reference.clone(),
                marker.clone(),
            );
            move || {
                run_crash_child(
                    &[
                        (ROOT, base.as_path()),
                        (STORE, objects.as_path()),
                        (REFERENCE, reference.as_path()),
                    ],
                    cut,
                    &marker,
                )
            }
        })
        .await
        .unwrap();

        // The crash left exactly the committed binding.
        assert_eq!(bound_targets(&base).len(), 1, "{cut}: one bound target");
        assert_eq!(
            target_root.join("workspace/value").exists(),
            cut == "complete",
            "{cut}: crash boundary"
        );

        // O0: a new provider recovers the crashed operation's own target.
        let recovering = LocalProvider::new(&base);
        let observed = recovering
            .acquire_restore_sandbox(&spec, &committed)
            .await
            .unwrap();
        assert_eq!(
            observed.disposition(),
            pc::SandboxRestoreTargetDisposition::Recovered,
            "{cut}/O0 the crashed reservation is continued, not recreated"
        );
        assert_eq!(observed.evidence(), &evidence, "{cut}/O2");
        drop(observed);

        let retried =
            pc::RestorableProvider::<std::sync::Arc<dyn pc::Sandbox>>::restore_environment(
                &recovering,
                &spec,
                &committed,
                &store,
            )
            .await
            .unwrap_or_else(|error| panic!("{cut}: exact retry: {error}"));
        let handle = pc::Sandbox::handle(retried.target().as_ref());
        continues_the_crashed_restore(&base, &spec, &committed, &handle)
            .unwrap_or_else(|violation| panic!("{cut}/O0-O2: {violation}"));
        if cut == "complete" {
            assert_eq!(
                handle.sandbox_id, child_marker,
                "{cut}/O0 the same target identity the child completed"
            );
        }
        assert_eq!(
            std::fs::read(target_root.join("workspace/value")).unwrap(),
            b"checkpoint-bytes",
            "{cut}/O1 exact bytes once"
        );
        assert_eq!(
            store.gets.load(Ordering::SeqCst),
            1,
            "{cut}/O1 the retry reads the durable object exactly once"
        );
        // O2: a request naming another checkpoint cannot claim this target.
        let mut foreign = checkpoint.clone();
        foreign.digest = awaken_resource_contract::content_id(b"other checkpoint");
        let foreign_request = pc::SandboxRestoreRequest {
            checkpoint: foreign,
            ..committed.clone()
        };
        assert!(
            foreign_request.verify_handle(&spec, &handle).is_err(),
            "{cut}/O2 the target is bound to the exact source checkpoint"
        );
        drop(retried);

        // NC: "pick any new target from the current provider" is executed and
        // must be rejected by the same oracle.
        let substitute = restore_request(&spec, &checkpoint, "t19-substitute-target");
        let substituted =
            pc::RestorableProvider::<std::sync::Arc<dyn pc::Sandbox>>::restore_environment(
                &recovering,
                &spec,
                &substitute,
                &store,
            )
            .await
            .expect("NC substitute target restores its own bytes");
        let violation = continues_the_crashed_restore(
            &base,
            &spec,
            &committed,
            &pc::Sandbox::handle(substituted.target().as_ref()),
        )
        .expect_err("NC a fresh target must not count as continuing the crashed restore");
        assert!(
            violation.contains("not the committed restore operation"),
            "{cut}/NC: {violation}"
        );
        drop(substituted);

        let authorization = disposal();
        let authority = pc::test_support::EffectAuthority::new(authorization.effect_fence());
        for request in [&committed, &substitute] {
            pc::RestoreTargetProvider::<std::sync::Arc<dyn pc::Sandbox>>::dispose_restored(
                &recovering,
                &spec,
                request,
                &authorization,
                &authority,
            )
            .await
            .unwrap();
        }
        assert!(bound_targets(&base).is_empty(), "{cut}: exact cleanup");
    }
}
