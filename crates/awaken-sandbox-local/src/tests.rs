use std::sync::Arc;

use awaken_file_store::FileStore;
use tempfile::TempDir;

use crate::{LocalSandboxProvider, Mount, NamespaceSandboxProvider, SandboxProvider, Source};

fn store(dir: &TempDir) -> Arc<FileStore> {
    Arc::new(FileStore::new(dir.path().join("store")))
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit: LocalSandboxProvider
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn local_resolve_source_bytes() {
    let dir = TempDir::new().unwrap();
    let provider = LocalSandboxProvider::new(store(&dir));
    let id = provider
        .resolve_source(&Source::Bytes(bytes::Bytes::from("hello")))
        .await
        .unwrap();
    assert_eq!(id.len(), 64);
}

#[tokio::test]
async fn local_resolve_source_file() {
    let dir = TempDir::new().unwrap();
    let src_file = dir.path().join("input.txt");
    std::fs::write(&src_file, b"from file").unwrap();
    let provider = LocalSandboxProvider::new(store(&dir));
    let id = provider
        .resolve_source(&Source::File(src_file))
        .await
        .unwrap();
    assert_eq!(id.len(), 64);
}

#[tokio::test]
async fn local_resolve_source_missing_file_returns_error() {
    let dir = TempDir::new().unwrap();
    let provider = LocalSandboxProvider::new(store(&dir));
    let err = provider
        .resolve_source(&Source::File(dir.path().join("missing.txt")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, crate::SandboxError::SourceNotFound { .. }),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn local_create_sandbox_materializes_mounts() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let provider = LocalSandboxProvider::new(Arc::clone(&s));
    let id = s.put(b"content A").await.unwrap();
    let mount = Mount::new(id, "sub/a.txt");
    let sandbox = provider.create_sandbox(&[mount]).await.unwrap();
    let target = sandbox.join("sub/a.txt");
    assert!(target.exists(), "mounted file should exist at sub/a.txt");
    assert_eq!(std::fs::read(target).unwrap(), b"content A");
}

#[tokio::test]
async fn local_create_sandbox_empty_mounts() {
    let dir = TempDir::new().unwrap();
    let provider = LocalSandboxProvider::new(store(&dir));
    let sandbox = provider.create_sandbox(&[]).await.unwrap();
    assert!(sandbox.path().is_dir());
}

#[tokio::test]
async fn local_realize_mount_after_create() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let provider = LocalSandboxProvider::new(Arc::clone(&s));
    let sandbox = provider.create_sandbox(&[]).await.unwrap();
    let id = s.put(b"late mount").await.unwrap();
    provider
        .realize_mount(&sandbox, &Mount::new(id, "late.txt"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(sandbox.join("late.txt")).unwrap(),
        b"late mount"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit: NamespaceSandboxProvider
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn namespace_resolve_source_bytes() {
    let dir = TempDir::new().unwrap();
    let provider = NamespaceSandboxProvider::new(store(&dir), "tenant-a", dir.path().join("ns"));
    let id = provider
        .resolve_source(&Source::Bytes(bytes::Bytes::from("ns content")))
        .await
        .unwrap();
    assert_eq!(id.len(), 64);
}

#[tokio::test]
async fn namespace_create_sandbox_is_scoped_under_namespace_dir() {
    let dir = TempDir::new().unwrap();
    let ns_root = dir.path().join("ns");
    let provider = NamespaceSandboxProvider::new(store(&dir), "tenant-b", ns_root.clone());
    let sandbox = provider.create_sandbox(&[]).await.unwrap();
    assert!(
        sandbox.path().starts_with(ns_root.join("tenant-b")),
        "sandbox should be under ns_root/tenant-b, got: {}",
        sandbox.path().display()
    );
}

#[tokio::test]
async fn namespace_create_sandbox_with_mounts() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let provider = NamespaceSandboxProvider::new(Arc::clone(&s), "tenant-c", dir.path().join("ns"));
    let id = s.put(b"namespaced content").await.unwrap();
    let sandbox = provider
        .create_sandbox(&[Mount::new(id, "out.txt")])
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(sandbox.join("out.txt")).unwrap(),
        b"namespaced content"
    );
}

#[tokio::test]
async fn namespace_isolates_separate_namespaces() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let ns_root = dir.path().join("ns");
    let provider_a = NamespaceSandboxProvider::new(Arc::clone(&s), "ns-a", ns_root.clone());
    let provider_b = NamespaceSandboxProvider::new(Arc::clone(&s), "ns-b", ns_root.clone());
    let sb_a = provider_a.create_sandbox(&[]).await.unwrap();
    let sb_b = provider_b.create_sandbox(&[]).await.unwrap();
    assert_ne!(
        sb_a.path().parent().unwrap(),
        sb_b.path().parent().unwrap(),
        "sandboxes from different namespaces must not share a parent directory"
    );
}
