// @formal surface exclude=test_only
// @formal owner crates/platform/session/awaken-session-store/src/sqlite.rs
// @formal reason: Kill an owned native child inside a production Session root write or after its acknowledgement, then reopen the real database and compare root, receipts and outbox.
//! T05-C03 (事务与进程死亡), file-SQLite rows. An owned child process runs a
//! PRODUCTION root write (`create` / `commit`) on a real database file; an
//! update hook on the store's single production connection parks the child
//! at the selected row write, inside the transaction, and the parent kills it.
//! The PostgreSQL rows are `tests/root_write_transaction_crash.rs`.
//!
//! | Row | Command | Cut | Expected after reopen |
//! |---|---|---|---|
//! | K1 | create (root + receipt + fact) | at the outbox INSERT (root and receipt written) | nothing: no root, receipt or fact; the retry applies once |
//! | K2 | commit Replace + fact | at the receipt INSERT (root updated) | the old root, no receipt, no fact; the retry applies once |
//! | K3 | commit Replace + fact | at the outbox INSERT | the same |
//! | A1/A2 | create / commit | after the acknowledgement | the complete new state; the retry replays it, no second fact |
//!
//! Oracles (groups-1): O0 the transaction rolls back entirely or exposes the
//! complete commit; O1 a restart only continues the original intent, with no
//! partial fact. Negative control (executed): the cut is the production
//! connection's own row write (the hook observed the exact table write inside
//! the production command) and the parent proves the uncommitted root/receipt
//! are invisible while the child is still alive at the cut.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionCreateResult,
    SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRepositoryError,
};
use awaken_session_contract::{LifecycleOutbox, SessionRepository, SessionRepositoryReads};
use awaken_session_types::{SessionId, SessionRevision};
use awaken_store_conformance::session_store::{fact, record, record_update, session};

use super::SqliteManagedSessionRepository;

const ID: &str = "sesn_t05_crash";
const CREATE_KEY: &str = "t05:create";
const REPLACE_KEY: &str = "t05:replace";
const CHILD: &str = "sqlite::transaction_crash_tests::root_write_crash_child";

fn create_command() -> (
    PersistedSession,
    IdempotencyRecord,
    Vec<ManagedLifecycleFact>,
) {
    let mut value = session(ID, "before");
    value.revision = SessionRevision(0);
    let payload = SessionMutationPayload::Replace(value.clone());
    (
        value,
        record(CREATE_KEY, &payload),
        vec![fact("t05-created", ID, "session.status_idled")],
    )
}

fn replace_command(created: &PersistedSession) -> SessionMutation {
    let mut candidate = created.clone();
    candidate.title = Some("after".into());
    let candidate = record_update(created, candidate, 0).into_snapshot();
    let payload = SessionMutationPayload::Replace(candidate);
    SessionMutation {
        expected_revision: created.revision,
        idempotency: record(REPLACE_KEY, &payload),
        payload,
        lifecycle_facts: vec![fact("t05-replaced", ID, "session.status_idled")],
    }
}

#[derive(Debug, PartialEq)]
struct Observed {
    root: Result<PersistedSession, SessionRepositoryError>,
    create_receipt: Option<SessionIdempotencyReceipt>,
    replace_receipt: Option<SessionIdempotencyReceipt>,
    pending: Vec<ManagedLifecycleFact>,
}

async fn observe(repository: &SqliteManagedSessionRepository) -> Observed {
    let id = SessionId::from(ID);
    Observed {
        root: repository.get(&id).await,
        create_receipt: repository
            .idempotency_receipt(&id, CREATE_KEY)
            .await
            .unwrap(),
        replace_receipt: repository
            .idempotency_receipt(&id, REPLACE_KEY)
            .await
            .unwrap(),
        pending: repository.pending_lifecycle().await.unwrap(),
    }
}

/// The production command of a row; returns whether it applied (vs replayed).
/// `seeded` is the revision-one root a Replace is authored against, so the
/// child and the parent's retry send byte-identical commands.
async fn run(
    repository: &SqliteManagedSessionRepository,
    command: &str,
    seeded: Option<&PersistedSession>,
) -> bool {
    match command {
        "create" => {
            let (value, identity, facts) = create_command();
            match repository
                .create("ws_a", value, identity, facts)
                .await
                .unwrap()
            {
                SessionCreateResult::Applied(_) => true,
                SessionCreateResult::Replayed(_) => false,
            }
        }
        "replace" => {
            let mutation = replace_command(seeded.expect("a Replace row has a seeded root"));
            match repository.commit("ws_a", mutation).await.unwrap() {
                SessionMutationResult::Applied { .. } => true,
                SessionMutationResult::Replayed { .. } => false,
                other => panic!("replace answered {other:?}"),
            }
        }
        _ => panic!("unknown T05 command {command}"),
    }
}

/// Seed the replace rows: the created root with its fact retired.
async fn seed_created(repository: &SqliteManagedSessionRepository) -> PersistedSession {
    assert!(run(repository, "create", None).await, "seed create applies");
    repository.complete_lifecycle("t05-created").await.unwrap();
    repository.get(&SessionId::from(ID)).await.unwrap()
}

fn assert_committed(command: &str, original: &Observed, current: &Observed) {
    match command {
        "create" => {
            let root = current
                .root
                .as_ref()
                .expect("O0 the created root is visible");
            assert_eq!(root.revision, SessionRevision(1), "O0 revision one");
            assert_eq!(
                current
                    .create_receipt
                    .as_ref()
                    .map(|r| r.committed_revision),
                Some(SessionRevision(1)),
                "O0 the receipt committed with the root"
            );
            assert_eq!(
                current
                    .pending
                    .iter()
                    .map(|f| f.id.as_str())
                    .collect::<Vec<_>>(),
                ["t05-created"],
                "O1 exactly the command's fact"
            );
        }
        "replace" => {
            let root = current.root.as_ref().expect("root");
            assert_eq!(
                root.revision,
                SessionRevision(2),
                "O0 the replace committed"
            );
            assert_eq!(root.title.as_deref(), Some("after"), "O0 the new content");
            assert_eq!(
                current.create_receipt, original.create_receipt,
                "the create receipt is untouched"
            );
            assert_eq!(
                current
                    .replace_receipt
                    .as_ref()
                    .map(|r| r.committed_revision),
                Some(SessionRevision(2)),
                "O0 the receipt committed with the root"
            );
            assert_eq!(
                current
                    .pending
                    .iter()
                    .map(|f| f.id.as_str())
                    .collect::<Vec<_>>(),
                ["t05-replaced"],
                "O1 exactly the command's fact"
            );
        }
        _ => unreachable!(),
    }
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn blocked(marker: &Path, reached: &[u8]) {
    std::fs::write(marker, reached).unwrap();
    // awaken-allow(wait): this child is deliberately blocked at an exact fault
    // cut; the parent retains its process handle and kills/reaps it on every exit.
    loop {
        std::thread::park();
    }
}

fn await_marker(child: &mut OwnedChild, marker: &Path, diagnostics: &Path) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(45);
    // awaken-allow(wait): bounded native child readiness, not a production retry.
    loop {
        if let Ok(reached) = std::fs::read(marker)
            && !reached.is_empty()
        {
            return reached;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before its cut: {}",
            std::fs::read_to_string(diagnostics).unwrap_or_default()
        );
        assert!(
            Instant::now() < deadline,
            "owned child never reached its cut"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn interrupted_row(command: &str, cut: &str) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sessions.db");
    let repository = SqliteManagedSessionRepository::open(path.to_str().unwrap())
        .await
        .unwrap();
    let seeded = if command == "replace" {
        Some(seed_created(&repository).await)
    } else {
        None
    };
    let original = observe(&repository).await;
    let marker = root.path().join("cut");
    let diagnostics = root.path().join("child.stderr");
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("AWAKEN_T05_DATABASE", &path)
            .env("AWAKEN_T05_MARKER", &marker)
            .env("AWAKEN_T05_COMMAND", command)
            .env("AWAKEN_T05_CUT", cut)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&diagnostics).unwrap())
            .spawn()
            .unwrap(),
    );
    let reached = await_marker(&mut child, &marker, &diagnostics);
    let observed = observe(&repository).await;
    if cut == "after-commit" {
        assert_eq!(
            reached, b"acknowledged",
            "the child acknowledged its command"
        );
        assert_committed(command, &original, &observed);
    } else {
        assert_eq!(
            reached,
            format!("production write reached {cut}").as_bytes(),
            "NC: the cut is the production connection's own {cut} row write"
        );
        assert_eq!(
            observed, original,
            "O0 {command}/{cut}: the uncommitted prefix is invisible while the child is alive"
        );
    }
    drop(repository);
    child.0.kill().unwrap();
    assert!(
        !child.0.wait().unwrap().success(),
        "a normal exit is not process interruption"
    );

    let reopened = SqliteManagedSessionRepository::open(path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        observe(&reopened).await,
        observed,
        "O0 reopen retains exactly the committed prefix: {command}/{cut}"
    );
    // O1: the restart continues only the original intent.
    let applied = run(&reopened, command, seeded.as_ref()).await;
    assert_eq!(
        applied,
        cut != "after-commit",
        "O1 {command}/{cut}: the retry applies only if nothing committed"
    );
    assert_committed(command, &original, &observe(&reopened).await);
}

#[tokio::test]
async fn process_death_inside_a_root_write_rolls_back_root_receipt_and_outbox() {
    for (command, cut) in [
        ("create", "managed_lifecycle_outbox"),
        ("replace", "managed_session_idempotency"),
        ("replace", "managed_lifecycle_outbox"),
    ] {
        interrupted_row(command, cut).await;
    }
}

#[tokio::test]
async fn process_death_after_a_root_write_acknowledgement_keeps_the_complete_commit() {
    for command in ["create", "replace"] {
        interrupted_row(command, "after-commit").await;
    }
}

#[tokio::test]
#[ignore = "owned child invoked and killed by the two T05-C03 process-death tests"]
async fn root_write_crash_child() {
    let database = std::env::var("AWAKEN_T05_DATABASE").unwrap();
    let marker = std::path::PathBuf::from(std::env::var_os("AWAKEN_T05_MARKER").unwrap());
    let command = std::env::var("AWAKEN_T05_COMMAND").unwrap();
    let cut = std::env::var("AWAKEN_T05_CUT").unwrap();
    let repository = SqliteManagedSessionRepository::open(&database)
        .await
        .unwrap();
    if cut != "after-commit" {
        use sqlx::sqlite::SqliteOperation;
        let remaining = AtomicUsize::new(1);
        let reached = format!("production write reached {cut}").into_bytes();
        let hook_marker = marker.clone();
        tokio::time::timeout(Duration::from_secs(30), repository.pool.acquire())
            .await
            .expect("the crash child acquires its production connection")
            .unwrap()
            .lock_handle()
            .await
            .unwrap()
            .set_update_hook(move |observed| {
                if observed.operation == SqliteOperation::Insert
                    && observed.table == cut
                    && remaining.fetch_sub(1, Ordering::SeqCst) == 1
                {
                    blocked(&hook_marker, &reached);
                }
            });
    }
    let seeded = match command.as_str() {
        "replace" => Some(repository.get(&SessionId::from(ID)).await.unwrap()),
        _ => None,
    };
    run(&repository, &command, seeded.as_ref()).await;
    blocked(&marker, b"acknowledged");
}
