# Independent SDK consumer

This application is a separate Cargo workspace. It imports only public SDK and
store APIs; it does not use the unpublished runtime-examples prelude, root dev
dependencies, fixture features, or workspace feature unification.

From the repository root:

```sh
cargo run --locked --manifest-path examples/sdk-consumer/Cargo.toml
cargo test --locked --manifest-path examples/sdk-consumer/Cargo.toml
```

Copy `Cargo.toml` and `src/` to start an embedded application. The manifest uses
repository-relative path dependencies; adjust those paths for your checkout.
Replace `DemoModel` with your provider adapter while retaining the public
`awaken_runtime::model::LlmExecutor` port.

The ordinary application's Awaken dependencies are `awaken-runtime`, the chosen
store, and `awaken-time`. Custom tools add `serde` and `schemars`. Filesystem
persistence is an optional backend in this example's dev dependencies.

`tests/tasks.rs` covers single-turn output, multi-turn history, a typed custom
tool, Unicode streaming, cancellation before inference, asynchronous approval,
and persistence/restart, plus public web-policy configuration. Runtime integration tests add denial, timeout, callback
failure, cold approval recovery and concurrent-reply histories.

The same consumer tests run in quick/push/full Rust gates. They require no model
credentials or live services. Backend crash, fencing and platform-specific cases
remain in their integration tiers. The required task identities are owned by
`EXPECTED` in [the consumer gate](../../scripts/ci/check_sdk_consumer.py); every
required case must exist and execute without ignoring or filtering it.
