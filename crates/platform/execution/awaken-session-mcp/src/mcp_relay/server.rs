// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Own the original relay task, its stop signal and repeatable join result.
// @formal reason: The original per-route MCP call fences still own local call drainage;
// @formal reason: this is a physical service lifetime, not durable Session authority or a proof claim.
//! Physical relay service ownership, shared only by explicit relay instances.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};

use super::Routes;

type Completion = Shared<BoxFuture<'static, Result<(), Arc<std::io::Error>>>>;

pub(super) struct RelayServer {
    stopping: awaken_runtime_contract::CancellationToken,
    // Keep the original join future even if a waiting caller is cancelled.
    // A later caller resumes the same task/result rather than losing a taken
    // JoinHandle or mistaking somebody else's pending join for completion.
    completion: Completion,
    #[cfg(any(test, feature = "test-support"))]
    abort: tokio::task::AbortHandle,
}

struct LiveGuard(Arc<AtomicBool>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl RelayServer {
    pub(super) fn start(
        listener: tokio::net::TcpListener,
        app: axum::Router,
        routes: Routes,
        alive: Arc<AtomicBool>,
        stopping: awaken_runtime_contract::CancellationToken,
    ) -> Arc<Self> {
        let shutdown = stopping.clone();
        // Capture before the first poll so dropping an unpolled server future
        // cannot leave its liveness observation permanently true.
        let live = LiveGuard(alive);
        let task = tokio::spawn(async move {
            let _live = live;
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown.cancelled().await;
                    // A poisoned route map serves no request (`forward`
                    // refuses it), so there is no call to wait for.
                    let fences = routes
                        .lock()
                        .map(|routes| {
                            routes
                                .values()
                                .map(|route| route.call_fence.clone())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    futures_util::future::join_all(
                        fences.iter().map(|fence| fence.close_and_wait()),
                    )
                    .await;
                    if let Ok(mut routes) = routes.lock() {
                        routes.clear();
                    }
                })
                .await
        });
        #[cfg(any(test, feature = "test-support"))]
        let abort = task.abort_handle();
        let completion = async move {
            match task.await {
                Ok(result) => result.map_err(Arc::new),
                Err(error) => Err(Arc::new(std::io::Error::other(error))),
            }
        }
        .boxed()
        .shared();
        Arc::new(Self {
            stopping,
            completion,
            #[cfg(any(test, feature = "test-support"))]
            abort,
        })
    }

    pub(super) fn is_closing(&self) -> bool {
        self.stopping.is_cancelled()
    }

    pub(super) async fn close_and_wait(&self) -> std::io::Result<()> {
        self.stopping.cancel();
        self.completion
            .clone()
            .await
            .map_err(|error| std::io::Error::new(error.kind(), error))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn abort_handle(&self) -> tokio::task::AbortHandle {
        self.abort.clone()
    }
}

impl Drop for RelayServer {
    fn drop(&mut self) {
        // Drop requests closure; only close_and_wait acknowledges joined local
        // completion. The original server task drives route drainage itself.
        self.stopping.cancel();
    }
}
