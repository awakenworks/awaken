//! Synchronous authority port for committing a physical execution effect.

use std::{io, time::Duration};

/// Invoke the effect exactly once while holding the SAME synchronous authority
/// guard used to revoke or replace the current generation. The remaining
/// lifetime comes from current authority, never a copied creation fence.
/// Denial invokes no callback; the callback must perform no asynchronous work.
/// Composition may intersect Worker and Session authority through this port.
pub trait CurrentExecutionAuthority: Send + Sync {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()>;
}
