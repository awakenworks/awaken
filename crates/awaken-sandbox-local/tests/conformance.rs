//! Conformance tests for [`SandboxProvider`] implementations.
//!
//! Every provider (local, namespace) is exercised through the same suite via
//! the `run_provider_conformance` helper.

use std::sync::Arc;

use awaken_file_store::FileStore;
use awaken_sandbox_local::{
    LocalSandboxProvider, Mount, NamespaceSandboxProvider, SandboxProvider, Source,
};
use tempfile::TempDir;

// ──────────────────────────────────────────────────────────────────────────────
// Conformance harness
// ──────────────────────────────────────────────────────────────────────────────

async fn run_provider_conformance(provider: &impl SandboxProvider, store: &FileStore) {
    // resolve_source from bytes
    let id = provider
        .resolve_source(&Source::Bytes(bytes::Bytes::from_static(b"conformance")))
        .await
        .expect("resolve_source(Bytes) must succeed");
    assert_eq!(id.len(), 64, "content_id must be 64 hex chars");

    // resolved id must be retrievable from the file store
    let blob = store
        .get(&id)
        .await
        .expect("resolved content_id must be in store");
    assert_eq!(blob.as_ref(), b"conformance");

    // create_sandbox with no mounts returns a valid directory
    let empty_sb = provider
        .create_sandbox(&[])
        .await
        .expect("create_sandbox([]) must succeed");
    assert!(
        empty_sb.path().is_dir(),
        "empty sandbox root must be a directory"
    );

    // create_sandbox materializes mounts
    let data = b"mount data";
    let mount_id = store.put(data).await.expect("pre-load blob");
    let mount = Mount::new(mount_id.clone(), "dir/mounted.txt");
    let sb = provider
        .create_sandbox(&[mount])
        .await
        .expect("create_sandbox with mount must succeed");
    let file_path = sb.join("dir/mounted.txt");
    assert!(file_path.exists(), "mounted file must exist in sandbox");
    assert_eq!(std::fs::read(&file_path).unwrap(), data);

    // realize_mount on existing sandbox
    let id2 = store.put(b"late").await.unwrap();
    provider
        .realize_mount(&sb, &Mount::new(id2, "late.bin"))
        .await
        .expect("realize_mount on existing sandbox must succeed");
    assert_eq!(std::fs::read(sb.join("late.bin")).unwrap(), b"late");

    // Sandbox is cleaned up when dropped
    let path = {
        let temp_sb = provider.create_sandbox(&[]).await.unwrap();
        temp_sb.path().to_path_buf()
    };
    assert!(!path.exists(), "sandbox directory must be removed on drop");
}

// ──────────────────────────────────────────────────────────────────────────────
// Local provider conformance
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn local_provider_conformance() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(FileStore::new(dir.path().join("store")));
    let provider = LocalSandboxProvider::new(Arc::clone(&store));
    run_provider_conformance(&provider, &store).await;
}

// ──────────────────────────────────────────────────────────────────────────────
// Namespace provider conformance
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn namespace_provider_conformance() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(FileStore::new(dir.path().join("store")));
    let provider = NamespaceSandboxProvider::new(
        Arc::clone(&store),
        "conformance-ns",
        dir.path().join("namespaces"),
    );
    run_provider_conformance(&provider, &store).await;
}
