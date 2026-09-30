// @formal evidence credential.managed_rollout_failure_or_busy_session_remains_pending
//! Exercise the existing delivery Future at its repository-call boundary.
//! This monitor has no storage state or repository algorithm. Every repository
//! call fails a check; actual adapters, durability and Session busy detection
//! remain outside this controlled-port executable oracle.
#![allow(unused_variables)]
use super::*;
use crate::catalog::ManagedVaultMutationError;
use std::future::Future;
use std::task::{Context, Poll, Waker};

struct RejectRepository;
// Reject synchronously at the port call, before any boxed Future exists.
// async-trait 0.1.91 uses these lifetimes/return types; no mock repository
// operation or owned-argument coroutine is created.
impl CredentialRepo for RejectRepository {
    fn put<'call0, 'async_trait>(
        &'call0 self,
        source: CredentialSource,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), CredentialError>> + Send + 'async_trait>>
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn put_if_absent<'call0, 'async_trait>(
        &'call0 self,
        source: CredentialSource,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<CredentialSource, CredentialError>> + Send + 'async_trait>,
    >
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn get<'call0, 'call1, 'async_trait>(
        &'call0 self,
        id: &'call1 CredentialSourceId,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<CredentialSource, CredentialError>> + Send + 'async_trait>,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn list<'call0, 'call1, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Vec<CredentialSource>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn begin_mutation<'call0, 'async_trait>(
        &'call0 self,
        intent: CredentialMutationIntent,
        _now_unix_ms: u64,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<bool, CredentialError>> + Send + 'async_trait>>
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn mark_mutation_ready<'call0, 'call1, 'async_trait>(
        &'call0 self,
        intent: &'call1 CredentialMutationIntent,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<CredentialMutationIntent, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn apply_mutation<'call0, 'call1, 'async_trait>(
        &'call0 self,
        intent: &'call1 CredentialMutationIntent,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<CredentialMutationIntent, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn pending_mutations<'call0, 'async_trait>(
        &'call0 self,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Vec<CredentialMutationIntent>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn claim_expired_mutation<'call0, 'call1, 'async_trait>(
        &'call0 self,
        intent: &'call1 CredentialMutationIntent,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Option<CredentialMutationIntent>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn abort_mutation<'call0, 'call1, 'async_trait>(
        &'call0 self,
        intent: &'call1 CredentialMutationIntent,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<CredentialMutationIntent, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn complete_mutation<'call0, 'call1, 'async_trait>(
        &'call0 self,
        intent: &'call1 CredentialMutationIntent,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), CredentialError>> + Send + 'async_trait>>
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn material_refs<'call0, 'async_trait>(
        &'call0 self,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Vec<crate::SecretRef>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn put_pool<'call0, 'async_trait>(
        &'call0 self,
        pool: CredentialPool,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), CredentialError>> + Send + 'async_trait>>
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn get_pool<'call0, 'call1, 'async_trait>(
        &'call0 self,
        id: &'call1 CredentialPoolId,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<CredentialPool, CredentialError>> + Send + 'async_trait>,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn list_pools<'call0, 'call1, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Vec<CredentialPool>, CredentialError>> + Send + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
}
impl ManagedVaultRepo for RejectRepository {
    fn insert_vault<'call0, 'call1, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
        vault: ManagedVault,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), CredentialError>> + Send + 'async_trait>>
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn ensure_vault<'call0, 'call1, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
        vault: ManagedVault,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<ManagedVault, CredentialError>> + Send + 'async_trait>,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn replace_vault<'call0, 'call1, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
        expected_revision: u64,
        vault: ManagedVault,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<(), ManagedVaultMutationError>> + Send + 'async_trait>,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn get_vault<'call0, 'call1, 'call2, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
        id: &'call2 str,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Option<ManagedVault>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        'call2: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn list_vaults<'call0, 'call1, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Vec<ManagedVault>, CredentialError>> + Send + 'async_trait>,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn get_vault_credential<'call0, 'call1, 'call2, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
        id: &'call2 str,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Option<ManagedVaultCredential>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        'call2: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn get_vault_credential_by_source<'call0, 'call1, 'call2, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
        source_id: &'call2 CredentialSourceId,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Option<ManagedVaultCredential>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        'call2: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn list_vault_credentials<'call0, 'call1, 'call2, 'async_trait>(
        &'call0 self,
        workspace_id: &'call1 str,
        vault_id: &'call2 str,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Vec<ManagedVaultCredential>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        'call2: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
}
impl ManagedCredentialRepository for RejectRepository {
    fn begin_managed_mutation<'call0, 'async_trait>(
        &'call0 self,
        pending: PendingManagedCredentialMutation,
        _now_unix_ms: u64,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<bool, CredentialError>> + Send + 'async_trait>>
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn commit_managed_mutation<'call0, 'call1, 'async_trait>(
        &'call0 self,
        pending: &'call1 PendingManagedCredentialMutation,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<
                        PendingManagedCredentialMutation,
                        ManagedCredentialMutationError,
                    >,
                > + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn mark_managed_mutation_ready<'call0, 'call1, 'async_trait>(
        &'call0 self,
        pending: &'call1 PendingManagedCredentialMutation,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<PendingManagedCredentialMutation, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn pending_managed_mutations<'call0, 'async_trait>(
        &'call0 self,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Vec<PendingManagedCredentialMutation>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn claim_expired_managed_mutation<'call0, 'call1, 'async_trait>(
        &'call0 self,
        pending: &'call1 PendingManagedCredentialMutation,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Option<PendingManagedCredentialMutation>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn abort_managed_mutation<'call0, 'call1, 'async_trait>(
        &'call0 self,
        pending: &'call1 PendingManagedCredentialMutation,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<PendingManagedCredentialMutation, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn complete_managed_mutation<'call0, 'call1, 'async_trait>(
        &'call0 self,
        pending: &'call1 PendingManagedCredentialMutation,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), CredentialError>> + Send + 'async_trait>>
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn managed_rollout<'call0, 'call1, 'async_trait>(
        &'call0 self,
        event_id: &'call1 str,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Option<ManagedCredentialRollout>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn pending_managed_rollouts<'call0, 'async_trait>(
        &'call0 self,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<Vec<ManagedCredentialRollout>, CredentialError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn complete_managed_rollout<'call0, 'call1, 'async_trait>(
        &'call0 self,
        rollout: &'call1 ManagedCredentialRollout,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), CredentialError>> + Send + 'async_trait>>
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
    fn pending_managed_vault_deletions<'call0, 'async_trait>(
        &'call0 self,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Vec<ManagedVault>, CredentialError>> + Send + 'async_trait>,
    >
    where
        'call0: 'async_trait,
        Self: 'async_trait,
    {
        panic!("repository accessed without convergence");
    }
}

struct ControlledTarget {
    outcome: u8,
    defer: bool,
}

// The controlled port owns only an outcome and one optional wait. Its Future
// makes each poll and its one optional wait explicit to the executable oracle.
// The production delivery Future and its await/drop behavior remain unchanged.
struct ControlledDelivery {
    outcome: u8,
    defer: bool,
}

impl Future for ControlledDelivery {
    type Output = Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError>;

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.defer {
            self.defer = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(match self.outcome {
            0 => Ok(ManagedCredentialAdoptionProgress::Pending),
            1 => Err(ManagedCredentialAdoptionError::InvalidEvent("x".into())),
            2 => Err(ManagedCredentialAdoptionError::IdentityCollision),
            3 => Err(ManagedCredentialAdoptionError::Unauthorized),
            4 => Err(ManagedCredentialAdoptionError::Unavailable("x".into())),
            _ => unreachable!("outcome outside the closed pending/error domain"),
        })
    }
}

impl ManagedCredentialRolloutTarget for ControlledTarget {
    fn rollout<'call0, 'call1, 'async_trait>(
        &'call0 self,
        event: &'call1 ManagedCredentialRollout,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<
                        ManagedCredentialAdoptionProgress,
                        ManagedCredentialAdoptionError,
                    >,
                > + Send
                + 'async_trait,
        >,
    >
    where
        'call0: 'async_trait,
        'call1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(ControlledDelivery {
            outcome: self.outcome,
            defer: self.defer,
        })
    }
}

fn check_case(outcome: u8, defer: bool, cancel: bool, version: u64, revision: u64, operation: u8) {
    // Causes: Pending or each of the four closed target errors; immediate vs
    // one deferred poll; cancellation only while deferred; fence revision
    // boundary values and every operation. R1 immediate/deferred completion =>
    // Ready(Ok(Pending)); R2 dropping the waiting Future completes nothing;
    // R3 every repository method is forbidden in both R1 and R2. The monitor
    // checks calls, not a copied admission predicate or simulated durable store.
    // Domain: fixed one-byte owned IDs/error payloads, one delivery, at most
    // two manual polls, default tracing state; no SQL or target implementation
    // refinement and no liveness under an arbitrary executor is claimed.
    assert!(outcome < 5 && operation < 4 && (!cancel || defer));
    let event = ManagedCredentialRollout {
        id: "e".into(),
        workspace_id: "w".into(),
        vault_id: "v".into(),
        credential_id: "c".into(),
        source_id: CredentialSourceId("s".into()),
        source_version: version,
        credential_revision: revision,
        operation: match operation {
            0 => ManagedCredentialOperation::Create,
            1 => ManagedCredentialOperation::Update,
            2 => ManagedCredentialOperation::Archive,
            _ => ManagedCredentialOperation::Delete,
        },
    };
    let repo = RejectRepository;
    let target = ControlledTarget { outcome, defer };
    let mut future = std::pin::pin!(reconcile_managed_credential_rollout(&event, &repo, &target));
    let mut context = Context::from_waker(Waker::noop());
    let first = future.as_mut().poll(&mut context);
    let completed = if defer {
        assert!(first.is_pending(), "target wait is preserved");
        if cancel {
            return;
        }
        future.as_mut().poll(&mut context)
    } else {
        first
    };
    assert!(
        matches!(
            completed,
            Poll::Ready(Ok(ManagedCredentialAdoptionProgress::Pending))
        ),
        "a non-converged target never acknowledges or returns convergence"
    );
}

#[test]
fn pending_delivery_and_cancellation_never_call_repository() {
    // R1/R2/R3: five outcomes x three valid poll/cancel schedules x four
    // operations x 3^2 revision boundaries = 540 executions of the real Future.
    for outcome in 0..5 {
        for (defer, cancel) in [(false, false), (true, false), (true, true)] {
            for operation in 0..4 {
                for version in [0, 1, u64::MAX] {
                    for revision in [0, 1, u64::MAX] {
                        check_case(outcome, defer, cancel, version, revision, operation);
                    }
                }
            }
        }
    }
}

#[test]
fn repository_monitor_rejects_future_construction_without_polling() {
    // M1 merely constructing a repository Future is already forbidden. This
    // catches a monitor that defers its assertion until its async body is polled.
    assert!(std::panic::catch_unwind(|| RejectRepository.pending_managed_rollouts()).is_err());
}
