// @formal surface exclude=test_only
// @formal owner crates/resources/awaken-memory-store/src/sqlite.rs
// @formal reason: Kill an owned native child inside the production transaction or after its acknowledgement, then reopen the real database and compare content, history, attribution and conflicts.
//! Native process interruption at the actual Memory mutation boundary.

use super::*;
use awaken_resource_contract::MemoryCopyConflict;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    heads: Vec<Memory>,
    versions: Vec<MemoryVersion>,
    conflicts: Vec<MemoryCopyConflict>,
    neighbor: Vec<Memory>,
    neighbor_versions: Vec<MemoryVersion>,
}

async fn snapshot(repository: &SqliteMemoryRepository) -> Snapshot {
    Snapshot {
        heads: repository.snapshot_heads("s").await.unwrap(),
        versions: repository.list_versions("s").await.unwrap(),
        conflicts: repository.list_copy_conflicts("s").await.unwrap(),
        neighbor: repository.snapshot_heads("neighbor").await.unwrap(),
        neighbor_versions: repository.list_versions("neighbor").await.unwrap(),
    }
}

fn actor() -> MemoryActor {
    MemoryActor::ApiActor {
        api_key_id: "transaction-crash-test".into(),
    }
}

async fn mutate(repository: &SqliteMemoryRepository, operation: &str) {
    match operation {
        "update" => {
            let base = MemoryQueries::get_by_path(repository, "s", "/from.md")
                .await
                .unwrap()
                .unwrap();
            MemoryWriter::update_head_as(
                repository,
                "s",
                &base.generation(),
                "changed",
                None,
                Some(&actor()),
            )
            .await
            .unwrap();
        }
        "rename" => {
            repository
                .rename_as("s", "/from.md", "/to.md", None)
                .await
                .unwrap();
        }
        "redact" => {
            let version = repository.list_versions("s").await.unwrap().remove(0);
            repository
                .redact_version_as("s", &version.id, Some(&actor()))
                .await
                .unwrap()
                .unwrap();
        }
        "purge" => {
            repository.purge_store("s").await.unwrap();
        }
        _ => panic!("unknown interruption operation"),
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

fn blocked(marker: &Path) {
    std::fs::write(marker, b"production mutation reached").unwrap();
    // awaken-allow(wait): this child is deliberately blocked at an exact fault
    // cut; the parent retains its process handle and kills/reaps it on every exit.
    loop {
        std::thread::park();
    }
}

fn await_cut(child: &mut OwnedChild, marker: &Path, diagnostics: &Path) {
    let deadline = Instant::now() + Duration::from_secs(45);
    // awaken-allow(wait): bounded native child readiness, not a production retry.
    while std::fs::read(marker).ok().as_deref() != Some(b"production mutation reached") {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before cut: {}",
            std::fs::read_to_string(diagnostics).unwrap_or_default()
        );
        assert!(
            Instant::now() < deadline,
            "owned child never reached its production fault cut"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "cut must precede process termination"
    );
}

fn assert_committed(operation: &str, original: &Snapshot, current: &Snapshot) {
    assert_eq!(current.neighbor, original.neighbor);
    assert_eq!(current.neighbor_versions, original.neighbor_versions);
    match operation {
        "update" => {
            assert_eq!(current.heads.len(), 2);
            assert_eq!(current.heads[0].id, original.heads[0].id);
            assert_eq!(current.heads[0].version, original.heads[0].version + 1);
            assert_eq!(current.heads[0].content.as_deref(), Some("changed"));
            assert_eq!(current.heads[1], original.heads[1]);
            assert_eq!(current.versions.len(), original.versions.len() + 1);
            assert_eq!(current.versions.last().unwrap().created_by, Some(actor()));
            assert_eq!(&current.versions[..2], original.versions.as_slice());
            assert_eq!(current.conflicts, original.conflicts);
        }
        "rename" => {
            assert_eq!(current.heads.len(), 1);
            assert_eq!(current.heads[0].id, original.heads[0].id);
            assert_eq!(current.heads[0].path, "/to.md");
            assert_eq!(current.heads[0].content, original.heads[0].content);
            assert_eq!(current.versions.len(), original.versions.len() + 2);
            assert_eq!(current.versions[2].memory_id, original.heads[1].id);
            assert_eq!(
                current.versions[2].operation,
                MemoryVersionOperation::Deleted
            );
            assert_eq!(current.versions[3].memory_id, original.heads[0].id);
            assert_eq!(
                current.versions[3].operation,
                MemoryVersionOperation::Modified
            );
            assert_eq!(&current.versions[..2], original.versions.as_slice());
            assert_eq!(current.conflicts, original.conflicts);
        }
        "redact" => {
            assert_eq!(current.heads, original.heads);
            assert_eq!(current.versions.len(), original.versions.len());
            let redacted = &current.versions[0];
            assert_eq!(redacted.id, original.versions[0].id);
            assert_eq!(redacted.created_by, original.versions[0].created_by);
            assert_eq!(redacted.redacted_by, Some(actor()));
            assert!(redacted.content.is_none());
            assert!(redacted.redacted_unix_nanos.is_some());
            assert_eq!(current.versions[1], original.versions[1]);
            assert_eq!(current.conflicts, original.conflicts);
        }
        "purge" => {
            assert!(current.heads.is_empty());
            assert!(current.versions.is_empty());
            assert!(current.conflicts.is_empty());
        }
        _ => unreachable!(),
    }
}

async fn interrupted_row(operation: &str, cut: &str) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("memory.db");
    let repository =
        SqliteMemoryRepository::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap();
    MemoryWriter::create_as(&repository, "s", "/from.md", "source", Some(&actor()))
        .await
        .unwrap();
    repository
        .create("s", "/to.md", "destination")
        .await
        .unwrap();
    repository
        .create("neighbor", "/from.md", "untouched")
        .await
        .unwrap();
    repository
        .preserve_copy_conflict(
            "s",
            &MemoryCopyConflict {
                path: "/from.md".into(),
                base: None,
                content: Some("losing proposal".into()),
            },
        )
        .await
        .unwrap();
    let original = snapshot(&repository).await;
    let marker = root.path().join("cut");
    let diagnostics = root.path().join("child.stderr");
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sqlite::transaction_crash_tests::transaction_interruption_child",
                "--ignored",
                "--nocapture",
            ])
            .env("AWAKEN_MEMORY_CRASH_DATABASE", &path)
            .env("AWAKEN_MEMORY_CRASH_MARKER", &marker)
            .env("AWAKEN_MEMORY_CRASH_OPERATION", operation)
            .env("AWAKEN_MEMORY_CRASH_CUT", cut)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&diagnostics).unwrap())
            .spawn()
            .unwrap(),
    );
    await_cut(&mut child, &marker, &diagnostics);
    let observed = snapshot(&repository).await;
    if cut == "before-commit" {
        assert_eq!(
            observed, original,
            "uncommitted prefix must be invisible: {operation}"
        );
    } else {
        assert_committed(operation, &original, &observed);
    }
    repository.pool.close().await;
    child.0.kill().unwrap();
    assert!(
        !child.0.wait().unwrap().success(),
        "a normal exit is not process interruption"
    );
    let reopened =
        SqliteMemoryRepository::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap();
    assert_eq!(
        snapshot(&reopened).await,
        observed,
        "reopen must retain exactly the committed prefix: {operation}/{cut}"
    );
    if cut == "before-commit" {
        mutate(&reopened, operation).await;
        assert_committed(operation, &original, &snapshot(&reopened).await);
    }
    if operation == "redact" {
        let before_replay = snapshot(&reopened).await;
        mutate(&reopened, operation).await;
        assert_eq!(
            snapshot(&reopened).await,
            before_replay,
            "replay must not restore content or rewrite attribution"
        );
    }
    if operation == "purge" {
        let replay = reopened.purge_store("s").await.unwrap();
        assert_eq!(
            (
                replay.heads_deleted,
                replay.versions_deleted,
                replay.copy_conflicts_deleted
            ),
            (0, 0, 0)
        );
    }
    reopened.pool.close().await;
}

#[tokio::test]
async fn process_death_inside_mutation_rolls_back_heads_history_and_conflicts() {
    for operation in ["update", "rename", "redact", "purge"] {
        interrupted_row(operation, "before-commit").await;
    }
}

#[tokio::test]
async fn process_death_after_acknowledgement_preserves_committed_content_and_redaction() {
    for operation in ["update", "rename", "redact", "purge"] {
        interrupted_row(operation, "after-commit").await;
    }
}

#[tokio::test]
#[ignore = "owned child invoked and killed by the two process_death tests"]
async fn transaction_interruption_child() {
    let database = std::env::var("AWAKEN_MEMORY_CRASH_DATABASE").unwrap();
    let marker = std::path::PathBuf::from(std::env::var_os("AWAKEN_MEMORY_CRASH_MARKER").unwrap());
    let operation = std::env::var("AWAKEN_MEMORY_CRASH_OPERATION").unwrap();
    let cut = std::env::var("AWAKEN_MEMORY_CRASH_CUT").unwrap();
    let repository = SqliteMemoryRepository::open(&database, awaken_time::system_clock())
        .await
        .unwrap();
    if cut == "before-commit" {
        use sqlx::sqlite::SqliteOperation;
        let (change, table, ordinal) = match operation.as_str() {
            "update" => (SqliteOperation::Insert, "memory_store_versions", 1),
            "rename" => (SqliteOperation::Insert, "memory_store_versions", 2),
            "redact" => (SqliteOperation::Update, "memory_store_versions", 1),
            "purge" => (SqliteOperation::Delete, "memory_store_memories", 1),
            _ => panic!("unknown production cut"),
        };
        let remaining = AtomicUsize::new(ordinal);
        tokio::time::timeout(Duration::from_secs(30), repository.pool.acquire())
            .await
            .expect("the crash child acquires its production connection")
            .unwrap()
            .lock_handle()
            .await
            .unwrap()
            .set_update_hook(move |observed| {
                if observed.operation == change
                    && observed.table == table
                    && remaining.fetch_sub(1, Ordering::SeqCst) == 1
                {
                    blocked(&marker);
                }
            });
    }
    mutate(&repository, &operation).await;
    assert_eq!(
        cut, "after-commit",
        "production mutation missed its precommit fault cut"
    );
    blocked(Path::new(
        &std::env::var_os("AWAKEN_MEMORY_CRASH_MARKER").unwrap(),
    ));
}
