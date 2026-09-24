use std::fs::OpenOptions;
use std::io::Write as _;

use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason, ResumeTicket};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::draft::Draft;
use awaken_agent_contract::audit::kind::Kind;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, OperationCoordinator};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitPayloadHash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_store_fs::FsCommitCoordinator;

const LOG: &str = "commits.ndjson";
const VERSION: &str = "commits.version";

fn ended_commit(thread: &str, run: &str) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId(thread.into()),
        run: RunDisposition::ended(RunId(run.into()), EndCause::NaturalEnd),
        messages: Vec::new(),
        state: Vec::new(),
        events: vec![Draft {
            kind: Kind::RunStateChanged,
            payload: serde_json::Value::Null,
        }],
    }
}

fn awaiting_commit(thread: &str, run: &str) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId(thread.into()),
        run: RunDisposition::awaiting(ResumeTicket::new(
            format!("correlation-{run}"),
            RunId(run.into()),
            ThreadId(thread.into()),
            format!("snapshot-{run}"),
            format!("fingerprint-{run}"),
            AwaitTarget::Pause(PauseReason::Manual),
        )),
        messages: Vec::new(),
        state: Vec::new(),
        events: vec![Draft {
            kind: Kind::RunAwaiting,
            payload: serde_json::Value::Null,
        }],
    }
}

async fn current_fixture(dir: &std::path::Path) -> String {
    let store = FsCommitCoordinator::open(dir).await.expect("open fixture");
    store
        .commit(ended_commit("direct-thread", "direct-run"))
        .await
        .expect("direct commit");
    let nested = awaiting_commit("operation-thread", "operation-run");
    store
        .commit_operation(CommitOperation {
            operation_id: CommitOperationId::new(RunId("operation-run".into()), 0),
            expected_thread_version: 0,
            payload_hash: CommitPayloadHash("fixture-hash".into()),
            commit: nested,
        })
        .await
        .expect("nested operation");
    drop(store);
    String::from_utf8(std::fs::read(dir.join(LOG)).expect("read fixture log"))
        .expect("fixture is utf8")
}

fn make_historical(dir: &std::path::Path, current: &str) -> String {
    let historical = current
        .replace("RunStateChanged", "RunPhaseChanged")
        .replace("RunAwaiting", "RunWaiting")
        .replace("\"resume_ticket\"", "\"waiting\"");
    std::fs::write(dir.join(LOG), historical.as_bytes()).expect("write historical log");
    std::fs::remove_file(dir.join(VERSION)).expect("remove version marker");
    historical
}

fn assert_current_log(log: &str) {
    assert!(log.contains("RunStateChanged"));
    assert!(log.contains("RunAwaiting"));
    assert!(log.contains("\"resume_ticket\""));
    assert!(!log.contains("RunPhaseChanged"));
    assert!(!log.contains("RunWaiting"));
    assert!(!log.contains("\"waiting\""));
    assert_eq!(log.lines().count(), 2, "record sequence is preserved");
}

#[tokio::test]
async fn old_and_nested_wire_shapes_migrate_to_the_current_log_version() {
    // Cause/effect decision table: C1 an unversioned log contains old Audit Kind
    // values; C2 the old `waiting` field is nested in CommitOperation; C3 every
    // record is newline-complete. R1=C1+C2+C3 -> E1 structurally normalize at
    // the one migration boundary before decoding through the current-only commit
    // contract, E2 atomically publish only current spellings, E3 preserve record
    // order and replayed state. A second domain decoder would violate E1.
    let dir = tempfile::tempdir().unwrap();
    let current = current_fixture(dir.path()).await;
    let historical = make_historical(dir.path(), &current);
    assert!(historical.contains("RunPhaseChanged"));
    assert!(historical.contains("RunWaiting"));
    assert!(historical.contains("\"waiting\""));

    let migrated = FsCommitCoordinator::open(dir.path())
        .await
        .expect("migrate historical log");
    assert_eq!(
        migrated
            .list_events(&EventScope::Run(RunId("direct-run".into())), None, 10)
            .await
            .expect("events")[0]
            .kind,
        Kind::RunStateChanged,
        "R1/E3 direct commit"
    );
    assert!(
        migrated
            .resume_ticket(&RunId("operation-run".into()))
            .is_some(),
        "R1/E3 nested operation"
    );
    drop(migrated);

    assert_eq!(std::fs::read(dir.path().join(VERSION)).unwrap(), b"2\n");
    assert_current_log(&std::fs::read_to_string(dir.path().join(LOG)).unwrap());
}

#[tokio::test]
async fn current_mixed_and_repeated_upgrade_are_idempotent() {
    // Causes: C1 current version marker present; C2 unversioned mixed old/current
    // records; C3 an already canonical log exists without its marker (crash after
    // log rename). Effects: E1 current is byte-preserving; E2 mixed becomes wholly
    // current; E3 C3 and every later reopen are no-ops in domain state.
    // Decision table: R1=C1->E1; R2=C2->E2; R3=C3->E3; R4=R2+reopen->E3.
    let current_dir = tempfile::tempdir().unwrap();
    let current = current_fixture(current_dir.path()).await;
    let reopened = FsCommitCoordinator::open(current_dir.path()).await.unwrap();
    drop(reopened);
    assert_eq!(
        std::fs::read_to_string(current_dir.path().join(LOG)).unwrap(),
        current,
        "R1/E1"
    );

    let mixed_dir = tempfile::tempdir().unwrap();
    let current = current_fixture(mixed_dir.path()).await;
    let mixed = current
        .replacen("RunStateChanged", "RunPhaseChanged", 1)
        .replacen("\"resume_ticket\"", "\"waiting\"", 1);
    std::fs::write(mixed_dir.path().join(LOG), mixed).unwrap();
    std::fs::remove_file(mixed_dir.path().join(VERSION)).unwrap();
    let migrated = FsCommitCoordinator::open(mixed_dir.path()).await.unwrap();
    drop(migrated);
    let once = std::fs::read_to_string(mixed_dir.path().join(LOG)).unwrap();
    assert_current_log(&once);
    let reopened = FsCommitCoordinator::open(mixed_dir.path()).await.unwrap();
    drop(reopened);
    assert_eq!(
        std::fs::read_to_string(mixed_dir.path().join(LOG)).unwrap(),
        once,
        "R4/E3"
    );

    // R3 models the exact durable state after log rename and before version
    // publication. Reapplying the decoder/encoder is safe and publishes marker.
    std::fs::remove_file(mixed_dir.path().join(VERSION)).unwrap();
    let recovered = FsCommitCoordinator::open(mixed_dir.path()).await.unwrap();
    drop(recovered);
    assert_current_log(&std::fs::read_to_string(mixed_dir.path().join(LOG)).unwrap());
}

#[tokio::test]
async fn phase_one_marker_upgrades_after_current_only_validation() {
    // Causes: C1 phase 1 left a v1 marker and canonical log; C2 the phase 2
    // decoder accepts every record without retired aliases; C3 a contradictory
    // v1 log still contains a retired name. Effects: E1 the valid log is
    // atomically revalidated and remains semantically identical; E2 its marker
    // advances to v2 only after E1; E3 C3 fails without changing either file.
    // Decision table: R1=C1+C2->E1+E2; R2=C1+C3->E3.
    let dir = tempfile::tempdir().unwrap();
    let current = current_fixture(dir.path()).await;
    std::fs::write(dir.path().join(VERSION), b"1\n").unwrap();

    let upgraded = FsCommitCoordinator::open(dir.path()).await.unwrap();
    drop(upgraded);
    assert_eq!(
        std::fs::read(dir.path().join(VERSION)).unwrap(),
        b"2\n",
        "R1/E2"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join(LOG)).unwrap(),
        current,
        "R1/E1"
    );

    let contradictory = tempfile::tempdir().unwrap();
    let current = current_fixture(contradictory.path()).await;
    let historical = current.replace("RunStateChanged", "RunPhaseChanged");
    std::fs::write(contradictory.path().join(LOG), historical.as_bytes()).unwrap();
    std::fs::write(contradictory.path().join(VERSION), b"1\n").unwrap();
    let error = FsCommitCoordinator::open(contradictory.path())
        .await
        .err()
        .expect("R2/E3");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read(contradictory.path().join(LOG)).unwrap(),
        historical.as_bytes(),
        "R2/E3"
    );
    assert_eq!(
        std::fs::read(contradictory.path().join(VERSION)).unwrap(),
        b"1\n",
        "R2/E3"
    );
}

#[tokio::test]
async fn corruption_and_torn_tail_have_distinct_migration_outcomes() {
    // Causes: C1 invalid newline-terminated record; C2 unterminated final bytes;
    // C3 either follows a valid historical prefix. Effects: E1 C1 fails closed
    // without changing log/version; E2 C2 is discarded and the valid prefix is
    // migrated. Decision table: R1=C1+C3->E1; R2=C2+C3->E2. Treating both as a
    // torn tail would silently erase durable corruption.
    let corrupt_dir = tempfile::tempdir().unwrap();
    let current = current_fixture(corrupt_dir.path()).await;
    let historical = make_historical(corrupt_dir.path(), &current);
    let corrupt = format!("{historical}{{not-json}}\n");
    std::fs::write(corrupt_dir.path().join(LOG), corrupt.as_bytes()).unwrap();
    let error = FsCommitCoordinator::open(corrupt_dir.path())
        .await
        .err()
        .expect("R1/E1");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read(corrupt_dir.path().join(LOG)).unwrap(),
        corrupt.as_bytes()
    );
    assert!(!corrupt_dir.path().join(VERSION).exists());

    let torn_dir = tempfile::tempdir().unwrap();
    let current = current_fixture(torn_dir.path()).await;
    make_historical(torn_dir.path(), &current);
    let mut log = OpenOptions::new()
        .append(true)
        .open(torn_dir.path().join(LOG))
        .unwrap();
    log.write_all(b"{\"torn\":").unwrap();
    drop(log);
    let migrated = FsCommitCoordinator::open(torn_dir.path())
        .await
        .expect("R2/E2");
    drop(migrated);
    let migrated = std::fs::read_to_string(torn_dir.path().join(LOG)).unwrap();
    assert_current_log(&migrated);
    assert!(!migrated.contains("torn"));
}

#[tokio::test]
async fn lifecycle_lock_excludes_another_coordinator_and_releases_on_drop() {
    // Causes: C1 one live coordinator owns the directory lock; C2 it is dropped.
    // Effects: E1 a competing open fails with WouldBlock; E2 a later open succeeds.
    // Decision table: R1=C1->E1; R2=C2->E2. The guard lifetime therefore owns
    // migration, replay, and every append instead of protecting migration alone.
    let dir = tempfile::tempdir().unwrap();
    let owner = FsCommitCoordinator::open(dir.path()).await.unwrap();
    let conflict = FsCommitCoordinator::open(dir.path())
        .await
        .err()
        .expect("R1/E1");
    assert_eq!(conflict.kind(), std::io::ErrorKind::WouldBlock);
    drop(owner);
    FsCommitCoordinator::open(dir.path()).await.expect("R2/E2");
}

#[tokio::test]
async fn stale_private_stage_cannot_replace_the_authoritative_log() {
    // Causes: C1 a crash leaves an unrelated private stage; C2 the authoritative
    // unversioned log is still old and complete. Effect: E1 open reads/migrates
    // only commits.ndjson and never promotes the stale stage. Decision table:
    // R1=C1+C2->E1. This is the pre-rename half of the atomic crash boundary;
    // the post-rename half is R3 in the idempotency test above.
    let dir = tempfile::tempdir().unwrap();
    let current = current_fixture(dir.path()).await;
    make_historical(dir.path(), &current);
    std::fs::write(
        dir.path().join(".commits.ndjson.awaken-file-stage-stale"),
        b"not authoritative",
    )
    .unwrap();
    let migrated = FsCommitCoordinator::open(dir.path()).await.unwrap();
    drop(migrated);
    assert_current_log(&std::fs::read_to_string(dir.path().join(LOG)).unwrap());
}

#[tokio::test]
async fn a_version_marker_can_never_substitute_for_missing_log_data() {
    // Causes: C1 a current marker exists; C2 a previous marker exists; C3 the
    // authoritative log is absent. Effect: E1 either recognized marker fails
    // closed instead of constructing an empty history. Decision table:
    // R1=C1+C3->E1; R2=C2+C3->E1. The valid publication protocol cannot create
    // either order, but rejecting both prevents deletion/tampering from looking
    // like a successfully migrated empty store.
    for (rule, version) in [("R1", b"2\n".as_slice()), ("R2", b"1\n".as_slice())] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(VERSION), version).unwrap();
        let error = FsCommitCoordinator::open(dir.path())
            .await
            .err()
            .expect("E1");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{rule}/E1");
        assert!(!dir.path().join(LOG).exists(), "{rule}/E1");
    }
}
