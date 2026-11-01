//! T22-C01: authored Memory writes keep their author through the production
//! composition (the Resources-owned `InvalidatingMemoryRepository` decorator
//! over each real backend), across a file-SQLite reopen and across two
//! independent PostgreSQL pools.
//!
//! The decorator is never bypassed: every write goes through
//! `ResourceAuthorities::memory_writer()` of an application built by
//! `open_embedded` / `open_postgres`, and every observation is read back from a
//! second application instance.

use std::sync::{Arc, Mutex};

use awaken_resource_contract::{
    Memory, MemoryActor, MemoryInvalidator, MemoryVersion, MemoryVersionOperation, MemoryWriter,
};
use awaken_resource_persistence::{SchemaMode, open_embedded, open_postgres};

#[derive(Default)]
struct RecordedInvalidations(Mutex<Vec<(String, String)>>);

impl MemoryInvalidator for RecordedInvalidations {
    fn publish(&self, store: &str, path: &str) {
        self.0.lock().unwrap().push((store.into(), path.into()));
    }
}

fn user(id: &str) -> MemoryActor {
    MemoryActor::UserActor { user_id: id.into() }
}

#[derive(Clone, Copy)]
enum Forwarding {
    Stated,
    Dropped,
}

impl Forwarding {
    fn actor(self, id: &str) -> Option<MemoryActor> {
        match self {
            Self::Stated => Some(user(id)),
            Self::Dropped => None,
        }
    }
}

fn unique_store(label: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock follows the Unix epoch")
        .as_nanos();
    format!("{label}-{}-{nonce:x}", std::process::id())
}

/// The authored script: create (alice), content update (bob), same-content
/// no-op (carol), target create (alice), path-changing overwrite onto that
/// target (bob), delete (alice). Returns the moved head's identity.
///
/// `Forwarding::Dropped` is the negative-control mutant of the forwarding
/// path: the same calls reach the same composed writer with the author removed
/// (the shape the decorator had before attribution forwarding).
async fn authored_script(
    writer: &dyn MemoryWriter,
    filesystem: &dyn awaken_resource_contract::MemoryFilesystem,
    versions: &dyn awaken_resource_contract::MemoryVersionQueries,
    store: &str,
    forwarding: Forwarding,
) -> Memory {
    let alice = forwarding.actor("alice");
    let bob = forwarding.actor("bob");
    let carol = forwarding.actor("carol");
    let created = writer
        .create_as(store, "/a.md", "one", alice.as_ref())
        .await
        .unwrap();
    let updated = writer
        .update_head_as(store, &created.generation(), "two", None, bob.as_ref())
        .await
        .unwrap();

    let before_noop = versions.list_versions(store).await.unwrap().len();
    let noop = writer
        .update_head_as(store, &updated.generation(), "two", None, carol.as_ref())
        .await
        .unwrap();
    assert_eq!(noop.version, updated.version, "O2 no-op keeps the version");
    assert_eq!(
        versions.list_versions(store).await.unwrap().len(),
        before_noop,
        "O2 no-op appends no history"
    );

    writer
        .create_as(store, "/b.md", "target", alice.as_ref())
        .await
        .unwrap();
    let moved = writer
        .update_head_as(
            store,
            &updated.generation(),
            "three",
            Some("/b.md"),
            bob.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(
        moved.id, created.id,
        "O0 the overwrite keeps the source identity"
    );
    assert_eq!(moved.path, "/b.md", "O0");
    writer
        .delete_by_path_as(store, "/b.md", alice.as_ref())
        .await
        .unwrap();
    // RA1 separate from update_head_as: the mounted-filesystem operation must
    // preserve the caller Bob through the same production decorator and atomic
    // replace; RA2 same-path Carol appends nothing, so the existing no-Carol
    // oracle also covers rename no-op. Both durable backend scripts reuse this.
    let source = writer
        .create_as(store, "/rename-source.md", "rename-source", alice.as_ref())
        .await
        .unwrap();
    writer
        .create_as(store, "/rename-target.md", "rename-target", alice.as_ref())
        .await
        .unwrap();
    let renamed = filesystem
        .rename_as(
            store,
            "/rename-source.md",
            "/rename-target.md",
            bob.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(renamed.id, source.id, "RA1 source identity");
    let before_noop = versions.list_versions(store).await.unwrap();
    filesystem
        .rename_as(
            store,
            "/rename-target.md",
            "/rename-target.md",
            carol.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(
        versions.list_versions(store).await.unwrap(),
        before_noop,
        "RA2 no-op history"
    );
    writer
        .delete_by_path_as(store, "/rename-target.md", alice.as_ref())
        .await
        .unwrap();
    moved
}

fn find<'a>(
    versions: &'a [MemoryVersion],
    operation: MemoryVersionOperation,
    path: &str,
    content: Option<&str>,
) -> Vec<&'a MemoryVersion> {
    versions
        .iter()
        .filter(|version| {
            version.operation == operation
                && version.path == path
                && (content.is_none() || version.content.as_deref() == content)
        })
        .collect()
}

/// The oracle check, applied to the history read from an independent instance.
fn attribution_is_complete(versions: &[MemoryVersion], moved: &Memory) -> Result<(), String> {
    if let Some(lost) = versions.iter().find(|version| version.created_by.is_none()) {
        return Err(format!(
            "O1 {:?} at {} lost its author",
            lost.operation, lost.path
        ));
    }
    if versions
        .iter()
        .any(|version| version.created_by == Some(user("carol")))
    {
        return Err("O2 the no-op recorded a version".into());
    }
    let expect = |found: Vec<&MemoryVersion>, author: &str, what: &str| -> Result<(), String> {
        if found
            .iter()
            .any(|version| version.created_by == Some(user(author)))
        {
            Ok(())
        } else {
            Err(format!(
                "O1 {what} is not attributed to {author}: {found:?}"
            ))
        }
    };
    expect(
        find(
            versions,
            MemoryVersionOperation::Created,
            "/a.md",
            Some("one"),
        ),
        "alice",
        "create",
    )?;
    expect(
        find(
            versions,
            MemoryVersionOperation::Modified,
            "/a.md",
            Some("two"),
        ),
        "bob",
        "content update",
    )?;
    let overwrite = find(
        versions,
        MemoryVersionOperation::Modified,
        "/b.md",
        Some("three"),
    );
    if !overwrite
        .iter()
        .any(|version| version.memory_id == moved.id)
    {
        return Err(format!(
            "O0 the overwrite is not the source identity: {overwrite:?}"
        ));
    }
    expect(overwrite, "bob", "path-changing overwrite")?;
    expect(
        find(versions, MemoryVersionOperation::Deleted, "/b.md", None),
        "alice",
        "delete",
    )?;
    expect(
        find(
            versions,
            MemoryVersionOperation::Deleted,
            "/rename-target.md",
            None,
        ),
        "bob",
        "filesystem displaced deletion",
    )?;
    expect(
        find(
            versions,
            MemoryVersionOperation::Modified,
            "/rename-target.md",
            Some("rename-source"),
        ),
        "bob",
        "filesystem moved head",
    )?;
    Ok(())
}

/// File SQLite: the script runs through the composed decorator, the history is
/// read after a full reopen, a redaction keeps both authors, and the
/// actor-dropping mutant is detected by the same check.
#[tokio::test(flavor = "multi_thread")]
async fn sqlite_decorated_writes_keep_every_author_across_reopen() {
    let root = tempfile::tempdir().unwrap();
    let events = Arc::new(RecordedInvalidations::default());
    let store = unique_store("attribution-sqlite");
    let moved = {
        let application = open_embedded(root.path(), awaken_time::system_clock(), events.clone())
            .await
            .unwrap();
        let authorities = application.authorities();
        authored_script(
            authorities.memory_writer().as_ref(),
            authorities.memory_filesystem().as_ref(),
            authorities.memory_versions().as_ref(),
            &store,
            Forwarding::Stated,
        )
        .await
    };
    assert!(
        !events.0.lock().unwrap().is_empty(),
        "the writes went through the invalidating decorator"
    );

    let reopened = open_embedded(root.path(), awaken_time::system_clock(), events.clone())
        .await
        .unwrap();
    let authorities = reopened.authorities();
    let versions = authorities
        .memory_versions()
        .list_versions(&store)
        .await
        .unwrap();
    attribution_is_complete(&versions, &moved).expect("O0-O2 after reopen");
    assert!(
        authorities
            .memory_snapshot_source()
            .snapshot_heads(&store)
            .await
            .unwrap()
            .is_empty(),
        "O0 the overwrite consumed the target and the delete removed it"
    );

    let first = find(
        &versions,
        MemoryVersionOperation::Created,
        "/a.md",
        Some("one"),
    )[0]
    .clone();
    let redacted = authorities
        .memory_version_redaction()
        .redact_version_as(&store, &first.id, Some(&user("dave")))
        .await
        .unwrap()
        .expect("O1 redaction target exists");
    assert_eq!(
        redacted.created_by,
        Some(user("alice")),
        "O1 created_by survives"
    );
    assert_eq!(
        redacted.redacted_by,
        Some(user("dave")),
        "O1 redacted_by recorded"
    );
    assert_eq!(redacted.content, None, "O1 content cleared");

    // Negative control: the same composition and check with the author dropped.
    let mutant_store = unique_store("attribution-mutant");
    let mutant_moved = authored_script(
        authorities.memory_writer().as_ref(),
        authorities.memory_filesystem().as_ref(),
        authorities.memory_versions().as_ref(),
        &mutant_store,
        Forwarding::Dropped,
    )
    .await;
    let mutant_versions = authorities
        .memory_versions()
        .list_versions(&mutant_store)
        .await
        .unwrap();
    assert!(
        attribution_is_complete(&mutant_versions, &mutant_moved).is_err(),
        "NC the attribution check detects a decorator that drops the author"
    );
}

/// PostgreSQL: the script runs through one composed application (pool A); the
/// history and redaction are observed through an independently connected
/// application (pool B).
#[tokio::test(flavor = "multi_thread")]
async fn postgres_decorated_writes_keep_every_author_across_independent_pools() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let events = Arc::new(RecordedInvalidations::default());
    let store = unique_store("attribution-pg");
    let writer_app = open_postgres(
        &url,
        SchemaMode::Migrate,
        awaken_time::system_clock(),
        events.clone(),
    )
    .await
    .expect("open PostgreSQL Resources (pool A)");
    let reader_app = open_postgres(
        &url,
        SchemaMode::Verify,
        awaken_time::system_clock(),
        events.clone(),
    )
    .await
    .expect("open PostgreSQL Resources (pool B)");
    let writer = writer_app.authorities();
    let moved = authored_script(
        writer.memory_writer().as_ref(),
        writer.memory_filesystem().as_ref(),
        writer.memory_versions().as_ref(),
        &store,
        Forwarding::Stated,
    )
    .await;

    let reader = reader_app.authorities();
    let versions = reader
        .memory_versions()
        .list_versions(&store)
        .await
        .unwrap();
    attribution_is_complete(&versions, &moved).expect("O0-O2 through pool B");
    let first = find(
        &versions,
        MemoryVersionOperation::Created,
        "/a.md",
        Some("one"),
    )[0]
    .clone();
    let redacted = reader
        .memory_version_redaction()
        .redact_version_as(&store, &first.id, Some(&user("dave")))
        .await
        .unwrap()
        .expect("O1 redaction target exists");
    assert_eq!(redacted.created_by, Some(user("alice")), "O1");
    assert_eq!(redacted.redacted_by, Some(user("dave")), "O1");
    let observed_by_a = writer
        .memory_versions()
        .list_versions(&store)
        .await
        .unwrap();
    assert_eq!(
        observed_by_a
            .iter()
            .find(|version| version.id == first.id)
            .and_then(|version| version.redacted_by.clone()),
        Some(user("dave")),
        "O1 pool A observes pool B's redaction author"
    );
}

#[test]
fn postgres_attribution_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_decorated_writes_keep_every_author_across_independent_pools",
    );
}

/// Cause/effect RA1: Alice creates a head; Bob performs the filesystem rename
/// through the composed application. The Modified history must name Bob, not
/// infer Alice from the create row. The same original case now exercises the
/// actor-bearing canonical port; the durable reopen/pool cases cover retention.
#[tokio::test(flavor = "multi_thread")]
async fn sqlite_rename_history_records_an_author() {
    let root = tempfile::tempdir().unwrap();
    let store = unique_store("attribution-rename");
    let application = open_embedded(
        root.path(),
        awaken_time::system_clock(),
        Arc::new(RecordedInvalidations::default()),
    )
    .await
    .unwrap();
    let authorities = application.authorities();
    authorities
        .memory_writer()
        .create_as(&store, "/x.md", "body", Some(&user("alice")))
        .await
        .unwrap();
    authorities
        .memory_filesystem()
        .rename_as(&store, "/x.md", "/y.md", Some(&user("bob")))
        .await
        .unwrap();
    let versions = authorities
        .memory_versions()
        .list_versions(&store)
        .await
        .unwrap();
    let renamed = find(&versions, MemoryVersionOperation::Modified, "/y.md", None);
    assert!(!renamed.is_empty(), "the rename appended history");
    assert!(
        renamed.iter().all(|version| version.created_by.is_some()),
        "rename history must name its author: {renamed:?}"
    );
    assert!(
        renamed
            .iter()
            .all(|version| version.created_by == Some(user("bob"))),
        "RA1 explicit Bob, never inherited Alice"
    );
}
