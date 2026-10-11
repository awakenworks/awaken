// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Required roles are statically bound from one owner; composition adds no second queue or lifecycle law.
//! Immutable local/remote dispatch composition.
use super::{AnyCoordinatorDispatch, AnyDispatchStore, RemoteDispatch};
use awaken_dispatch_contract::{
    ClaimedCheckpointTransport, ClaimedCredentialDeliveryTransport, ClaimedRunCommit,
};
use std::sync::Arc;
#[derive(Clone)]
pub enum DispatchTopology {
    Local(Arc<AnyCoordinatorDispatch>),
    Remote {
        checkpoints: Arc<dyn ClaimedCheckpointTransport>,
        credentials: Arc<dyn ClaimedCredentialDeliveryTransport>,
        commit: Arc<dyn ClaimedRunCommit>,
    },
}
#[derive(Clone)]
/// Required ports are supplied together and remain immutable after construction.
///
/// A core Worker view cannot supply remote checkpoint/credential transport.
/// ```compile_fail
/// use std::sync::Arc;
/// use awaken_dispatch_erasure::{DispatchBinding, AnyDispatchStore};
/// use awaken_dispatch_contract::ClaimedRunCommit;
/// fn remote(worker: Arc<AnyDispatchStore>, commit: Arc<dyn ClaimedRunCommit>) {
///     DispatchBinding::remote(worker, commit);
/// }
/// ```
/// Claim-fenced commit is mandatory for a remote binding.
/// ```compile_fail
/// use std::sync::Arc;
/// use awaken_dispatch_erasure::{DispatchBinding, RemoteDispatch};
/// fn remote(owner: Arc<dyn RemoteDispatch>) { DispatchBinding::remote(owner); }
/// ```
/// Independent replacement of a bound queue is forbidden.
/// ```compile_fail
/// use std::sync::Arc;
/// use awaken_dispatch_erasure::{DispatchBinding, AnyDispatchStore};
/// fn replace(binding: &mut DispatchBinding, worker: Arc<AnyDispatchStore>) { binding.queue = worker; }
/// ```
pub struct DispatchBinding {
    queue: Arc<AnyDispatchStore>,
    placement: DispatchTopology,
}
impl DispatchBinding {
    pub fn local(owner: Arc<AnyCoordinatorDispatch>) -> Self {
        Self {
            queue: owner.worker_view(),
            placement: DispatchTopology::Local(owner),
        }
    }
    pub fn remote(owner: Arc<dyn RemoteDispatch>, commit: Arc<dyn ClaimedRunCommit>) -> Self {
        Self {
            queue: Arc::new(AnyDispatchStore::from_dispatch(owner.clone())),
            placement: DispatchTopology::Remote {
                checkpoints: owner.clone(),
                credentials: owner,
                commit,
            },
        }
    }
    pub fn queue(&self) -> Arc<AnyDispatchStore> {
        self.queue.clone()
    }
    pub fn placement(&self) -> &DispatchTopology {
        &self.placement
    }
}
