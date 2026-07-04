use tempfile::TempDir;

use crate::FileStore;

fn store(dir: &TempDir) -> FileStore {
    FileStore::new(dir.path())
}

#[tokio::test]
async fn put_and_get_roundtrip() {
    let dir = TempDir::new().unwrap();
    let fs = store(&dir);
    let data = b"hello awaken-file-store";
    let id = fs.put(data).await.unwrap();
    assert_eq!(id.len(), 64);
    let got = fs.get(&id).await.unwrap();
    assert_eq!(got.as_ref(), data);
}

#[tokio::test]
async fn exists_returns_true_after_put() {
    let dir = TempDir::new().unwrap();
    let fs = store(&dir);
    let id = fs.put(b"exists test").await.unwrap();
    assert!(fs.exists(&id).await);
}

#[tokio::test]
async fn exists_returns_false_for_unknown_id() {
    let dir = TempDir::new().unwrap();
    let fs = store(&dir);
    let id = "a".repeat(64);
    assert!(!fs.exists(&id).await);
}

#[tokio::test]
async fn get_missing_returns_not_found() {
    let dir = TempDir::new().unwrap();
    let fs = store(&dir);
    let id = "b".repeat(64);
    let err = fs.get(&id).await.unwrap_err();
    assert!(
        matches!(err, crate::FileStoreError::NotFound { .. }),
        "expected NotFound, got {err}"
    );
}

#[tokio::test]
async fn get_invalid_id_returns_invalid_id_error() {
    let dir = TempDir::new().unwrap();
    let fs = store(&dir);
    let err = fs.get("not-a-valid-id").await.unwrap_err();
    assert!(
        matches!(err, crate::FileStoreError::InvalidId(_)),
        "expected InvalidId, got {err}"
    );
}

#[tokio::test]
async fn put_is_content_addressed_and_deterministic() {
    let dir = TempDir::new().unwrap();
    let fs = store(&dir);
    let id1 = fs.put(b"same content").await.unwrap();
    let id2 = fs.put(b"same content").await.unwrap();
    assert_eq!(id1, id2);
}

#[tokio::test]
async fn blob_path_uses_two_char_prefix_sharding() {
    let dir = TempDir::new().unwrap();
    let fs = store(&dir);
    let data = b"sharding test";
    let id = fs.put(data).await.unwrap();
    let prefix = &id[..2];
    let shard_dir = fs.root().join(prefix);
    assert!(shard_dir.is_dir(), "shard directory should exist");
}
