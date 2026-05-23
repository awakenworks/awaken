//! Wire together `AgentRuntime` → `Mailbox` → `ServerState` end-to-end —
//! pins the construction sequence `reference/http-api.md` cites. This
//! mirrors what an embedder does before calling `serve(state).await?`.
//! Route registration is what `serve()` does internally; we exit before
//! binding so the example stays offline.

use std::sync::Arc;

use awaken::prelude::*;
use awaken::stores::{InMemoryMailboxStore, InMemoryStore};

fn main() {
    // 1. Build an empty runtime — no agents, no providers, just enough to
    //    show the wiring shape. A real embedder would register agents,
    //    tools, models, and providers here.
    let runtime = Arc::new(
        AgentRuntimeBuilder::new()
            .build()
            .expect("empty runtime builds"),
    );

    // 2. Thread + run store backs message history and run records.
    let store = Arc::new(InMemoryStore::new());

    // 3. Mailbox queues dispatches off the HTTP request path so a run can
    //    outlive a hung connection. `InMemoryMailboxStore` is fine for
    //    smoke tests; `SqliteMailboxStore` / NATS for production.
    let mailbox = Arc::new(Mailbox::new(
        runtime.clone(),
        Arc::new(InMemoryMailboxStore::new()),
        store.clone(),
        "doctest-consumer".into(),
        MailboxConfig::default(),
    ));

    // 4. `ServerConfig` is the only cluster of overridable knobs; default
    //    binds 0.0.0.0:3000. Override per embedder.
    let config = ServerConfig {
        address: "127.0.0.1:0".into(),
        ..ServerConfig::default()
    };

    // 5. Assemble — `ServerState::new` is the canonical entry point that
    //    every route handler reads from. `runtime.resolver_arc()` returns
    //    the same `Arc<dyn AgentResolver>` the runtime already owns.
    let state = ServerState::new(
        runtime.clone(),
        mailbox,
        store,
        runtime.resolver_arc(),
        config,
    );

    // Smoke assertions — the wiring is correct and Arc identity holds.
    assert_eq!(state.config.address, "127.0.0.1:0");
    assert!(Arc::ptr_eq(&state.runtime, &runtime));
}
