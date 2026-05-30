//! `ThreadStore` save/load round-trip via the in-memory backend — pins
//! the trait method shapes `reference/thread-model.md` cites.

use awaken::contract::message::{Message, Role};
use awaken::server_contract::storage::ThreadStore;
use awaken::stores::InMemoryStore;
use awaken_contract::Thread;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let store = InMemoryStore::new();

    let thread = Thread::with_id("t-1");
    store.save_thread(&thread).await.expect("save thread");

    let loaded = store
        .load_thread("t-1")
        .await
        .expect("load thread")
        .expect("thread exists");
    assert_eq!(loaded.id, "t-1");

    // Message construction — `user/system/assistant` constructors hide the
    // ContentBlock layout from end users.
    let msg = Message::user("hello world");
    assert!(matches!(msg.role, Role::User));
    assert_eq!(msg.content.len(), 1);
}
