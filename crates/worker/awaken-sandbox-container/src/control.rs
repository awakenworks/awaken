//! Provider-owned publication lifecycle for one container Sandbox control port.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_sandbox_control::{
    PublishedSandboxControlService, SandboxControlPublishError, SandboxControlService,
    SandboxControlServiceKind, SandboxControlServicePublisher, serve_one_after_first_byte,
};
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;

use crate::{ContainerRuntime, ContainerSandbox};

const FIRST_CHANNEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const CONTROL_EXCHANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CONTROL_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_millis(50);
const CONTROL_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone)]
struct ContainerControlPublicationState {
    generation: u64,
    cancel: CancellationToken,
    task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl ContainerControlPublicationState {
    async fn close_task(&self) {
        self.cancel.cancel();
        // Keep the slot locked until the one task has actually stopped. `None`
        // therefore means completed/never installed, never "another closer is
        // still joining", so disposal cannot remove the runtime early.
        let mut task_slot = self.task.lock().await;
        if let Some(task) = task_slot.as_mut() {
            let _ = task.await;
            task_slot.take();
        }
    }

    fn abort_task(&self) {
        self.cancel.cancel();
        if let Ok(mut task) = self.task.try_lock()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}

async fn wait_before_reopen(cancel: &CancellationToken, delay: &mut std::time::Duration) -> bool {
    let keep_running = tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(*delay) => true,
    };
    *delay = delay.saturating_mul(2).min(CONTROL_RETRY_MAX);
    keep_running
}

async fn open_channel<R: ContainerRuntime + 'static>(
    runtime: &R,
    container_id: &str,
    binding: &awaken_provisioning_contract::SandboxControlIncarnation,
    kind: SandboxControlServiceKind,
    cancel: &CancellationToken,
) -> Result<Box<dyn AgentChannel>, SandboxControlPublishError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(SandboxControlPublishError),
        opened = tokio::time::timeout(
            FIRST_CHANNEL_TIMEOUT,
            runtime.open_sandbox_control_channel(container_id, binding, kind),
        ) => opened
            .map_err(|_| SandboxControlPublishError)
            .and_then(|result| result.map_err(|_| SandboxControlPublishError)),
    }
}

#[derive(Default)]
pub(crate) struct ContainerControlPublicationRegistry {
    next_generation: std::sync::atomic::AtomicU64,
    disposed: std::sync::atomic::AtomicBool,
    active: std::sync::Mutex<Option<ContainerControlPublicationState>>,
}

impl ContainerControlPublicationRegistry {
    fn acquire(&self) -> Result<ContainerControlPublicationState, SandboxControlPublishError> {
        if self.disposed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(SandboxControlPublishError);
        }
        let mut active = self.active.lock().map_err(|_| SandboxControlPublishError)?;
        if self.disposed.load(std::sync::atomic::Ordering::Acquire) || active.is_some() {
            return Err(SandboxControlPublishError);
        }
        let state = ContainerControlPublicationState {
            generation: self
                .next_generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .wrapping_add(1),
            cancel: CancellationToken::new(),
            task: Arc::new(tokio::sync::Mutex::new(None)),
        };
        *active = Some(state.clone());
        Ok(state)
    }

    fn owns(&self, generation: u64) -> bool {
        !self.disposed.load(std::sync::atomic::Ordering::Acquire)
            && self.active.lock().is_ok_and(|active| {
                active
                    .as_ref()
                    .is_some_and(|state| state.generation == generation)
            })
    }

    fn release(&self, generation: u64) {
        if let Ok(mut active) = self.active.lock()
            && active
                .as_ref()
                .is_some_and(|state| state.generation == generation)
        {
            *active = None;
        }
    }

    pub(crate) async fn close_for_dispose(&self) {
        self.disposed
            .store(true, std::sync::atomic::Ordering::Release);
        let state = self.active.lock().ok().and_then(|active| active.clone());
        if let Some(state) = state {
            state.close_task().await;
            self.release(state.generation);
        }
    }
}

struct ContainerControlPublication {
    registry: Arc<ContainerControlPublicationRegistry>,
    state: ContainerControlPublicationState,
}

impl Drop for ContainerControlPublication {
    fn drop(&mut self) {
        self.state.abort_task();
        self.registry.release(self.state.generation);
    }
}

#[async_trait]
impl PublishedSandboxControlService for ContainerControlPublication {
    async fn close(&self) {
        self.state.close_task().await;
        self.registry.release(self.state.generation);
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> SandboxControlServicePublisher for ContainerSandbox<R> {
    async fn publish_sandbox_control_service(
        &self,
        kind: SandboxControlServiceKind,
        service: Arc<dyn SandboxControlService>,
    ) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError> {
        if !self.control_services.contains(&kind)
            || !self.runtime.sandbox_control_services().contains(&kind)
        {
            return Err(SandboxControlPublishError);
        }
        let state = self.control_publication.acquire()?;
        let binding = self.sandbox_control_incarnation.clone().ok_or_else(|| {
            self.control_publication.release(state.generation);
            SandboxControlPublishError
        })?;
        let first = open_channel(
            self.runtime.as_ref(),
            &self.container_id,
            &binding,
            kind,
            &state.cancel,
        )
        .await;
        let first = match first {
            Ok(channel) if self.control_publication.owns(state.generation) => channel,
            Ok(_) | Err(_) => {
                self.control_publication.release(state.generation);
                return Err(SandboxControlPublishError);
            }
        };

        let runtime = self.runtime.clone();
        let container_id = self.container_id.clone();
        let task_cancel = state.cancel.clone();
        let task = tokio::spawn(async move {
            let mut next_channel = Some(first);
            let mut retry_delay = CONTROL_RETRY_INITIAL;
            loop {
                let mut channel = match next_channel.take() {
                    Some(channel) => channel,
                    None => match open_channel(
                        runtime.as_ref(),
                        &container_id,
                        &binding,
                        kind,
                        &task_cancel,
                    )
                    .await
                    {
                        Ok(channel) => channel,
                        Err(_) if wait_before_reopen(&task_cancel, &mut retry_delay).await => {
                            continue;
                        }
                        Err(_) => break,
                    },
                };

                // A pre-opened Pod-forwarded channel may remain idle for the
                // entire Session. Idle is not an exchange timeout: wait for the
                // first frame byte (or cancellation) without a wall-clock cap.
                let mut first_byte = [0_u8; 1];
                let active = tokio::select! {
                    biased;
                    () = task_cancel.cancelled() => break,
                    result = channel.read_exact(&mut first_byte) => result.is_ok(),
                };
                if !active {
                    if !wait_before_reopen(&task_cancel, &mut retry_delay).await {
                        break;
                    }
                    continue;
                }

                let served = tokio::select! {
                    biased;
                    () = task_cancel.cancelled() => break,
                    result = tokio::time::timeout(
                        CONTROL_EXCHANGE_TIMEOUT,
                        serve_one_after_first_byte(
                            &mut *channel,
                            first_byte[0],
                            service.as_ref(),
                        ),
                    ) => matches!(result, Ok(Ok(()))),
                };
                if task_cancel.is_cancelled() {
                    break;
                }
                if served {
                    retry_delay = CONTROL_RETRY_INITIAL;
                } else if !wait_before_reopen(&task_cancel, &mut retry_delay).await {
                    break;
                }
            }
        });
        {
            let mut task_slot = state.task.lock().await;
            if state.cancel.is_cancelled() || !self.control_publication.owns(state.generation) {
                task.abort();
                self.control_publication.release(state.generation);
                return Err(SandboxControlPublishError);
            }
            *task_slot = Some(task);
        }
        Ok(Box::new(ContainerControlPublication {
            registry: self.control_publication.clone(),
            state,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generation_release_is_fenced_and_disposal_is_irreversible() {
        /* Provider lifecycle cause/effect table:
         * C1=one active generation; C2=a stale generation releases after a
         * replacement; C3=environment disposal. E1=second publication denied;
         * E2=stale release cannot clear the replacement; E3=all later acquire
         * attempts fail. Rules: P1 C1=>E1; P2 C2=>E2; P3 C3=>E3.
         */
        let registry = ContainerControlPublicationRegistry::default();
        let first = registry.acquire().unwrap();
        assert!(registry.acquire().is_err(), "P1/E1");
        registry.release(first.generation);
        let second = registry.acquire().unwrap();
        registry.release(first.generation);
        assert!(registry.owns(second.generation), "P2/E2");
        registry.close_for_dispose().await;
        assert!(registry.acquire().is_err(), "P3/E3");
    }

    #[tokio::test]
    async fn concurrent_close_and_dispose_wait_for_the_same_join_completion() {
        /* Close-join cause/effect table:
         * C1=one task ignores cancellation and waits at a barrier; C2=the first
         * closer is awaiting its JoinHandle; C3=that close future is cancelled;
         * C4=dispose arrives. E1=the JoinHandle remains in the state; E2=dispose
         * stays pending (and cannot remove/reopen) until the task exits.
         * Rule P4 C1+C2+C3+C4=>E1+E2.
         */
        let registry = Arc::new(ContainerControlPublicationRegistry::default());
        let state = registry.acquire().unwrap();
        let release = Arc::new(tokio::sync::Notify::new());
        *state.task.lock().await = Some(tokio::spawn({
            let release = release.clone();
            async move { release.notified().await }
        }));

        let first = tokio::spawn({
            let state = state.clone();
            async move { state.close_task().await }
        });
        while state.task.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
        first.abort();
        assert!(
            first.await.unwrap_err().is_cancelled(),
            "P4 cancelled close"
        );
        assert!(state.task.lock().await.is_some(), "P4/E1");
        let disposal = tokio::spawn({
            let registry = registry.clone();
            async move { registry.close_for_dispose().await }
        });
        tokio::task::yield_now().await;
        assert!(!disposal.is_finished(), "P4/E1");
        release.notify_one();
        disposal.await.unwrap();
        assert!(state.task.lock().await.is_none(), "P4/E2");
        assert!(registry.acquire().is_err(), "P4 disposal is irreversible");
    }
}
