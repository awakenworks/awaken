// @formal evidence sandbox.scoped_launch_requires_current_grant_and_init_exit_for_quiescence
//! Real Linux tree receipts through the existing Session Environment lifecycle.
//! The portability gate supplies the built helper explicitly; missing substrate
//! is failure, not a passing skip. Authority below is a provider-port fixture,
//! not evidence that production Session renewal has been installed.

use super::super::environment::EnvironmentStopProof;
use super::super::tests::{DoneProcess, FakeHandExecutor, UnreapableProcess, spec};
use super::*;
use crate::test_support::deployment_test_binary;
use crate::test_support::{SandboxEnvironmentAccess as _, UnusedHandExecutorFactory};
use crate::{SandboxEnvironment, SandboxEnvironmentProvider, SessionEnvironment};
use std::path::Path;

struct CurrentAuthority;

impl awaken_provisioning_contract::CurrentExecutionAuthority for CurrentAuthority {
    fn with_current(
        &self,
        effect: &mut dyn FnMut(Duration) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        effect(Duration::from_secs(60))
    }
}

async fn environment(base: &Path, failing_hand: bool) -> SessionEnvironment {
    let mut provider =
        SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(base, false);
    if failing_hand {
        provider.install_workdir_hand(
            Arc::new(UnusedHandExecutorFactory),
            "unused-hand".into(),
            Duration::ZERO,
        );
    }
    let fence =
        pc::SandboxEffectFence::new("create-scoped", "owner", "runtime", 1, u64::MAX).unwrap();
    let environment = provider
        .create_effective_for_effect(
            &spec(),
            awaken_session_environment_contract::ProviderEffect::Fenced {
                fence: &fence,
                authority: &awaken_provisioning_contract::test_support::EffectAuthority::new(
                    &fence,
                ),
            },
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
        .unwrap();
    let SandboxEnvironment::Workdir { sandbox, hand } = environment.sandbox_environment() else {
        panic!("TrustedHost Workdir")
    };
    sandbox
        .install_scoped_execution(
            &deployment_test_binary("AWAKEN_TEST_SCOPED_LAUNCHER"),
            &deployment_test_binary("AWAKEN_TEST_SCOPED_HELPER"),
            Arc::new(CurrentAuthority),
        )
        .unwrap();
    if let Some(hand) = hand {
        *hand.lifecycle.binding.lock().await = HandBindingState::Ready(HandBinding {
            process: Some(Box::new(UnreapableProcess)),
            executor: Arc::new(FakeHandExecutor),
        });
    }
    environment
}

fn command(ready_file: &str) -> pc::Command {
    // Two descendants deliberately outlive their shell leader; successful
    // leader reap is therefore insufficient for the Session receipt below.
    let mut command = pc::Command::new([
        "/bin/sh",
        "-c",
        "setsid /bin/sleep 60 & /bin/sleep 60 & printf ready > \"$1\"; wait",
        "scoped-session-fixture",
        ready_file,
    ]);
    command.cwd = "/workspace".into();
    command.stdio = pc::Stdio::Null;
    command
}

async fn wait_ready(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read(path).is_ok_and(|bytes| bytes == b"ready") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("workload forked its descendants before stop");
}

async fn terminal_delete(environment: &SessionEnvironment) {
    let fence =
        pc::SandboxEffectFence::new("terminal-scoped", "owner", "runtime", 1, u64::MAX).unwrap();
    let prepared = environment
        .lifecycle()
        .prepare_disposal_for_effect(
            &fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence),
        )
        .await
        .unwrap();
    let authorization =
        super::super::tests::restart_disposal_authorization(prepared, "scoped-stop");
    environment
        .lifecycle()
        .dispose_for_effect(
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "required real Linux helper supplied by the portability supervision gate"]
async fn trusted_host_scoped_session_stop_reaches_every_tree_before_resource_effects() {
    // Cause/effect graph: C1 two live scopes, from sandbox + ACP seams;
    // C2 Hand absent or indeterminate; C3 entry=revocation stop, checkpoint
    // quiesce, or candidate disposal; C4 later exact Hand reap succeeds.
    // E1 BOTH trees confirmed absent and admission remains closed;
    // E2 Hand uncertainty retained (never NoBinding or a successful receipt);
    // E3 exact resource bytes preserved until aggregate-authorized disposal;
    // E4 retry converges after all receipts, with no resource/process fallback.
    // Decision table (all rows C1):
    // | rule | Hand | entry | effects |
    // | S1 | absent | stop | E1+E3, tree proof (not NoBoundProcesses) |
    // | S2 | absent | quiesce | E1+E3, quiescence proof |
    // | S3 | absent | dispose | E1+E3, V2 unfenced disposal rejected |
    // | S4-S6 | unknown | each entry | E1+E2+E3, no short circuit |
    // | S7 | C4 | retry stop then authorized disposal | E4 |
    // Scope processes are real. Injected Hand uncertainty tests conjunction of
    // independent participants, not OS failure injection or live vendor login.
    for failing_hand in [false, true] {
        for entry in ["stop", "quiesce", "dispose"] {
            let base = tempfile::tempdir().unwrap();
            let environment = environment(base.path(), failing_hand).await;
            let SandboxEnvironment::Workdir { sandbox, hand } = environment.sandbox_environment()
            else {
                unreachable!()
            };
            let file = sandbox.workspace_path().join("workspace/retained");
            environment
                .agent_process()
                .materialize_inline("/workspace/retained", b"exact\0resource\xff")
                .await
                .unwrap();
            let native = sandbox.spawn(command("native-ready")).await.unwrap();
            let (acp, _channel) = environment
                .agent_process()
                .spawn_agent(command("acp-ready"))
                .await
                .unwrap();
            wait_ready(&sandbox.workspace_path().join("workspace/native-ready")).await;
            wait_ready(&sandbox.workspace_path().join("workspace/acp-ready")).await;
            assert!(native.poll().await.unwrap().is_none());
            assert!(acp.poll().await.unwrap().is_none());
            let result = match entry {
                "stop" => environment
                    .lifecycle()
                    .stop_bound_processes()
                    .await
                    .map(|proof| {
                        assert_eq!(proof, EnvironmentStopProof::ExecutionTreeQuiescent);
                    }),
                "quiesce" => environment.lifecycle().quiesce().await.map(|_| ()),
                "dispose" => environment.lifecycle().dispose().await,
                _ => unreachable!(),
            };
            assert_eq!(
                result.is_err(),
                failing_hand || entry == "dispose",
                "{entry}"
            );
            assert!(native.poll().await.unwrap().is_some(), "native {entry}");
            assert!(acp.poll().await.unwrap().is_some(), "ACP {entry}");
            assert_eq!(std::fs::read(&file).unwrap(), b"exact\0resource\xff");
            assert!(
                sandbox.spawn(command("closed")).await.is_err(),
                "closed {entry}"
            );
            if let Some(hand) = hand {
                assert!(hand.has_tracked_binding().await);
                assert!(!hand.lifecycle.is_open());
                let mut binding = hand.lifecycle.binding.lock().await;
                let HandBindingState::Ready(current) = &mut *binding else {
                    panic!("uncertain Hand must remain tracked")
                };
                current.process = Some(Box::new(DoneProcess::success("same-hand-reaped")));
            }
            assert_eq!(
                environment
                    .lifecycle()
                    .stop_bound_processes()
                    .await
                    .unwrap(),
                EnvironmentStopProof::ExecutionTreeQuiescent,
            );
            terminal_delete(&environment).await;
            assert!(!file.exists());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "required real Linux helper supplied by the portability supervision gate"]
async fn trusted_host_scoped_session_unknown_observer_blocks_receipt_and_retries() {
    // Decision rules: U1 live exact local handle + foreign durable observer =>
    // send exact stop but fail quiescence, retain bytes; U2 real marker restored
    // by this test's exclusive fixture + already stopped tree => retry succeeds;
    // U3 repeated quiescence stays closed; U4 only typed terminal authorization
    // removes resources. A successful Hand/no-Hand receipt cannot erase unknown
    // durable execution evidence. Editing the marker is fault injection only.
    let base = tempfile::tempdir().unwrap();
    let environment = environment(base.path(), false).await;
    let SandboxEnvironment::Workdir { sandbox, .. } = environment.sandbox_environment() else {
        unreachable!()
    };
    let file = sandbox.workspace_path().join("workspace/retained");
    environment
        .agent_process()
        .materialize_inline("/workspace/retained", b"retained exact bytes")
        .await
        .unwrap();
    let process = sandbox.spawn(command("ready")).await.unwrap();
    wait_ready(&sandbox.workspace_path().join("workspace/ready")).await;
    let marker = base
        .path()
        .join(format!(".{}.awaken-realization.json", spec().scope));
    let original = std::fs::read(&marker).unwrap();
    let mut foreign: serde_json::Value = serde_json::from_slice(&original).unwrap();
    foreign["executions"][0]["boot_id"] = serde_json::to_value([7_u8; 16]).unwrap();
    std::fs::write(&marker, serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert!(environment.lifecycle().quiesce().await.is_err());
    assert!(
        process.poll().await.unwrap().is_some(),
        "local exact stop was still attempted"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"retained exact bytes");
    std::fs::write(&marker, &original).unwrap();
    environment.lifecycle().quiesce().await.unwrap();
    environment.lifecycle().quiesce().await.unwrap();
    assert!(
        environment
            .agent_process()
            .spawn_agent(command("closed"))
            .await
            .is_err()
    );
    terminal_delete(&environment).await;
    assert!(!file.exists());
}

struct PendingReap(Arc<tokio::sync::Notify>);

#[async_trait]
impl pc::ProcessHandle for PendingReap {
    fn id(&self) -> &str {
        "pending-scoped-hand"
    }
    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        Ok(())
    }
    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        self.0.notify_one();
        std::future::pending().await
    }
    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        std::future::pending().await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "required real Linux helper supplied by the portability supervision gate"]
async fn trusted_host_scoped_session_cancelled_stop_retains_hand_and_concurrent_retry_converges() {
    // Temporal decision table: C1 Hand reap's initial poll waits at an observed
    // barrier (before the existing TERM grace, not after an assumed delay);
    // C2 real tree independently exits; C3 caller is cancelled; C4 two callers
    // concurrently retry after exact Hand reap. R1 C1+C2 => no head-of-line
    // blocking of tree stop; R2 +C3 => retain Hand and bytes, admission closed;
    // R3 +C4 => no new process, retryable marker-lock contention is NOT a receipt,
    // and canonical sequential retry converges before typed resource deletion.
    // The barrier and real pidfd poll establish ordering, not a scheduling sleep.
    let base = tempfile::tempdir().unwrap();
    let environment = Arc::new(environment(base.path(), true).await);
    let SandboxEnvironment::Workdir {
        sandbox,
        hand: Some(hand),
    } = environment.sandbox_environment()
    else {
        unreachable!()
    };
    environment
        .agent_process()
        .materialize_inline("/workspace/retained", b"retained")
        .await
        .unwrap();
    let file = sandbox.workspace_path().join("workspace/retained");
    let process = sandbox.spawn(command("ready")).await.unwrap();
    wait_ready(&sandbox.workspace_path().join("workspace/ready")).await;
    let waiting = Arc::new(tokio::sync::Notify::new());
    if let HandBindingState::Ready(binding) = &mut *hand.lifecycle.binding.lock().await {
        binding.process = Some(Box::new(PendingReap(waiting.clone())));
    } else {
        panic!("retained Hand fixture")
    }
    let task_environment = environment.clone();
    let stop =
        tokio::spawn(async move { task_environment.lifecycle().stop_bound_processes().await });
    tokio::time::timeout(Duration::from_secs(1), waiting.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while process.poll().await.unwrap().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("scope stop proceeds while Hand reap waits");
    stop.abort();
    assert!(stop.await.unwrap_err().is_cancelled());
    assert_eq!(std::fs::read(&file).unwrap(), b"retained");
    assert!(hand.has_tracked_binding().await);
    assert!(!hand.lifecycle.is_open());
    assert!(sandbox.spawn(command("closed")).await.is_err());
    if let HandBindingState::Ready(binding) = &mut *hand.lifecycle.binding.lock().await {
        binding.process = Some(Box::new(DoneProcess::success("same-hand-reaped")));
    } else {
        panic!("cancellation must not consume the pending Hand")
    }
    let (first, second) = tokio::join!(
        environment.lifecycle().stop_bound_processes(),
        environment.lifecycle().quiesce()
    );
    assert!(
        first.is_ok() || second.is_ok(),
        "one exact marker observer progresses"
    );
    environment.lifecycle().quiesce().await.unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"retained");
    terminal_delete(&environment).await;
    assert!(!file.exists());
}
