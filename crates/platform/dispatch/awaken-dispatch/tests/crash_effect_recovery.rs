//! T07-C03 crash-after-commit observation: a real DispatchWorker child process
//! is killed (TerminateProcess / SIGKILL) inside a committed tool step, either
//! before the external effect (window `before_effect`) or after the effect but
//! before its receipt (window `after_effect`). A fresh process reopens the
//! same file SQLite dispatch and commit databases and recovers.
//!
//! Oracles: the recovered Run keeps its original Run/call identity; a
//! NeverReplay tool whose effect cannot be confirmed is sealed Indeterminate
//! and is never invoked again. Negative control: the same harness with the
//! tool advertising ReplaySafe (the "retry on every external timeout" policy)
//! re-invokes it, and the oracle check reports the duplicate.

mod harness;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_dispatch::{DispatchWorker, RunClaim, RunDispatch, SqliteDispatchStore};
use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchExecution as _, DispatchReads as _,
};
use awaken_lease_testkit::ManualClock;
use awaken_tool_contract::tool::{
    RawTool, ToolCall, ToolError, ToolOutput, ToolRecoveryCapability, ToolRecoveryPolicy,
};

/// The worker lease plays two roles. The stores judge expiry against the
/// test's `ManualClock`, which every `clock.set` below drives; the renewal guard
/// must also re-prove ownership within the lease in real host time, or it
/// cancels the drive. Only the first role is under test, so the lease is the
/// production one: with a 1s lease a renewal held up by fsync pressure past its
/// ~333ms budget cancelled the drive before the tool on a loaded host.
const LEASE: u64 = awaken_dispatch::DEFAULT_LEASE_MS;
const ROOT: &str = "AWAKEN_T07C03_ROOT";
const WINDOW: &str = "AWAKEN_T07C03_WINDOW";
const REPLAY_SAFE: &str = "AWAKEN_T07C03_REPLAY_SAFE";
const RUN: &str = "t07c03-run";

/// The external effect, observed through files that survive the child.
struct FileEffectTool {
    root: PathBuf,
    /// `Some(window)` in the doomed child: record, maybe effect, then hang.
    crash_window: Option<String>,
    replay_safe: bool,
}

fn append(path: &Path, line: &str) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
    file.sync_all().unwrap();
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[async_trait::async_trait]
impl RawTool for FileEffectTool {
    fn id(&self) -> &str {
        "echo"
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        if self.replay_safe {
            ToolRecoveryCapability::ReplaySafe
        } else {
            ToolRecoveryCapability::NonRecoverable
        }
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        append(&self.root.join("invocations.log"), &call.call_id);
        match self.crash_window.as_deref() {
            Some(window) => {
                if window == "after_effect" {
                    append(&self.root.join("effects.log"), &call.call_id);
                }
                std::fs::write(self.root.join("marker"), call.call_id.as_bytes()).unwrap();
                std::future::pending::<()>().await;
                unreachable!()
            }
            None => {
                append(&self.root.join("effects.log"), &call.call_id);
                Ok(ToolOutput::ok(call.call_id, "effect applied"))
            }
        }
    }
}

fn activation(replay_safe: bool) -> awaken_runtime_contract::activation::RunActivation {
    let mut activation = harness::activation(RUN);
    if replay_safe {
        let descriptors = &mut activation.snapshot.resolved_spec.tool_descriptors;
        descriptors[0] = descriptors[0]
            .clone()
            .with_recovery(ToolRecoveryPolicy::replay_safe());
    }
    activation
}

async fn open(
    root: &Path,
    clock: Arc<ManualClock>,
) -> (
    Arc<SqliteDispatchStore>,
    Arc<awaken_store_sqlite::SqliteCommitCoordinator>,
) {
    (
        Arc::new(
            SqliteDispatchStore::open(root.join("dispatch.db").to_str().unwrap(), clock)
                .await
                .unwrap(),
        ),
        Arc::new(
            awaken_store_sqlite::SqliteCommitCoordinator::open(
                root.join("commit.db").to_str().unwrap(),
            )
            .await
            .unwrap(),
        ),
    )
}

/// Child process body: claim, drive into the tool, and be killed there.
async fn doomed_child(root: &Path, window: String, replay_safe: bool) -> ! {
    let clock = Arc::new(ManualClock::new(0));
    let (dispatch, commit) = open(root, clock.clone()).await;
    dispatch
        .enqueue(RunDispatch::new(activation(replay_safe)))
        .await
        .unwrap();
    let claimed = dispatch
        .claim("process-a", LEASE, &Default::default())
        .await
        .unwrap()
        .unwrap();
    std::fs::write(root.join("epoch"), claimed.lease.epoch().to_string()).unwrap();
    let runtime = harness::tool_until_result_runtime(
        Arc::new(FileEffectTool {
            root: root.to_path_buf(),
            crash_window: Some(window),
            replay_safe,
        }),
        1,
    );
    let worker = DispatchWorker::new(runtime, dispatch, commit, "process-a").with_lease_ms(LEASE);
    let _drive = worker.drive_claimed(claimed, harness::at(&clock, 0)).await;
    panic!("the tool never returns in the doomed child");
}

/// Observed result of one crash-and-recover history.
#[derive(Debug)]
struct History {
    invocations: Vec<String>,
    effects: Vec<String>,
    final_state: Option<(RunId, RunState)>,
    tool_messages: usize,
}

/// The oracle shared by the real policy and the negative-control mutant.
fn oracle(history: &History, window: &str) -> Result<(), String> {
    if history.invocations != ["call-1"] {
        return Err(format!(
            "the tool was re-invoked: {:?}",
            history.invocations
        ));
    }
    let expected_effects: &[&str] = if window == "after_effect" {
        &["call-1"]
    } else {
        &[]
    };
    if history.effects != expected_effects {
        return Err(format!("external effects {:?}", history.effects));
    }
    match &history.final_state {
        Some((run, RunState::Ended(_))) if run.0 == RUN => {}
        other => return Err(format!("not recovered under the original Run: {other:?}")),
    }
    if history.tool_messages != 1 {
        return Err(format!("{} tool receipts", history.tool_messages));
    }
    Ok(())
}

async fn crash_and_recover(test_name: &str, window: &str, replay_safe: bool) -> History {
    let root = tempfile::tempdir().unwrap();
    let status =
        awaken_reliability_testkit::CrashProcess::new(test_name, root.path().join("marker"))
            .env(ROOT, root.path())
            .env(WINDOW, window)
            .env(REPLAY_SAFE, if replay_safe { "1" } else { "0" })
            .timeout(Duration::from_secs(60))
            .run()
            .expect("the child reaches the tool and is killed there");
    assert!(!status.success(), "the child was terminated");
    let crashed_epoch = awaken_lease_contract::fence::Epoch::from_persisted(
        std::fs::read_to_string(root.path().join("epoch"))
            .unwrap()
            .parse()
            .unwrap(),
    )
    .expect("the crashed child held a granted epoch");
    assert_eq!(lines(&root.path().join("invocations.log")), ["call-1"]);

    let clock = Arc::new(ManualClock::new(0));
    let (dispatch, commit) = open(root.path(), clock.clone()).await;
    let row = dispatch.list_dispatches().await.unwrap().remove(0);
    assert!(
        row.physical_attempt_active,
        "the dead child's slot is retained"
    );
    let tool = || {
        harness::tool_until_result_runtime(
            Arc::new(FileEffectTool {
                root: root.path().to_path_buf(),
                crash_window: None,
                replay_safe,
            }),
            1,
        )
    };

    // Recovery fails closed while the predecessor is unconfirmed.
    clock.set(LEASE + 1);
    let replacement = dispatch
        .claim("process-b", LEASE, &Default::default())
        .await
        .unwrap()
        .expect("expired claim is recoverable");
    assert!(replacement.recovered);
    assert_eq!(
        awaken_lease_contract::fence::Epoch::from_persisted(replacement.lease.epoch()),
        crashed_epoch.next(),
        "same dispatch identity"
    );
    let worker = DispatchWorker::new(tool(), dispatch.clone(), commit.clone(), "process-b")
        .with_lease_ms(LEASE);
    let blocked = tokio::time::timeout(
        Duration::from_secs(10),
        worker.drive_claimed(replacement, harness::at(&clock, LEASE + 1)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        blocked, None,
        "no restart before the predecessor is confirmed gone"
    );
    assert_eq!(lines(&root.path().join("invocations.log")).len(), 1);

    // The Coordinator confirmed the child's death (CrashProcess waited on it)
    // and acknowledges the exact predecessor attempt.
    assert_eq!(
        dispatch
            .finish_attempt(
                &RunClaim {
                    run_id: RunId(RUN.into()),
                    owner: "process-a".into(),
                    epoch: crashed_epoch.get(),
                },
                None,
            )
            .await
            .unwrap(),
        awaken_dispatch::SettleOutcome::Applied
    );
    clock.set(LEASE * 2 + 1);
    let recovered = dispatch
        .claim("process-b", LEASE, &Default::default())
        .await
        .unwrap()
        .expect("recovery claim after confirmed quiescence");
    let final_state = tokio::time::timeout(
        Duration::from_secs(10),
        worker.drive_claimed(recovered, harness::at(&clock, LEASE * 2 + 1)),
    )
    .await
    .unwrap()
    .unwrap();
    let tool_messages =
        CommittedThreadView::committed_messages(commit.as_ref(), &ThreadId(harness::THREAD.into()))
            .into_iter()
            .filter(|message| matches!(message.role, Role::Tool))
            .count();
    History {
        invocations: lines(&root.path().join("invocations.log")),
        effects: lines(&root.path().join("effects.log")),
        final_state,
        tool_messages,
    }
}

async fn child_or_parent(test_name: &str, window: &str) {
    if let Ok(root) = std::env::var(ROOT) {
        let window = std::env::var(WINDOW).unwrap();
        let replay_safe = std::env::var(REPLAY_SAFE).unwrap() == "1";
        doomed_child(Path::new(&root), window, replay_safe).await;
    }
    let never_replay = crash_and_recover(test_name, window, false).await;
    oracle(&never_replay, window)
        .unwrap_or_else(|error| panic!("{window} NeverReplay: {error}; {never_replay:?}"));

    let mutant = crash_and_recover(test_name, window, true).await;
    let detected = oracle(&mutant, window).expect_err("NC: a replaying policy must be detected");
    assert!(
        detected.contains("re-invoked"),
        "{window} NC detects the duplicate invocation: {detected}"
    );
    assert_eq!(
        mutant.invocations,
        ["call-1", "call-1"],
        "{window} NC same call id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_commit_before_effect_recovers_without_replay() {
    child_or_parent(
        "crash_after_commit_before_effect_recovers_without_replay",
        "before_effect",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_after_effect_before_receipt_keeps_uncertainty_without_replay() {
    child_or_parent(
        "crash_after_effect_before_receipt_keeps_uncertainty_without_replay",
        "after_effect",
    )
    .await;
}
