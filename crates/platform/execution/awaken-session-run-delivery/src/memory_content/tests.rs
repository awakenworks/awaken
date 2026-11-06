use super::*;
use std::sync::Arc;

use awaken_resource_contract::MemoryWriter as _;

struct RecallSnapshotRepository {
    heads: Vec<awaken_memory_store::Memory>,
    reads: std::sync::atomic::AtomicUsize,
    fail: bool,
}

#[async_trait]
#[cfg(test)]
impl awaken_resource_contract::MemorySnapshotSource for RecallSnapshotRepository {
    async fn snapshot_heads(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_memory_store::Memory>, awaken_memory_store::MemErr> {
        assert_eq!(store, "store-a", "keep the selected store identity");
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail {
            return Err(awaken_memory_store::MemErr::Storage(
                "snapshot refused".into(),
            ));
        }
        Ok(self.heads.clone())
    }
}

#[tokio::test]
async fn platform_handle_enforces_read_only_at_the_data_plane_boundary() {
    // Cause/effect table: C1 the selected store contains an entry; C2 access
    // is read-only; C3 operation={write,recall}. R1 C2+write => refusal with
    // no new content; R2 C1+C2+recall => the existing entry remains readable.
    // These owner-level cases move with the canonical adapter; extraction
    // composition tests remain in the executor and use this same public type.
    let fs = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    fs.create("store-a", "/existing.md", "safe").await.unwrap();
    let read_only = PlatformMemoryHandle::from_roles(
        awaken_resource_contract::MemoryExecutionContent::new(fs.clone()),
        "store-a".into(),
        false,
    );

    let error = read_only
        .write("new", "must not persist")
        .await
        .unwrap_err();
    assert!(error.contains("read-only"));
    assert!(
        awaken_resource_contract::MemoryQueries::get_by_path(fs.as_ref(), "store-a", "/new.md")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(read_only.entries().await.unwrap().len(), 1);
}

#[tokio::test]
async fn platform_semantic_mutations_preserve_repository_cas_and_aba_fences() {
    // Cause/effect decision table: R1 absent expected generation + absent path ->
    // create; R2 absent generation + present path -> path conflict; R3 current
    // generation -> update; R4 stale generation -> conflict/no clobber; R5 exact
    // generation -> delete; R6 recreated path + stale generation -> no ABA delete.
    let fs = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let handle = PlatformMemoryHandle::from_roles(
        awaken_resource_contract::MemoryExecutionContent::new(fs.clone()),
        "store".into(),
        true,
    );
    let created = handle
        .write_exact("/note.md", "v1", None)
        .await
        .expect("R1");
    assert!(
        handle
            .write_exact("/note.md", "duplicate", None)
            .await
            .is_err(),
        "R2"
    );
    let updated = handle
        .write_exact("/note.md", "v2", Some(&created.generation()))
        .await
        .expect("R3");
    assert!(
        handle
            .write_exact("/note.md", "clobber", Some(&created.generation()))
            .await
            .is_err(),
        "R4"
    );
    assert_eq!(
        handle
            .read("/note.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2"),
        "R4"
    );
    assert!(
        handle
            .delete_exact("/note.md", &updated.generation())
            .await
            .expect("R5"),
        "R5"
    );
    let recreated = handle
        .write_exact("/note.md", "v3", None)
        .await
        .expect("recreate");
    assert!(
        handle
            .delete_exact("/note.md", &updated.generation())
            .await
            .is_err(),
        "R6"
    );
    assert_eq!(
        handle.read("/note.md").await.unwrap().unwrap().id,
        recreated.id,
        "R6"
    );
}

#[tokio::test]
async fn planned_memory_head_is_an_opaque_token_only_the_store_decodes() {
    // Cause/effect decision table: T1 a planned mutation over an existing path
    // carries the observed head as a token; T2 the token is the store's own
    // head encoding; T3 a token the store cannot decode is refused and nothing
    // is written (never read as a stale head); T4 the planned token applies.
    let fs = Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
        awaken_time::system_clock(),
    ));
    let handle = PlatformMemoryHandle::from_roles(
        awaken_resource_contract::MemoryExecutionContent::new(fs.clone()),
        "store".into(),
        true,
    );
    let created = handle
        .write_exact("/note.md", "v1", None)
        .await
        .expect("seed");
    let planned = handle
        .plan_mutations(std::collections::BTreeMap::from([(
            "/note.md".to_string(),
            "v2".to_string(),
        )]))
        .await
        .expect("T1");
    let token = planned[0].observed_generation.clone().expect("T1");
    assert_eq!(
        token.encoded(),
        &serde_json::to_value(created.generation()).unwrap(),
        "T2"
    );
    let mut unreadable = planned[0].clone();
    unreadable.observed_generation = Some(awaken_ext_memory::MemoryHeadToken::new(
        serde_json::json!("not a memory head"),
    ));
    let error = handle.apply_mutation(&unreadable).await.expect_err("T3");
    assert!(
        error.contains("unreadable observed memory head"),
        "T3: {error}"
    );
    let current = handle.read("/note.md").await.unwrap().unwrap();
    assert_eq!(current.content.as_deref(), Some("v1"), "T3");
    handle.apply_mutation(&planned[0]).await.expect("T4");
    let current = handle.read("/note.md").await.unwrap().unwrap();
    assert_eq!(current.content.as_deref(), Some("v2"), "T4");
}

/// R1: one selected snapshot, source-nanosecond recency and lexical ties, no blank or
/// missing content. R2: an empty snapshot stays empty. R3: snapshot failure
/// propagates unchanged without fallback. Each row reads exactly once; the
/// snapshot-only role cannot perform legacy list/get stitching or writes.
/// R4: distinct source times keep their order even when projection saturates.
#[tokio::test]
async fn recall_entries_use_only_the_atomic_memory_snapshot() {
    let memory = |id: &str, path: &str, content: Option<&str>, nanos: u128| {
        let text = content.unwrap_or_default();
        awaken_memory_store::Memory {
            id: id.into(),
            path: path.into(),
            content_sha256: awaken_memory_store::sha256_hex(text),
            content_size: text.len() as u64,
            version: 1,
            created_unix_nanos: 1,
            updated_unix_nanos: nanos,
            content: content.map(str::to_owned),
        }
    };
    for (rule, heads, fail, expected) in [
        (
            "R1",
            vec![
                memory("old", "/old.md", Some(" old "), 2),
                memory("z", "/z.md", Some(" z "), 5),
                memory("blank", "/blank.md", Some("  "), 100),
                memory("a", "/a.md", Some(" a "), 5),
                memory("missing", "/missing.md", None, 100),
            ],
            false,
            vec![("/a.md", "a"), ("/z.md", "z"), ("/old.md", "old")],
        ),
        ("R2", Vec::new(), false, Vec::new()),
        ("R3", Vec::new(), true, Vec::new()),
        (
            "R4",
            vec![
                memory("old", "/a-old.md", Some("old"), u128::from(u64::MAX) + 1),
                memory("new", "/z-new.md", Some("new"), u128::from(u64::MAX) + 2),
            ],
            false,
            vec![("/z-new.md", "new"), ("/a-old.md", "old")],
        ),
    ] {
        let source = RecallSnapshotRepository {
            heads,
            reads: std::sync::atomic::AtomicUsize::new(0),
            fail,
        };
        let result = PlatformMemoryHandle::entries_from_snapshot(&source, "store-a").await;
        assert_eq!(
            source.reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{rule}"
        );
        if fail {
            let Err(error) = result else {
                panic!("{rule}: snapshot failure must propagate");
            };
            assert_eq!(error, "storage: snapshot refused", "{rule}");
        } else {
            let entries = result.expect(rule);
            let actual: Vec<_> = entries
                .iter()
                .map(|entry| (entry.path.to_str().unwrap(), entry.content.as_str()))
                .collect();
            assert_eq!(actual, expected, "{rule}");
        }
    }
}
