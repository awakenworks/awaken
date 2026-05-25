//! `ThreadRunStore` conformance for the file backend, focused on the
//! version-guarded committed append (ADR-0042 A). `FileStore` is a
//! single-node backend serialized by its coordinator's commit lock, so it
//! relies on the trait's default `checkpoint_append`; these tests prove that
//! default behaves correctly on the file backend.
#![cfg(feature = "file")]

#[path = "thread_store_conformance.rs"]
mod thread_store_conformance;

use awaken_stores::FileStore;
use tempfile::TempDir;

macro_rules! conformance_test {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            let dir = TempDir::new().expect("tempdir");
            let store = FileStore::new(dir.path());
            thread_store_conformance::$name(&store).await;
        }
    };
}

conformance_test!(checkpoint_append_assigns_version);
conformance_test!(checkpoint_append_unconditional_appends);
conformance_test!(checkpoint_append_rejects_stale_version);
