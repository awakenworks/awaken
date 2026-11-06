// @formal entrypoint worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal entrypoint worker_drain.grace_expiry_retains_cancellation_observer
//! Start and drain the original process services without an execution composition.

use std::sync::Arc;
use std::time::Duration;

use awaken_dispatch_contract::CompletionSink;
use awaken_dispatch_erasure::{AnyCoordinatorDispatch, AnyDispatchStore, DispatchBinding};
use awaken_dispatch_execution::{Error, ProcessDispatch, WorkerResolver};

use crate::BackgroundRuns;

type ResolverFactory = dyn Fn(&ProcessDispatch) -> Result<Arc<dyn WorkerResolver<AnyDispatchStore>>, Error>
    + Send
    + Sync;

/// One process's dispatch lifecycle over its selected authority and background
/// registry. Consumers: the Coordinator composition and the claiming Worker.
/// The resolver remains lazy, so an absent binding, a disabled service or a
/// drained process never constructs execution or observation capabilities.
#[derive(Clone)]
#[must_use = "retain the lifecycle and its application owners until drain completes"]
pub struct SessionDispatchLifecycle<T = ()> {
    dispatch: Arc<ProcessDispatch>,
    binding: Option<DispatchBinding>,
    completion: Arc<dyn CompletionSink>,
    terminal_owner: Option<Arc<AnyCoordinatorDispatch>>,
    background: Arc<BackgroundRuns>,
    resolver: Arc<ResolverFactory>,
    _lifetime: T,
}

impl SessionDispatchLifecycle {
    /// Select the original process and collaborators once. `terminal_owner`
    /// is present only when this process owns committed-terminal scanning.
    /// The factory receives the process only while constructing its resolver;
    /// the resolver retains its scan state, never the process services.
    pub fn new(
        dispatch: Arc<ProcessDispatch>,
        binding: Option<DispatchBinding>,
        completion: Arc<dyn CompletionSink>,
        terminal_owner: Option<Arc<AnyCoordinatorDispatch>>,
        background: Arc<BackgroundRuns>,
        resolver: impl Fn(&ProcessDispatch) -> Result<Arc<dyn WorkerResolver<AnyDispatchStore>>, Error>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            dispatch,
            binding,
            completion,
            terminal_owner,
            background,
            resolver: Arc::new(resolver),
            _lifetime: (),
        }
    }

    /// Retain process-owned application capabilities outside the resolver.
    pub fn retaining<T>(self, lifetime: T) -> SessionDispatchLifecycle<T> {
        SessionDispatchLifecycle {
            dispatch: self.dispatch,
            binding: self.binding,
            completion: self.completion,
            terminal_owner: self.terminal_owner,
            background: self.background,
            resolver: self.resolver,
            _lifetime: lifetime,
        }
    }
}

impl<T> SessionDispatchLifecycle<T> {
    #[must_use]
    pub fn dispatch(&self) -> &Arc<ProcessDispatch> {
        &self.dispatch
    }

    /// Start the original pool only for the topology that owns local claims.
    pub fn start_local_pool(&self) -> Result<(), Error> {
        self.dispatch
            .ensure_pool(self.binding.clone(), self.completion.clone(), || {
                (self.resolver)(&self.dispatch)
            })
    }

    /// Start the original terminal scanner only for a poolless truth owner.
    /// A failed resolver leaves its existing initializer retryable.
    pub fn start_terminal_reconciliation(&self) -> bool {
        self.dispatch.ensure_terminal_reconciliation(
            self.terminal_owner.is_some(),
            self.terminal_owner.clone(),
            self.completion.clone(),
            || (self.resolver)(&self.dispatch).ok(),
        )
    }

    /// Close admission, await claimed Runs, then drain the same auxiliary
    /// registry within the remaining deadline. A Run can register extraction
    /// just before releasing its claim capacity; reversing this order loses it.
    pub async fn drain(&self, timeout: Duration) -> bool {
        drain_session_dispatch(&self.dispatch, &self.background, timeout).await
    }
}

/// Close admission and drain the existing process without constructing or
/// replacing startup ports. One deadline and ordering serve every caller.
pub async fn drain_session_dispatch(
    dispatch: &ProcessDispatch,
    background: &BackgroundRuns,
    timeout: Duration,
) -> bool {
    let deadline = awaken_time::mono::MonoDeadline::after(timeout);
    let Some(_claimed_runs) = dispatch.drain_claimed_runs(&deadline).await else {
        return false;
    };
    let remaining = deadline.remaining();
    !remaining.is_zero() && background.drain(remaining).await
}

#[async_trait::async_trait]
impl<T: Send + Sync + 'static> awaken_dispatch_execution::ProcessDispatchLifecycle
    for SessionDispatchLifecycle<T>
{
    fn start(&self) -> Result<(), Error> {
        self.start_local_pool()?;
        self.start_terminal_reconciliation();
        Ok(())
    }

    async fn drain(&self, timeout: Duration) -> bool {
        Self::drain(self, timeout).await
    }
}

#[cfg(test)]
mod tests;
