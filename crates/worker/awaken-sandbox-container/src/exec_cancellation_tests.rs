//! What survives an agent exec whose caller is cancelled between the exec
//! starting inside the container and the process reaching the caller.
//!
//! A fake `ContainerRuntime` rather than a daemon: the leak is a Worker-side
//! ordering defect, and only a fake can hold `spawn_agent` open at the instant
//! the caller is dropped. The physical half these tests stand in for — the pid
//! file the wrapper writes and the shell that lists it — is exercised against a
//! real daemon only by the feature-gated backend suites.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use crate::{ContainerSandbox, ContainerState, RuntimeAgentProcess, RuntimeError};

/// How the fake resolves a spawn that the test lets run to completion.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpawnOutcome {
    /// The exec starts and the process reaches the caller.
    Attached,
    /// The exec fails. A failed effect is equally no longer in flight, so the
    /// claim must settle here too.
    Rejected,
}

struct ExecRuntime {
    outcome: SpawnOutcome,
    /// Held closed until a test opens it, so a spawn can be observed *while*
    /// the exec is in flight.
    gate: tokio::sync::Semaphore,
    spawn_agent_started: tokio::sync::Notify,
    /// Every plain `spawn` argv, which is where the orphan reaper shows up.
    execs: Mutex<Vec<Vec<String>>>,
    reaper_exit: AtomicUsize,
}

impl ExecRuntime {
    fn new(outcome: SpawnOutcome) -> Self {
        Self {
            outcome,
            gate: tokio::sync::Semaphore::new(0),
            spawn_agent_started: tokio::sync::Notify::new(),
            execs: Mutex::new(Vec::new()),
            reaper_exit: AtomicUsize::new(0),
        }
    }

    fn reaper_argv(&self) -> Vec<Vec<String>> {
        self.execs.lock().unwrap().clone()
    }
}

struct ExitedProcess {
    code: i32,
}

#[async_trait]
impl pc::ProcessHandle for ExitedProcess {
    fn id(&self) -> &str {
        "fake-exec"
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        Ok(pc::ExitStatus {
            code: Some(self.code),
            signaled: false,
        })
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        Ok(Some(pc::ExitStatus {
            code: Some(self.code),
            signaled: false,
        }))
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

#[async_trait]
impl crate::ContainerRuntime for ExecRuntime {
    async fn probe_ready(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn create(
        &self,
        _id: &str,
        _plan: &crate::ContainerPlan,
    ) -> Result<String, RuntimeError> {
        Err(RuntimeError::Backend("not used by exec tests".into()))
    }

    async fn spawn(
        &self,
        _admitted: crate::AdmittedExec,
        command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        self.execs.lock().unwrap().push(command.argv);
        Ok(Box::new(ExitedProcess {
            code: i32::try_from(self.reaper_exit.load(Ordering::SeqCst)).unwrap_or(1),
        }))
    }

    async fn spawn_agent(
        &self,
        _admitted: crate::AdmittedExec,
        _command: pc::MaterializedCommand,
    ) -> Result<RuntimeAgentProcess, RuntimeError> {
        self.spawn_agent_started.notify_waiters();
        // Past this await the exec has begun inside the container: whatever the
        // caller does now, a process may exist.
        self.gate
            .acquire()
            .await
            .expect("exec test gate remains open")
            .forget();
        match self.outcome {
            SpawnOutcome::Attached => {
                let (agent, _host) = tokio::io::duplex(64);
                let (read, write) = tokio::io::split(agent);
                Ok(RuntimeAgentProcess {
                    process: Box::new(ExitedProcess { code: 0 }),
                    channel: Box::new(awaken_agent_channel::SplitChannel::new(read, write)),
                })
            }
            SpawnOutcome::Rejected => Err(RuntimeError::Backend("agent exec refused".into())),
        }
    }

    async fn open_channel(
        &self,
        _container_id: &str,
    ) -> Result<Box<dyn awaken_agent_channel::AgentChannel>, RuntimeError> {
        Err(RuntimeError::Backend("not used by exec tests".into()))
    }

    async fn inspect(&self, _container_id: &str) -> Result<ContainerState, RuntimeError> {
        Ok(ContainerState::Running)
    }

    async fn wait(&self, _container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        Err(RuntimeError::Backend("not used by exec tests".into()))
    }

    async fn poll(&self, _container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        Ok(None)
    }

    async fn signal(&self, _container_id: &str, _signal: pc::Signal) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        Ok(Vec::new())
    }

    async fn read_artifact(
        &self,
        _container_id: &str,
        _artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        Err(RuntimeError::Backend("not used by exec tests".into()))
    }

    async fn touch_lease(&self, _container_id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn remove(&self, _container_id: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
}

fn sandbox(runtime: Arc<ExecRuntime>) -> ContainerSandbox<ExecRuntime> {
    ContainerSandbox {
        retained_attempt: None,
        runtime,
        id: "exec-sandbox".into(),
        container_id: "exec-container".into(),
        outputs_path: "/outputs".into(),
        base_env: Vec::new(),
        control_services: Default::default(),
        sandbox_control_incarnation: None,
        control_publication: Arc::new(crate::ContainerControlPublicationRegistry::default()),
        blobs: Default::default(),
        file_store: None,
        live_input_projection: false,
        runtime_handle: None,
        continuation_excluded_paths: Vec::new(),
        adoption_fingerprint: None,
        realization_fingerprint: None,
        memory_materializations: Vec::new(),
        owned_paths: Mutex::new(Vec::new()),
        adopted_handle: None,
        realized: Vec::new(),
        recovered: false,
        lifecycle: Arc::new(crate::ContainerCleanupState::physical_cleanup_only()),
        execs: Arc::default(),
    }
}

/// Start `spawn_agent` on a detached task and return once the exec has begun
/// inside the container, so the caller can be dropped at that exact point.
async fn spawn_agent_in_flight(
    runtime: &Arc<ExecRuntime>,
    sandbox: &Arc<ContainerSandbox<ExecRuntime>>,
) -> tokio::task::JoinHandle<()> {
    let started = runtime.spawn_agent_started.notified();
    let spawning = {
        let sandbox = sandbox.clone();
        tokio::spawn(async move {
            let _ = sandbox.spawn_agent(pc::Command::new(["agent"])).await;
        })
    };
    started.await;
    spawning
}

/// Cause/effect design — the leak this closes:
///
/// C1 = an agent exec is in flight; C2 = the caller's future is dropped before
/// the process reaches it; C3 = the spawn returned (`Ok` or `Err`); C4 =
/// disposal runs. E1 = the attempt is on record as orphaned; E2 = disposal
/// makes the container enumerate and reap its own exec pid files; E3 = the
/// record is empty afterwards; E4 = no attempt is on record.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect    |
/// | X1   | 1  | 1  | 0  | 0  | E1        |
/// | X2   | 1  | 1  | 0  | 1  | E1,E2,E3  |
/// | X3   | 1  | 0  | 1  | 1  | E4,E2     |
///
/// X3 is what keeps the record honest, and it holds for a failed spawn too: a
/// failed effect is no longer in flight, so its claim settles and must not
/// retain.
#[tokio::test]
async fn an_agent_spawn_dropped_mid_exec_is_on_record_and_reaped_by_disposal() {
    let runtime = Arc::new(ExecRuntime::new(SpawnOutcome::Attached));
    let sandbox = Arc::new(sandbox(runtime.clone()));

    let spawning = spawn_agent_in_flight(&runtime, &sandbox).await;
    assert!(
        sandbox.orphaned_exec_attempts().is_empty(),
        "X1 nothing is owed while the caller still owns the spawn"
    );
    spawning.abort();
    let _ = spawning.await;

    assert_eq!(
        sandbox.orphaned_exec_attempts(),
        vec!["exec-0".to_string()],
        "X1"
    );
    assert!(
        runtime.reaper_argv().is_empty(),
        "X1 recording is not reaping"
    );

    // Let the abandoned exec complete inside the runtime, as a real container
    // would: the process exists and no Worker value refers to it.
    runtime.gate.add_permits(1);
    sandbox.dispose().await.expect("X2 disposal succeeds");

    let reaps = runtime.reaper_argv();
    assert_eq!(reaps.len(), 1, "X2 exactly one reap per disposal");
    let script = reaps[0].last().expect("X2 the reaper carries a script");
    assert!(
        script.contains("/tmp/awaken-exec/*.pid"),
        "X2 the reap enumerates rather than looks up: {script}"
    );
    assert!(sandbox.orphaned_exec_attempts().is_empty(), "X3");
}

/// X3: a spawn that returned leaves nothing owed, while disposal still reaps —
/// because the container's pid files, not the in-process record, are what say
/// whether a process is still there.
#[tokio::test]
async fn a_settled_agent_spawn_leaves_no_orphan_but_still_demands_a_reap() {
    for outcome in [SpawnOutcome::Attached, SpawnOutcome::Rejected] {
        let runtime = Arc::new(ExecRuntime::new(outcome));
        runtime.gate.add_permits(1);
        let sandbox = sandbox(runtime.clone());

        let spawned = sandbox.spawn_agent(pc::Command::new(["agent"])).await;
        assert_eq!(
            spawned.is_ok(),
            outcome == SpawnOutcome::Attached,
            "X3 fake honors the requested outcome"
        );
        assert!(sandbox.orphaned_exec_attempts().is_empty(), "X3 E4");
        drop(spawned);

        sandbox.dispose().await.expect("X3 disposal succeeds");
        assert_eq!(runtime.reaper_argv().len(), 1, "X3 E2");
    }
}

/// A Session environment that never exec'd cannot be holding an orphan, so it
/// must not demand reaping — otherwise every warm container's disposal would
/// fail closed against a runtime that has no exec at all.
///
/// C1 = no exec attempt was ever started; C2 = disposal runs.
/// E1 = no reap is attempted; E2 = disposal succeeds.
/// Rule: (C1,C2) -> (E1,E2).
#[tokio::test]
async fn a_sandbox_that_never_spawned_is_never_asked_to_reap() {
    let runtime = Arc::new(ExecRuntime::new(SpawnOutcome::Attached));
    let sandbox = sandbox(runtime.clone());

    sandbox.dispose().await.expect("E2");
    assert!(runtime.reaper_argv().is_empty(), "E1");
}

/// A reap the container could not complete must fail disposal rather than let
/// Secret write-back and the Memory harvest read a workspace a live orphan is
/// still writing.
///
/// C1 = an exec attempt was started; C2 = the reap exits non-zero.
/// E1 = disposal fails; E2 = the orphan record survives for the next attempt.
/// Rule: (C1,C2) -> (E1,E2).
#[tokio::test]
async fn a_failed_reap_fails_disposal_instead_of_harvesting_around_a_live_orphan() {
    let runtime = Arc::new(ExecRuntime::new(SpawnOutcome::Attached));
    runtime.reaper_exit.store(3, Ordering::SeqCst);
    let sandbox = Arc::new(sandbox(runtime.clone()));

    let spawning = spawn_agent_in_flight(&runtime, &sandbox).await;
    spawning.abort();
    let _ = spawning.await;
    runtime.gate.add_permits(1);

    let error = sandbox.dispose().await.expect_err("E1");
    assert!(error.to_string().contains("reaping"), "E1 {error}");
    assert_eq!(
        sandbox.orphaned_exec_attempts(),
        vec!["exec-0".to_string()],
        "E2"
    );
}
