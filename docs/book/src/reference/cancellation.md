# Cancellation

Cooperative cancellation token used to interrupt agent runs, streaming loops,
and long-running operations.

## CancellationToken

A cloneable handle backed by a shared `AtomicBool` and a `tokio::sync::Notify`.
All clones share the same cancellation state -- cancelling any clone cancels all of them.

```rust,ignore
use awaken::CancellationToken;

let token = CancellationToken::new();
```

**Crate path:** `awaken_runtime::cancellation::CancellationToken`

### Methods

```rust,ignore
/// Create a new uncancelled token.
pub fn new() -> Self

/// Signal cancellation. Wakes all async waiters immediately.
pub fn cancel(&self)

/// Check if cancellation has been requested (synchronous poll).
pub fn is_cancelled(&self) -> bool

/// Wait until cancellation is signalled. Resolves immediately if already cancelled.
pub async fn cancelled(&self)
```

### Traits

- `Clone` -- clones share the same underlying state
- `Default` -- creates an uncancelled token (equivalent to `new()`)

## Synchronous polling

Use `is_cancelled()` to check cancellation from synchronous code or tight loops:

```rust,ignore
let token = CancellationToken::new();

while !token.is_cancelled() {
    // do work
}
```

## Async waiting with tokio::select!

Use `cancelled()` in a `tokio::select!` branch to interrupt async operations
without polling:

```rust,ignore
let token = CancellationToken::new();

tokio::select! {
    result = some_async_work() => {
        // work completed before cancellation
    }
    _ = token.cancelled() => {
        // cancellation was signalled
    }
}
```

The `cancelled()` future resolves immediately if the token is already cancelled,
so there is no race between checking and waiting.

## Cooperative semantics

Cancellation is cooperative. Calling `cancel()` sets a flag and wakes async
waiters, but does not abort any running task. Code must check `is_cancelled()`
or `select!` on `cancelled()` to observe and respond to cancellation.

Key properties:

- **Idempotent** -- calling `cancel()` multiple times is safe and has no additional effect.
- **Shared** -- all clones observe the same cancellation state. Cancelling from any clone is visible to all others.
- **Ordering** -- uses `SeqCst` ordering on the atomic flag, so cancellation is immediately visible across threads.
- **Immediate wake** -- `cancel()` calls `Notify::notify_waiters()`, waking all tasks blocked on `cancelled()` without waiting for the next poll.

## Runtime usage

The runtime passes a `CancellationToken` into each agent run. It is used to:

- Interrupt streaming inference mid-response when the caller requests cancellation.
- Stop the agent loop between inference cycles.
- Propagate cancellation from external transports (HTTP, SSE) into the runtime.

A typical streaming loop checks cancellation alongside each chunk:

```rust,ignore
let token = CancellationToken::new();
let clone = token.clone();

tokio::select! {
    _ = async {
        while let Some(chunk) = stream.next().await {
            // process chunk
        }
    } => {}
    _ = clone.cancelled() => {
        // stop processing, clean up
    }
}
```

## Key Files

- `crates/awaken-runtime/src/cancellation.rs` -- `CancellationToken` implementation
