// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Reserves a replay identity for one local caller until the Coordinator's receipt is known.
//! Process-local ownership of an in-flight claim request id.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_sync::LeafLock;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

type Target = Arc<AsyncMutex<Option<String>>>;
type Targets = LeafLock<HashMap<RunId, ActiveTarget>>;

#[derive(Default)]
struct ActiveTarget {
    gate: Target,
    requests: usize,
}

pub(super) struct PendingClaims {
    untargeted: LeafLock<VecDeque<String>>,
    targets: Targets,
}

impl Default for PendingClaims {
    fn default() -> Self {
        Self {
            untargeted: LeafLock::new(VecDeque::new()),
            targets: LeafLock::new(HashMap::new()),
        }
    }
}

impl PendingClaims {
    pub(super) async fn begin(&self, run_id: Option<&RunId>) -> Attempt<'_> {
        let mut request = if let Some(run_id) = run_id {
            let entry = {
                let mut targets = self.targets.lock();
                let active = targets.entry(run_id.clone()).or_default();
                active.requests += 1;
                TargetEntry {
                    run_id: run_id.clone(),
                    target: active.gate.clone(),
                    targets: &self.targets,
                }
            };
            ClaimRequestId::Directed {
                id: entry.target.clone().lock_owned().await,
                _entry: entry,
            }
        } else {
            ClaimRequestId::Undirected(self.untargeted.lock().pop_front())
        };
        let fresh = request.id_mut().is_none();
        request
            .id_mut()
            .get_or_insert_with(|| uuid::Uuid::now_v7().to_string());
        Attempt {
            request,
            fresh,
            untargeted: &self.untargeted,
        }
    }
}

struct TargetEntry<'a> {
    run_id: RunId,
    target: Target,
    targets: &'a Targets,
}

impl Drop for TargetEntry<'_> {
    fn drop(&mut self) {
        let mut targets = self.targets.lock();
        let active = targets
            .get_mut(&self.run_id)
            .expect("registered directed claim");
        active.requests -= 1;
        if active.requests == 0 && self.target.try_lock().is_ok_and(|id| id.is_none()) {
            targets.remove(&self.run_id);
        }
    }
}

enum ClaimRequestId<'a> {
    Directed {
        id: OwnedMutexGuard<Option<String>>,
        // The guard above drops before the entry checks whether to remove it.
        _entry: TargetEntry<'a>,
    },
    Undirected(Option<String>),
}

impl ClaimRequestId<'_> {
    fn id(&self) -> &Option<String> {
        match self {
            Self::Directed { id, .. } => id,
            Self::Undirected(id) => id,
        }
    }

    fn id_mut(&mut self) -> &mut Option<String> {
        match self {
            Self::Directed { id, .. } => id,
            Self::Undirected(id) => id,
        }
    }
}

pub(super) struct Attempt<'a> {
    request: ClaimRequestId<'a>,
    fresh: bool,
    untargeted: &'a LeafLock<VecDeque<String>>,
}

impl Attempt<'_> {
    pub(super) fn request_id(&self) -> &str {
        self.request
            .id()
            .as_deref()
            .expect("active claim request id")
    }

    pub(super) fn fresh(&self) -> bool {
        self.fresh
    }

    pub(super) fn complete(&mut self) {
        self.request.id_mut().take();
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if let ClaimRequestId::Undirected(id) = &mut self.request
            && let Some(id) = id.take()
        {
            self.untargeted.lock().push_back(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::Poll;

    #[tokio::test]
    async fn cancelling_a_directed_request_releases_only_its_gate_and_retains_its_id() {
        let pending = PendingClaims::default();
        let target = RunId("same-run".into());
        let first = pending.begin(Some(&target)).await;
        let first_id = first.request_id().to_owned();
        let mut same = Box::pin(pending.begin(Some(&target)));
        std::future::poll_fn(|cx| {
            assert!(same.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let mut other = pending.begin(Some(&RunId("other-run".into()))).await;
        assert_ne!(other.request_id(), first_id);
        other.complete();
        drop(other);

        drop(first);
        let mut retry = same.await;
        assert_eq!(retry.request_id(), first_id);
        assert!(!retry.fresh());
        retry.complete();
        drop(retry);
        assert!(pending.targets.lock().is_empty());
    }

    #[tokio::test]
    async fn cancelled_undirected_ids_are_each_reserved_by_only_one_retry() {
        let pending = PendingClaims::default();
        let first = pending.begin(None).await;
        let second = pending.begin(None).await;
        let first_id = first.request_id().to_owned();
        let second_id = second.request_id().to_owned();
        assert_ne!(first_id, second_id);
        drop(first);
        drop(second);

        let mut first_retry = pending.begin(None).await;
        let mut second_retry = pending.begin(None).await;
        assert_eq!(first_retry.request_id(), first_id);
        assert_eq!(second_retry.request_id(), second_id);
        assert!(!first_retry.fresh() && !second_retry.fresh());
        first_retry.complete();
        second_retry.complete();
    }
}
