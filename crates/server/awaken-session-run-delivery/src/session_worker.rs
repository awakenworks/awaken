// @formal entrypoint session_execution.terminal_state_never_reopens
// The original combined commit, dispatch binding and Runtime construct one Worker;
// recovery remains a projection of that same authority, not another write path.
// The retained entrypoint is this source's sole formal ledger link.
//! Session-bound Worker construction over its one combined commit boundary.

use std::sync::Arc;

use awaken_dispatch_erasure::{AnyDispatchStore, DispatchBinding};
use awaken_dispatch_execution::{DispatchWorker, Error};
use awaken_runtime::Runtime;

use crate::SessionCommit;

/// Construct a Session's Worker and select its original recovery projection.
/// The caller supplies its existing Runtime and configures attempt capabilities
/// through the Worker's ordinary builders. `with_context` preserves this
/// Worker's reader, so inherited context cannot replace the selected projection.
pub fn session_dispatch_worker(
    runtime: Arc<Runtime>,
    binding: DispatchBinding,
    commit: Arc<SessionCommit>,
    owner: impl Into<String>,
) -> Result<DispatchWorker<AnyDispatchStore>, Error> {
    let projection = commit.recovery_projection();
    let worker = DispatchWorker::new_bound(runtime, binding, commit, owner);
    match projection {
        Some(projection) => worker.with_recovery_projection(projection),
        None => Ok(worker),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    use awaken_dispatch_erasure::{AnyCoordinatorDispatch, LocalCommitAdapter};
    use awaken_runtime_contract::RuntimeRunContext;

    #[test]
    fn session_worker_configuration_retains_the_original_runtime_and_recovery_reader() {
        // Cause/effect graph: C1 Local authoritative commit / Remote recovery
        // cache; C2 inherited context has no reader / a foreign reader. Effects:
        // E1 the exact Runtime is retained; E2 later context installation keeps
        // the Worker's selected reader; E3 Remote uses its exact original cache
        // and cannot mint committed authority. Four table rows cross C1 x C2.
        // The one local queue isolates constructor/read selection; this test
        // does not execute a Remote commit through a local write authority.
        // Existing SDK remote-transport and recovery tests own that boundary.
        // WorkerContext's inherited-authority table owns per-attempt fencing;
        // this table binds its readers to the SessionCommit selection.
        for remote in [false, true] {
            for inherited_reader in [false, true] {
                let clock = awaken_time::system_clock();
                let owner = Arc::new(AnyCoordinatorDispatch::from_durable_dispatch(Arc::new(
                    awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
                )));
                let commit = Arc::new(if remote {
                    SessionCommit::Remote(crate::WorkerRecoveryCommit::new())
                } else {
                    SessionCommit::Local(Arc::new(LocalCommitAdapter::projected(
                        awaken_store_inmem::MemoryCommitCoordinator::new(clock.clone()),
                    )))
                });
                let projection = commit.recovery_projection();
                let runtime = Arc::new(Runtime::new().with_clock(clock));
                let worker = session_dispatch_worker(
                    runtime.clone(),
                    DispatchBinding::local(owner),
                    commit.clone(),
                    "session-worker",
                )
                .unwrap();
                let before = worker.committed_reader();
                let foreign: Arc<dyn CommittedThreadView> =
                    Arc::new(SessionCommit::Remote(crate::WorkerRecoveryCommit::new()));
                let context = if inherited_reader {
                    RuntimeRunContext::new().with_reader(foreign.clone())
                } else {
                    RuntimeRunContext::new()
                };
                let worker = worker.with_context(context);
                let after = worker.committed_reader();
                assert!(Arc::ptr_eq(worker.runtime(), &runtime), "E1");
                assert!(Arc::ptr_eq(&before, &after), "E2");
                assert!(!Arc::ptr_eq(&after, &foreign), "E2 foreign input refused");
                if let Some(projection) = projection {
                    let expected: Arc<dyn CommittedThreadView> = projection;
                    assert!(Arc::ptr_eq(&after, &expected), "E3 exact Remote cache");
                }
                assert_eq!(commit.committed_authority().is_some(), !remote, "E3");
            }
        }
    }
}
