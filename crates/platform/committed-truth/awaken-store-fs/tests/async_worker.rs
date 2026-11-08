//! The store's synchronous file I/O never holds the async worker that awaits it.
//!
//! A commit's critical section (inspect, stamp, append + `fsync`, apply) and a
//! checkpoint read run on the blocking pool. Each test parks that I/O on a
//! single-worker runtime and requires another task, standing in for a lease
//! heartbeat, to run while it is parked. The park point is a fake inside the
//! same synchronous section: the commit's clock reading, taken between
//! inspection and the durable append, and a FIFO in place of a checkpoint file.
//! Inline, the parked section holds the only worker and the other task cannot
//! run until the park gives up, which each test reports as its failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use awaken_agent_contract::agent::message::{Id as MsgId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::observation::Observed;
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, OperationCoordinator};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_store_fs::{FsCommitCoordinator, FsStreamCheckpointStore};
use awaken_sync::LeafLock;
use awaken_time::{UtcTime, WallClock};

/// How long a parked section waits for the other task before giving up. Only
/// an inline section, which holds the worker that task needs, ever reaches it.
const PARK_LIMIT: Duration = Duration::from_secs(20);

/// A wall clock whose next reading, once armed, reports that it was entered
/// and then waits until the test releases it: a stand-in for an `fsync` the
/// filesystem holds, at the same point of the commit's critical section.
struct ParkedClock {
    armed: AtomicBool,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: LeafLock<mpsc::Receiver<()>>,
    released_in_time: AtomicBool,
}

impl ParkedClock {
    fn new() -> (
        Arc<Self>,
        tokio::sync::mpsc::UnboundedReceiver<()>,
        mpsc::Sender<()>,
    ) {
        let (entered, entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release) = mpsc::channel();
        let clock = Arc::new(Self {
            armed: AtomicBool::new(false),
            entered,
            release: LeafLock::new(release),
            released_in_time: AtomicBool::new(false),
        });
        (clock, entered_rx, release_tx)
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn released_in_time(&self) -> bool {
        self.released_in_time.load(Ordering::SeqCst)
    }
}

impl WallClock for ParkedClock {
    fn now(&self) -> UtcTime {
        if self.armed.swap(false, Ordering::SeqCst) {
            let _ = self.entered.send(());
            let released = self.release.lock().recv_timeout(PARK_LIMIT).is_ok();
            self.released_in_time.store(released, Ordering::SeqCst);
        }
        UtcTime::from_epoch_ms(1_700_000_000_000)
    }
}

fn running(thread: &str, run: &str, text: &str) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId(thread.into()),
        run: RunDisposition::running(RunId(run.into())),
        messages: vec![Message::text(
            MsgId(format!("m-{text}")),
            Role::Assistant,
            text,
        )],
        state: Vec::new(),
        events: Vec::new(),
    }
}

fn ended(thread: &str, run: &str, text: &str) -> ThreadCommit {
    ThreadCommit {
        run: RunDisposition::ended(RunId(run.into()), EndCause::NaturalEnd),
        ..running(thread, run, text)
    }
}

fn texts(store: &FsCommitCoordinator, thread: &str) -> Vec<String> {
    store
        .committed_messages(&ThreadId(thread.into()))
        .iter()
        .map(Message::text_content)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn a_parked_commit_leaves_the_one_async_worker_free() {
    // Decision table. C1 the store is entered through `commit` / through
    // `commit_operation`; C2 its critical section is parked between inspection
    // and the durable append; C3 another task on the one async worker is
    // waiting to run, as a lease heartbeat is when it comes due.
    // E1 (every C1) that task runs while the section is parked, and releases
    // it; E2 the commit is still durable and applied: its run is Ended and its
    // message reads back, from the live store and from a reopened log.
    for through_operation in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (clock, mut entered, release) = ParkedClock::new();
        let store = FsCommitCoordinator::open(dir.path(), clock.clone())
            .await
            .unwrap();
        let commit = ended("thread", "run", "parked");
        clock.arm();
        let heartbeat = tokio::spawn(async move {
            tokio::time::timeout(PARK_LIMIT, entered.recv())
                .await
                .expect("the section is entered within the park limit")
                .expect("the section was entered");
            release.send(()).expect("the section is still parked");
        });

        if through_operation {
            store
                .commit_operation(CommitOperation {
                    operation_id: CommitOperationId::new(RunId("run".into()), 0),
                    expected_thread_version: 0,
                    payload_hash: commit_payload_hash(&commit).unwrap(),
                    commit,
                })
                .await
                .expect("E2 operation");
        } else {
            store.commit(commit).await.expect("E2 commit");
        }

        assert!(clock.released_in_time(), "E1 {through_operation}");
        heartbeat.await.unwrap();
        let run = RunId("run".into());
        assert_eq!(
            store.run_state(&run),
            Some(RunState::Ended(EndCause::NaturalEnd)),
            "E2 {through_operation}"
        );
        drop(store);
        let reopened = FsCommitCoordinator::open(dir.path(), awaken_time::system_clock())
            .await
            .unwrap();
        assert_eq!(
            texts(&reopened, "thread"),
            ["parked"],
            "E2 {through_operation}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_dropped_commit_finishes_before_the_next_commit_is_admitted() {
    // Decision table. C1 commit A's critical section is parked on the blocking
    // pool; C2 A's caller is dropped there; C3 commit B on the same Thread and
    // Run is submitted while A is still parked; C4 A is released.
    // E1 B is not admitted while A is parked: it waits for the commit lock A's
    // job still holds; E2 A is applied whole, append and model together,
    // although nobody awaits it; E3 B follows A: the Thread reads A then B,
    // live and from the reopened log, and the Run ends as B committed it.
    let dir = tempfile::tempdir().unwrap();
    let (clock, mut entered, release) = ParkedClock::new();
    let store = Arc::new(
        FsCommitCoordinator::open(dir.path(), clock.clone())
            .await
            .unwrap(),
    );
    clock.arm();
    let first = tokio::spawn({
        let store = store.clone();
        async move { store.commit(running("thread", "run", "a")).await }
    });
    tokio::time::timeout(PARK_LIMIT, entered.recv())
        .await
        .expect("A's section is entered within the park limit")
        .expect("A's section was entered");
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled(), "C2");

    let second = tokio::spawn({
        let store = store.clone();
        async move { store.commit(ended("thread", "run", "b")).await }
    });
    // A yield runs every other ready task on the one worker first, so B has
    // been polled until it waits; it has neither finished nor applied.
    tokio::task::yield_now().await;
    assert!(
        !second.is_finished(),
        "E1 B was admitted while A was parked"
    );
    assert!(texts(&store, "thread").is_empty(), "E1");

    release.send(()).unwrap();
    second.await.unwrap().expect("E3 B commits");
    assert!(clock.released_in_time(), "C4");
    assert_eq!(texts(&store, "thread"), ["a", "b"], "E2/E3 live");
    assert_eq!(
        store.run_state(&RunId("run".into())),
        Some(RunState::Ended(EndCause::NaturalEnd)),
        "E3"
    );
    drop(store);
    let reopened = FsCommitCoordinator::open(dir.path(), awaken_time::system_clock())
        .await
        .unwrap();
    assert_eq!(texts(&reopened, "thread"), ["a", "b"], "E2/E3 durable");
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_parked_checkpoint_read_leaves_the_one_async_worker_free() {
    // Decision table. C1 the checkpoint file is a FIFO, so reading it parks
    // until a writer opens it; C2 another task on the one async worker is
    // waiting to run. E1 that task runs while the read is parked: its signal
    // makes the writer supply the checkpoint; E2 the read returns that exact
    // checkpoint. Inline, the writer gives up after PARK_LIMIT and writes bytes
    // that are no checkpoint, and the read reports them as Unknown.
    let dir = tempfile::tempdir().unwrap();
    let store = FsStreamCheckpointStore::open(dir.path()).unwrap();
    let fifo = dir.path().join("run-fifo.json");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success(), "C1");
    let checkpoint = StreamCheckpoint {
        run_id: "run-fifo".into(),
        thread_id: "thread".into(),
        model: "m".into(),
        partial_text: "partial".into(),
        partial_tools: Vec::new(),
        retry_count: 0,
    };
    let (beat, heartbeat) = mpsc::channel();
    let writer = std::thread::spawn({
        let bytes = serde_json::to_vec(&checkpoint).unwrap();
        move || {
            let ran = heartbeat.recv_timeout(PARK_LIMIT).is_ok();
            let bytes = if ran { bytes } else { b"inline".to_vec() };
            std::fs::write(&fifo, bytes).unwrap();
        }
    });
    let other = tokio::spawn(async move { beat.send(()).unwrap() });

    let read = store.get("run-fifo").await;

    writer.join().unwrap();
    other.await.unwrap();
    match read {
        Observed::Present(read) => assert_eq!(read, checkpoint, "E2"),
        unexpected => panic!("E1 the read held the worker: {unexpected:?}"),
    }
}
