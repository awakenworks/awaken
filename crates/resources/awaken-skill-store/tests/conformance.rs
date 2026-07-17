//! Backend-generic conformance for `SkillStore`, run against every backend
//! (in-memory + filesystem + sqlite, and — when reachable — postgres), so all
//! backends keep identical semantics: sanitized ids, sorted + workspace-scoped
//! listing, and idempotent delete.

use awaken_skill_store::{FsSkillStore, InMemorySkillStore, SkillStore};
use std::sync::Arc;

/// A real, multiline `SKILL.md` body (YAML frontmatter + Markdown, blank lines,
/// trailing newline, unicode) round-trips through `put`/`get` byte-for-byte on every
/// backend — the store is opaque to content, so nothing is normalized or truncated.
async fn verbatim_skill_md_round_trips(store: &dyn SkillStore) {
    let body = "---\n\
name: Code Reviewer\n\
description: Reviews a diff for correctness and style.\n\
allowed-tools:\n\
  - read\n\
  - grep\n\
---\n\
\n\
# Code Reviewer\n\
\n\
Review the current diff. Focus on:\n\
\n\
1. **Correctness** — off-by-one, nil deref, races.\n\
2. Style — naming, dead code.\n\
\n\
> Note: keep findings terse. 你好, café — unicode survives.\n";
    let id = store.put("wsV", "code-reviewer", body).await.unwrap();
    assert_eq!(id, "code-reviewer");
    assert_eq!(
        store.get("wsV", "code-reviewer").await.unwrap().as_deref(),
        Some(body),
        "the SKILL.md body is stored and returned verbatim"
    );
    // It also lists with the same verbatim content.
    let listed = store.list("wsV").await.unwrap();
    assert_eq!(
        listed,
        vec![("code-reviewer".to_string(), body.to_string())]
    );
}

/// Concurrent `put`s of the SAME id resolve to exactly one surviving entry whose
/// content is one of the racers' bodies (last write wins), never a partial blend and
/// never a duplicate list row.
async fn concurrent_put_same_id_last_wins<S>(store: Arc<S>)
where
    S: SkillStore + Send + Sync + 'static,
{
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store.put("wsC", "dup", &format!("body-{i}")).await.unwrap()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), "dup", "every put addresses the same stem");
    }
    // Exactly one entry, and its content is one of the written bodies.
    let listed = store.list("wsC").await.unwrap();
    assert_eq!(
        listed.len(),
        1,
        "concurrent same-id puts collapse to one entry"
    );
    let survivor = store.get("wsC", "dup").await.unwrap().unwrap();
    assert!(
        (0..8u32).any(|i| survivor == format!("body-{i}")),
        "the survivor is one whole racer's body (last write wins), got {survivor:?}"
    );
    assert_eq!(listed[0].1, survivor, "list and get agree on the survivor");
}

#[tokio::test]
async fn in_memory_verbatim_and_concurrent() {
    verbatim_skill_md_round_trips(&InMemorySkillStore::new()).await;
    concurrent_put_same_id_last_wins(Arc::new(InMemorySkillStore::new())).await;
}

#[tokio::test]
async fn fs_verbatim_and_concurrent() {
    let dir = tempfile::tempdir().unwrap();
    verbatim_skill_md_round_trips(&FsSkillStore::open(dir.path()).unwrap()).await;
    let dir2 = tempfile::tempdir().unwrap();
    concurrent_put_same_id_last_wins(Arc::new(FsSkillStore::open(dir2.path()).unwrap())).await;
}

async fn put_get_list_delete_and_scope_by_workspace(store: &dyn SkillStore) {
    // put returns the sanitized id it is addressable by.
    assert_eq!(store.put("ws1", "greet", "HELLO").await.unwrap(), "greet");
    assert_eq!(
        store.put("ws1", "../etc/passwd", "x").await.unwrap(),
        "etc-passwd"
    );
    store.put("ws1", "review", "REVIEW").await.unwrap();
    // A different workspace is isolated.
    store.put("ws2", "greet", "HALLO").await.unwrap();

    assert_eq!(
        store.get("ws1", "greet").await.unwrap().as_deref(),
        Some("HELLO")
    );
    assert_eq!(
        store.get("ws2", "greet").await.unwrap().as_deref(),
        Some("HALLO")
    );
    assert_eq!(store.get("ws1", "missing").await.unwrap(), None);

    // list is sorted by id and scoped to the workspace.
    let ws1: Vec<String> = store
        .list("ws1")
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ws1, vec!["etc-passwd", "greet", "review"]);
    assert_eq!(store.list("ws2").await.unwrap().len(), 1);
    assert!(store.list("ws_absent").await.unwrap().is_empty());

    // delete reports prior existence and is idempotent; other workspaces untouched.
    assert!(store.delete("ws1", "greet").await.unwrap());
    assert!(!store.delete("ws1", "greet").await.unwrap());
    assert_eq!(store.get("ws1", "greet").await.unwrap(), None);
    assert_eq!(
        store.get("ws2", "greet").await.unwrap().as_deref(),
        Some("HALLO")
    );

    // sanitize fallbacks: an all-illegal id collapses to the "skill" stem; a very long
    // id is length-bounded. Both round-trip through get (same sanitize on both sides).
    assert_eq!(store.put("wsS", "***", "B").await.unwrap(), "skill");
    assert_eq!(
        store.get("wsS", "skill").await.unwrap().as_deref(),
        Some("B")
    );
    let long_id = store.put("wsS", &"a".repeat(300), "L").await.unwrap();
    assert!(long_id.len() <= 120, "the id stem is length-bounded");
    assert_eq!(
        store.get("wsS", &"a".repeat(300)).await.unwrap().as_deref(),
        Some("L")
    );

    // put overwrites in place: the same id twice keeps only the latest content and
    // produces no duplicate list entry.
    store.put("wsO", "dup", "first").await.unwrap();
    store.put("wsO", "dup", "second").await.unwrap();
    assert_eq!(
        store.get("wsO", "dup").await.unwrap().as_deref(),
        Some("second")
    );
    assert_eq!(store.list("wsO").await.unwrap().len(), 1);
}

/// Sanitize collisions, the empty-stem fallback, and cross-workspace isolation with
/// safe (collation-agnostic, lowercase) ids — so it holds identically on every backend
/// including postgres regardless of the database's default collation.
async fn sanitize_collisions_scope_and_empty(store: &dyn SkillStore) {
    // An empty id → the "skill" fallback stem (same collapse as an all-illegal id).
    assert_eq!(store.put("c", "", "E").await.unwrap(), "skill");
    // Distinct raw ids that sanitize to the same stem collide onto ONE entry; last wins.
    assert_eq!(store.put("c", "a b", "one").await.unwrap(), "a-b");
    assert_eq!(store.put("c", "a/b", "two").await.unwrap(), "a-b");
    assert_eq!(store.put("c", "a....b", "three").await.unwrap(), "a-b");
    assert_eq!(
        store.get("c", "a-b").await.unwrap().as_deref(),
        Some("three")
    );
    // Outer separators are trimmed on the stored stem.
    assert_eq!(store.put("c", "--foo_bar--", "F").await.unwrap(), "foo_bar");
    // Exactly the three distinct stems, ascending, no duplicates from the collisions.
    let ids: Vec<String> = store
        .list("c")
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids, vec!["a-b", "foo_bar", "skill"]);

    // Cross-workspace isolation: a skill under one workspace is invisible in another,
    // by both point lookup and listing.
    store.put("wsA", "secret", "A-ONLY").await.unwrap();
    assert_eq!(store.get("wsB", "secret").await.unwrap(), None);
    assert!(store.list("wsB").await.unwrap().is_empty());
}

/// The three *local* backends (in-mem `BTreeMap`, fs `str::cmp`, sqlite `BINARY`
/// collation) all order `list` by raw byte value — uppercase < `_` < lowercase, and a
/// shorter id sorts before its extension. Pinned here so they cannot silently diverge.
/// Deliberately NOT run against postgres, whose default collation may order these
/// mixed-case/punctuation ids differently (see the divergence note in the report).
async fn list_is_byte_ordered(store: &dyn SkillStore) {
    for (id, body) in [
        ("Zed", "z"),
        ("apex", "a"),
        ("Beta", "b"),
        ("_hidden", "h"),
        ("Zed-2", "z2"),
    ] {
        store.put("w", id, body).await.unwrap();
    }
    let ids: Vec<String> = store
        .list("w")
        .await
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids, vec!["Beta", "Zed", "Zed-2", "_hidden", "apex"]);
}

#[tokio::test]
async fn in_memory_conforms() {
    put_get_list_delete_and_scope_by_workspace(&InMemorySkillStore::new()).await;
    sanitize_collisions_scope_and_empty(&InMemorySkillStore::new()).await;
    list_is_byte_ordered(&InMemorySkillStore::new()).await;
}

#[tokio::test]
async fn fs_conforms_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    put_get_list_delete_and_scope_by_workspace(&FsSkillStore::open(&root).unwrap()).await;
    sanitize_collisions_scope_and_empty(&FsSkillStore::open(&root).unwrap()).await;
    list_is_byte_ordered(&FsSkillStore::open(&root).unwrap()).await;

    // A fresh handle over the same root (a restart) reads the committed catalog.
    let reopened = FsSkillStore::open(&root).unwrap();
    reopened.put("wsX", "persist", "BODY").await.unwrap();
    let again = FsSkillStore::open(&root).unwrap();
    assert_eq!(
        again.get("wsX", "persist").await.unwrap().as_deref(),
        Some("BODY")
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_conforms() {
    use awaken_skill_store::SqliteSkillStore;
    put_get_list_delete_and_scope_by_workspace(&SqliteSkillStore::open_in_memory().unwrap()).await;
    sanitize_collisions_scope_and_empty(&SqliteSkillStore::open_in_memory().unwrap()).await;
    list_is_byte_ordered(&SqliteSkillStore::open_in_memory().unwrap()).await;
}

/// Live Postgres conformance on a fresh schema. Skips when no Postgres is reachable
/// (`AWAKEN_TEST_DATABASE_URL`), proving the same portable bundle renders there too.
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_skill_store::PgSkillStore;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    fn database_url() -> String {
        std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        })
    }

    async fn schema_pool(schema: &'static str) -> Option<PgPool> {
        let admin = match PgPool::connect(&database_url()).await {
            Ok(pool) => pool,
            Err(err) => {
                println!("[skip] no Postgres reachable: {err}");
                return None;
            }
        };
        let _ = admin
            .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
            .await;
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        admin.close().await;
        PgPoolOptions::new()
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    conn.execute(format!("SET search_path = {schema}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url())
            .await
            .ok()
    }

    #[tokio::test]
    async fn postgres_conforms() {
        let Some(pool) = schema_pool("t_skill").await else {
            return;
        };
        let store = PgSkillStore::with_pool(pool);
        store.ensure_schema().await.unwrap();
        put_get_list_delete_and_scope_by_workspace(&store).await;
        // Collation-agnostic (lowercase-only) ordering, so it holds on postgres too. The
        // mixed-case `list_is_byte_ordered` is intentionally NOT run here: postgres uses
        // its default collation, not raw byte order (see report divergence note).
        sanitize_collisions_scope_and_empty(&store).await;
    }
}
