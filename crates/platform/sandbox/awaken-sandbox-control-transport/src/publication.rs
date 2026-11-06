// @formal entrypoint sandbox_control.single_publication_is_stale_fenced_non_wrapping_and_close_absorbing
//! Provider-neutral publication lifecycle shared by Sandbox implementations.

use std::sync::Arc;

use awaken_lease_contract::fence::SlotToken;
use awaken_sandbox_control::SandboxControlPublishError;
use awaken_sync::LeafLock;

/// Opaque identity of one process-local Sandbox control publication.
///
/// Identity is the kernel's allocation-backed [`SlotToken`] rather than a
/// number, so an exhausted counter cannot wrap and let a stale publisher match
/// a later publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxControlPublicationLease(SlotToken);

/// Shared task participant for a publication, including asynchronous setup.
/// Cancellation requests shutdown; only joining establishes completion.
#[derive(Clone, Default)]
pub struct SandboxControlPublicationTask {
    cancel: tokio_util::sync::CancellationToken,
    task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl SandboxControlPublicationTask {
    #[must_use]
    pub fn cancellation(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.clone()
    }

    /// Install before running setup. No spawned task can escape an in-flight
    /// join: start and join use the same slot, and close permanently cancels it.
    pub fn start(
        &self,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<(), SandboxControlPublishError> {
        let mut task = self
            .task
            .try_lock()
            .map_err(|_| SandboxControlPublishError)?;
        if self.cancel.is_cancelled() || task.is_some() {
            return Err(SandboxControlPublishError);
        }
        *task = Some(tokio::spawn(future));
        Ok(())
    }

    /// Cancellation of this future leaves the JoinHandle in its original slot.
    pub async fn close(&self) {
        self.cancel.cancel();
        let mut task = self.task.lock().await;
        if let Some(join) = task.as_mut() {
            let _ = join.await;
            task.take();
        }
    }

    /// Drop cannot await. Retain the publication until the same join completes;
    /// never detach an aborted task and release its resource identity early.
    /// Without a live runtime, leave cleanup to the registry's explicit dispose.
    pub fn close_after_drop(&self, cleanup: impl FnOnce() + Send + 'static) {
        self.cancel.cancel();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let state = self.clone();
            runtime.spawn(async move {
                state.close().await;
                cleanup();
            });
        }
    }
}

/// One provider-neutral publication slot shared by Sandbox implementations.
///
/// The slot owns only local lifecycle serialization: vacant -> active -> vacant,
/// or any open state -> permanently closed. Provider-specific channel, socket,
/// and cleanup state remains in `T`; durable Session authority stays outside
/// this contract.
///
/// The slot state is a leaf lock: every critical section reads the slot state
/// or replaces it whole, so a holder that panicked leaves a valid state.
/// Disposal therefore stays absorbing after a panic, and a poisoned slot still
/// admits only its exact current lease.
pub struct SandboxControlPublicationSlot<T> {
    state: LeafLock<PublicationSlotState<T>>,
}

enum PublicationSlotState<T> {
    Open(Option<(SandboxControlPublicationLease, T)>),
    Closed(Option<(SandboxControlPublicationLease, T)>),
}

impl<T> Default for SandboxControlPublicationSlot<T> {
    fn default() -> Self {
        Self {
            state: LeafLock::new(PublicationSlotState::Open(None)),
        }
    }
}

impl<T> SandboxControlPublicationSlot<T> {
    /// Reserve the sole open slot for `value`.
    pub fn reserve(
        &self,
        value: T,
    ) -> Result<SandboxControlPublicationLease, SandboxControlPublishError> {
        let mut state = self.state.lock();
        let PublicationSlotState::Open(active) = &mut *state else {
            return Err(SandboxControlPublishError);
        };
        if active.is_some() {
            return Err(SandboxControlPublishError);
        }
        let lease = SandboxControlPublicationLease(SlotToken::mint());
        *active = Some((lease.clone(), value));
        Ok(lease)
    }

    /// Whether `lease` still owns the sole active slot.
    #[must_use]
    pub fn owns(&self, lease: &SandboxControlPublicationLease) -> bool {
        matches!(
            &*self.state.lock(),
            PublicationSlotState::Open(Some((current, _))) if current == lease
        )
    }

    /// Mutate provider-local state only while `lease` is the current owner.
    pub fn with_current_mut<R>(
        &self,
        lease: &SandboxControlPublicationLease,
        update: impl FnOnce(&mut T) -> R,
    ) -> Result<R, SandboxControlPublishError> {
        self.with_matching_mut(lease, false, update)
    }

    /// Resource teardown only: the exact closing allocation still owns its
    /// cleanup state, but cannot publish or reserve another task.
    pub fn with_retained_mut<R>(
        &self,
        lease: &SandboxControlPublicationLease,
        cleanup: impl FnOnce(&mut T) -> R,
    ) -> Result<R, SandboxControlPublishError> {
        self.with_matching_mut(lease, true, cleanup)
    }

    fn with_matching_mut<R>(
        &self,
        lease: &SandboxControlPublicationLease,
        allow_closed: bool,
        update: impl FnOnce(&mut T) -> R,
    ) -> Result<R, SandboxControlPublishError> {
        let mut state = self.state.lock();
        if !allow_closed && matches!(*state, PublicationSlotState::Closed(_)) {
            return Err(SandboxControlPublishError);
        }
        let (PublicationSlotState::Open(Some((current, value)))
        | PublicationSlotState::Closed(Some((current, value)))) = &mut *state
        else {
            return Err(SandboxControlPublishError);
        };
        if *current != *lease {
            return Err(SandboxControlPublishError);
        }
        Ok(update(value))
    }

    /// Release only the exact current lease. A stale lease is inert.
    pub fn release(&self, lease: &SandboxControlPublicationLease) {
        let mut state = self.state.lock();
        let PublicationSlotState::Open(active) = &mut *state else {
            return;
        };
        if active.as_ref().is_some_and(|(current, _)| current == lease) {
            *active = None;
        }
    }

    /// Permanently close admission and retain the same cleanup participant for
    /// every closer, including a retry after a cancelled disposal future.
    /// Disposal remains absorbing even after another local task panicked while
    /// holding the slot: the leaf lock recovers the state.
    pub fn close(&self) -> Option<T>
    where
        T: Clone,
    {
        let mut state = self.state.lock();
        if let PublicationSlotState::Open(active) = &mut *state {
            *state = PublicationSlotState::Closed(active.take());
        }
        match &*state {
            PublicationSlotState::Closed(Some((_, value))) => Some(value.clone()),
            _ => None,
        }
    }

    /// Call only after the retained task was joined. Final resource cleanup and
    /// forgetting the participant are one synchronous transition, so no closer
    /// can return before teardown or repeat an inode cleanup after completion.
    pub fn finish_close(&self, cleanup: impl FnOnce(&mut T)) {
        let mut state = self.state.lock();
        if let PublicationSlotState::Closed(active) = &mut *state {
            if let Some((_, value)) = active {
                cleanup(value);
            }
            *active = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_slot_has_one_unforgeable_owner_and_absorbing_close() {
        /* Publication-slot decision table:
         * C1=vacant open slot; C2=current lease; C3=stale predecessor lease;
         * C4=close. E1=C1 reserves exactly once; E2=C2 alone may mutate or
         * release; E3=C3 cannot clear a replacement; E4=C4 returns the active
         * value and permanently rejects reserve. Rules PS1 C1=>E1;
         * PS2 C2=>E2; PS3 C3=>E3; PS4 C4=>E4. Allocation identity, rather
         * than a wrapping number, makes PS3 hold for arbitrarily many cycles.
         */
        let slot = SandboxControlPublicationSlot::default();
        let first = slot.reserve(1_u8).expect("PS1/E1");
        assert!(slot.reserve(2).is_err(), "PS1 only one active value");
        slot.with_current_mut(&first, |value| *value = 3)
            .expect("PS2/E2 current mutates");
        slot.release(&first);

        let second = slot.reserve(4).expect("PS1 replacement");
        slot.release(&first);
        assert!(slot.owns(&second), "PS3/E3 stale release is inert");
        assert_eq!(slot.close(), Some(4), "PS4/E4 returns active value");
        assert!(!slot.owns(&second), "PS4/E4 no active owner");
        assert!(slot.reserve(5).is_err(), "PS4/E4 close is absorbing");
        assert_eq!(slot.close(), Some(4), "PS4/E4 retains cleanup on retry");
        slot.release(&second);
        assert_eq!(
            slot.close(),
            Some(4),
            "PS4 stale release cannot lose cleanup"
        );
        assert!(slot.with_current_mut(&second, |_| ()).is_err());
        slot.finish_close(|value| assert_eq!(*value, 4));
        assert_eq!(slot.close(), None, "PS4 only finished cleanup is forgotten");
    }

    #[tokio::test]
    async fn publication_task_serializes_start_cancel_join_and_drop_cleanup() {
        // Task decision table: C1=empty/live/closed participant; C2=setup has
        // started but ignores cancellation at a barrier; C3=explicit close or
        // Drop; C4=closer cancelled. E1=start once only; E2=no cleanup before
        // actual join; E3=retry joins the retained handle; E4=closed cannot start.
        // T1 empty=>E1; T2 live+C2+C3=>E2; T3 C4=>E3; T4 closed=>E4.
        for drop_close in [false, true] {
            let state = SandboxControlPublicationTask::default();
            let release = Arc::new(tokio::sync::Notify::new());
            state
                .start({
                    let release = release.clone();
                    async move { release.notified().await }
                })
                .unwrap();
            assert!(state.start(async {}).is_err(), "T1 only one installed task");
            let (cleaned, cleanup) = tokio::sync::oneshot::channel();
            let first = if drop_close {
                state.close_after_drop(move || {
                    let _ = cleaned.send(());
                });
                None
            } else {
                drop(cleaned);
                Some(tokio::spawn({
                    let state = state.clone();
                    async move { state.close().await }
                }))
            };
            state.cancellation().cancelled().await;
            let mut cleanup = cleanup;
            if drop_close {
                assert!(
                    matches!(
                        cleanup.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                    ),
                    "T2 cleanup waits for join"
                );
            }
            if let Some(first) = first {
                first.abort();
                assert!(first.await.unwrap_err().is_cancelled());
            }
            let retry = state.close();
            tokio::pin!(retry);
            tokio::select! {
                biased;
                () = &mut retry => panic!("T2/T3 cannot report cancellation as completion"),
                () = std::future::ready(()) => {}
            }
            assert!(state.start(async {}).is_err(), "T4 late start rejected");
            release.notify_one();
            retry.await;
            if drop_close {
                cleanup.await.unwrap();
            }
            state.close().await;
            assert!(state.start(async {}).is_err(), "T4 completed is absorbing");
        }
    }

    #[test]
    fn poisoned_slot_retains_cleanup_but_never_readmits_publication() {
        // PS5 C1=panic under the authoritative slot lock; C2=close/retry;
        // E1=admission denied; E2=exact cleanup still available once; E3=foreign
        // lease cannot mutate cleanup even in the ordinary closing state.
        let slot = SandboxControlPublicationSlot::default();
        let lease = slot.reserve(7_u8).unwrap();
        let foreign = SandboxControlPublicationSlot::default()
            .reserve(9_u8)
            .unwrap();
        slot.close();
        assert!(slot.with_retained_mut(&foreign, |_| ()).is_err(), "PS5/E3");
        assert!(
            std::panic::catch_unwind(|| {
                let _ = slot.with_retained_mut(&lease, |_| panic!("synthetic cleanup panic"));
            })
            .is_err()
        );
        assert!(slot.reserve(8).is_err(), "PS5/E1");
        assert_eq!(slot.close(), Some(7), "PS5/E2 retry retains participant");
        slot.finish_close(|value| assert_eq!(*value, 7));
        slot.finish_close(|_| panic!("PS5 cleanup repeated"));
        assert_eq!(slot.close(), None);
    }

    #[test]
    fn panic_under_an_open_slot_keeps_the_exact_owner() {
        // PS6 C1=panic under the slot lock while the slot is open (leaf lock);
        // E1=the current lease still owns and mutates; E2=a second reserve is
        // refused while the owner is active; E3=after release a new reserve is
        // admitted. The slot state is replaced whole, so poison carries no
        // information about it.
        let slot = SandboxControlPublicationSlot::default();
        let lease = slot.reserve(1_u8).expect("PS6 reserve");
        assert!(
            std::panic::catch_unwind(|| {
                let _ = slot.with_current_mut(&lease, |_| panic!("synthetic update panic"));
            })
            .is_err()
        );
        assert!(slot.owns(&lease), "PS6/E1 owner kept");
        let value = slot
            .with_current_mut(&lease, |value| *value)
            .expect("PS6/E1 owner mutates");
        assert_eq!(value, 1, "PS6/E1 owner reads the value it reserved");
        assert!(slot.reserve(2).is_err(), "PS6/E2 one active value");
        slot.release(&lease);
        assert!(slot.reserve(3).is_ok(), "PS6/E3 vacant slot readmits");
    }

    #[test]
    fn drop_outside_runtime_leaves_the_original_registry_responsible() {
        // T7 C1=task installed on a live runtime; C2=Drop outside an entered
        // runtime. E1=cancel requested without panicking or falsely invoking
        // cleanup; E2=explicit registry disposal later joins that same task.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let slot = SandboxControlPublicationSlot::default();
        let state = SandboxControlPublicationTask::default();
        slot.reserve(state.clone()).unwrap();
        let cancel = state.cancellation();
        runtime.block_on(async {
            state
                .start(async move { cancel.cancelled().await })
                .unwrap();
        });
        state.close_after_drop(|| panic!("T7 no runtime cannot claim cleanup"));
        assert!(state.cancellation().is_cancelled());
        runtime.block_on(async {
            slot.close().unwrap().close().await;
            slot.finish_close(|_| {});
        });
        assert!(slot.close().is_none());
    }

    #[tokio::test]
    async fn unstarted_and_panicked_publications_have_joinable_terminal_states() {
        // T5 no task + close => terminal without spawning; T6 task panic +
        // close => JoinError consumed, repeated close terminates, no restart.
        // Panic is not successful service execution; this proves only quiescence.
        for panics in [false, true] {
            let state = SandboxControlPublicationTask::default();
            if panics {
                state
                    .start(async { panic!("synthetic publication failure") })
                    .unwrap();
            }
            state.close().await;
            state.close().await;
            assert!(state.start(async {}).is_err());
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    /// @exhaustive no symbolic input (1) = 1
    /// @obligation sandbox_control.single_publication_is_stale_fenced_non_wrapping_and_close_absorbing
    #[test]
    fn publication_slot_directly_preserves_single_owner_stale_fence_and_absorbing_close() {
        // Cause/effect decision table over the production slot itself:
        // S1 Open+vacant reserves one owner and rejects a second; S2 Open+active
        // admits current mutation/release but rejects a foreign owner; S3 after
        // release and replacement, the retained predecessor cannot mutate or
        // release the replacement and its allocation identity is never reused;
        // S4 Close(active) retains exactly that value for cleanup and blocks all
        // ordinary owner operations; S5 repeated close retains it until the one
        // finish, then every close is empty; S6 Close(vacant) is also absorbing.
        // These rows cover every non-poisoned PublicationSlotState shape.
        let slot = SandboxControlPublicationSlot::default();
        let first = slot.reserve(1_u8).unwrap();
        assert!(slot.owns(&first), "S1 current owner");
        assert!(slot.reserve(2).is_err(), "S1 one publication");
        slot.with_current_mut(&first, |value| *value = 3).unwrap();

        let foreign_slot = SandboxControlPublicationSlot::default();
        let foreign = foreign_slot.reserve(9_u8).unwrap();
        assert!(!slot.owns(&foreign), "S2 foreign owner");
        assert!(slot.with_current_mut(&foreign, |_| ()).is_err(), "S2");
        slot.release(&foreign);
        assert!(slot.owns(&first), "S2 foreign release stutters");

        slot.release(&first);
        let second = slot.reserve(4_u8).unwrap();
        assert!(first != second, "S3 no identity reuse");
        slot.release(&first);
        assert!(slot.owns(&second), "S3 stale release stutters");
        assert!(slot.with_current_mut(&first, |_| ()).is_err(), "S3");

        assert_eq!(slot.close(), Some(4), "S4 retained cleanup value");
        assert!(!slot.owns(&second), "S4 no ordinary owner after close");
        assert!(slot.reserve(5).is_err(), "S4 no reserve after close");
        assert!(slot.with_current_mut(&second, |_| ()).is_err(), "S4");
        assert!(slot.with_retained_mut(&foreign, |_| ()).is_err(), "S4");
        slot.with_retained_mut(&second, |value| *value = 6).unwrap();
        slot.release(&second);
        assert_eq!(slot.close(), Some(6), "S5 close retry retains value");
        slot.finish_close(|value| assert_eq!(*value, 6));
        assert_eq!(slot.close(), None, "S5 one finish forgets value");
        slot.finish_close(|_| panic!("S5 cleanup repeated"));

        let vacant = SandboxControlPublicationSlot::<u8>::default();
        assert_eq!(vacant.close(), None, "S6 vacant close");
        assert!(vacant.reserve(1).is_err(), "S6 absorbing close");
    }
}
