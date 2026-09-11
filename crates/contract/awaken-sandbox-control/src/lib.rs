//! Closed, provider-neutral control protocol for one live Sandbox.
//!
//! This crate owns only the bounded request/response codec and the publication
//! ports. It owns no Session, claim, credential, repository, policy, cache, or
//! persistence state. A [`SandboxControlService`] must resolve every request
//! against its owning application's current authority.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

/// Hard upper bound for one control request or response.
pub const MAX_SANDBOX_CONTROL_FRAME_BYTES: usize = 64 * 1024;

/// Runtime-owned rendezvous directory. It is outside the repository/workspace
/// namespace and is projected read-only into untrusted workloads.
pub const SANDBOX_CONTROL_DIRECTORY_PATH: &str = "/run/awaken/control";

/// One stable in-Sandbox endpoint shared by Namespace and hosted Kubernetes.
/// Providers project it differently outside the Sandbox, but Agent/Hand
/// processes inherit one transport-independent helper overlay.
pub const REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH: &str =
    "/run/awaken/control/repository-git-credential.sock";

/// Sidecar-private readiness evidence adjacent to the stable logical socket.
/// The workload can read but never write this file; only the trusted forwarder
/// creates it after both of its listeners are bound.
pub const REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH: &str =
    "/run/awaken/control/repository-git-credential.ready";

/// Closed service families a Sandbox provider may publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxControlServiceKind {
    RepositoryGitCredential,
}

/// Exact rewritten HTTPS coordinate supplied by Git's credential-helper wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryGitCredentialQuery {
    pub protocol: String,
    pub host: String,
    pub path: String,
}

impl RepositoryGitCredentialQuery {
    /// Reject ambiguous or line-oriented values before they reach an authority.
    pub fn validate(&self) -> Result<(), SandboxControlProtocolError> {
        if self.protocol != "https"
            || self.host.trim().is_empty()
            || self.path.trim_matches('/').is_empty()
            || [&self.protocol, &self.host, &self.path]
                .into_iter()
                .any(|value| {
                    value
                        .chars()
                        .any(|character| matches!(character, '\r' | '\n' | '\0'))
                })
        {
            return Err(SandboxControlProtocolError::InvalidMessage);
        }
        Ok(())
    }
}

/// One request on the closed Sandbox control port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxControlRequest {
    RepositoryGitCredentialGet { query: RepositoryGitCredentialQuery },
}

/// Exact wall-clock upper bound supplied by the capability issuer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityExpiresAtUnixMs(pub u64);

impl CapabilityExpiresAtUnixMs {
    #[must_use]
    pub const fn is_live_at(self, now_unix_ms: u64) -> bool {
        self.0 > now_unix_ms
    }
}

/// Secret bytes used only on the private, process-local Sandbox control wire.
///
/// Unlike durable/public contracts, this type is intentionally serializable so
/// the provider-neutral transport can deliver one freshly verified capability
/// to Git. It is redacted in operator surfaces and zeroized on drop.
#[derive(Serialize)]
#[serde(transparent)]
pub struct SandboxControlSecret(String);

impl SandboxControlSecret {
    pub fn new(value: impl Into<String>) -> Result<Self, SandboxControlProtocolError> {
        let mut value = value.into();
        if value.is_empty()
            || value
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0'))
        {
            value.zeroize();
            return Err(SandboxControlProtocolError::InvalidMessage);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SandboxControlSecret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl Drop for SandboxControlSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SandboxControlSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SandboxControlSecret([redacted])")
    }
}

/// One response from the closed Sandbox control port.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxControlResponse {
    RepositoryGitCredential {
        username: SandboxControlSecret,
        password: SandboxControlSecret,
        expires_at_unix_ms: CapabilityExpiresAtUnixMs,
    },
    Denied,
    Unavailable,
}

impl std::fmt::Debug for SandboxControlResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepositoryGitCredential {
                expires_at_unix_ms, ..
            } => formatter
                .debug_struct("RepositoryGitCredential")
                .field("username", &"[REDACTED]")
                .field("password", &"[REDACTED]")
                .field("expires_at_unix_ms", expires_at_unix_ms)
                .finish(),
            Self::Denied => formatter.write_str("Denied"),
            Self::Unavailable => formatter.write_str("Unavailable"),
        }
    }
}

/// Application-owned request handler. Implementations must perform a fresh
/// authority read; this port deliberately offers no seed/store/cache method.
#[async_trait]
pub trait SandboxControlService: Send + Sync {
    async fn handle(&self, request: SandboxControlRequest) -> SandboxControlResponse;
}

/// Session-owned publication lease. Closing it revokes the provider transport;
/// it does not mutate application authority.
#[async_trait]
pub trait PublishedSandboxControlService: Send + Sync {
    async fn close(&self);
}

/// Provider capability for publishing one application-owned control handler.
#[async_trait]
pub trait SandboxControlServicePublisher: Send + Sync {
    async fn publish_sandbox_control_service(
        &self,
        kind: SandboxControlServiceKind,
        service: Arc<dyn SandboxControlService>,
    ) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError>;
}

#[derive(Debug, thiserror::Error)]
#[error("Sandbox control service publication is unavailable")]
pub struct SandboxControlPublishError;

/// Opaque identity of one process-local Sandbox control publication.
///
/// Identity is allocation-backed rather than numeric, so an exhausted counter
/// cannot wrap and let a stale publisher match a later publication.
#[derive(Clone)]
pub struct SandboxControlPublicationLease(Arc<()>);

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
pub struct SandboxControlPublicationSlot<T> {
    state: std::sync::Mutex<PublicationSlotState<T>>,
}

enum PublicationSlotState<T> {
    Open(Option<(SandboxControlPublicationLease, T)>),
    Closed(Option<(SandboxControlPublicationLease, T)>),
}

impl<T> Default for SandboxControlPublicationSlot<T> {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(PublicationSlotState::Open(None)),
        }
    }
}

impl<T> SandboxControlPublicationSlot<T> {
    /// Reserve the sole open slot for `value`.
    pub fn reserve(
        &self,
        value: T,
    ) -> Result<SandboxControlPublicationLease, SandboxControlPublishError> {
        let mut state = self.state.lock().map_err(|_| SandboxControlPublishError)?;
        let PublicationSlotState::Open(active) = &mut *state else {
            return Err(SandboxControlPublishError);
        };
        if active.is_some() {
            return Err(SandboxControlPublishError);
        }
        let lease = SandboxControlPublicationLease(Arc::new(()));
        *active = Some((lease.clone(), value));
        Ok(lease)
    }

    /// Whether `lease` still owns the sole active slot.
    #[must_use]
    pub fn owns(&self, lease: &SandboxControlPublicationLease) -> bool {
        self.state.lock().is_ok_and(|state| {
            matches!(
                &*state,
                PublicationSlotState::Open(Some((current, _)))
                    if Arc::ptr_eq(&current.0, &lease.0)
            )
        })
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
        let mut state = self.state.lock().map_err(|_| SandboxControlPublishError)?;
        if !allow_closed && matches!(*state, PublicationSlotState::Closed(_)) {
            return Err(SandboxControlPublishError);
        }
        let (PublicationSlotState::Open(Some((current, value)))
        | PublicationSlotState::Closed(Some((current, value)))) = &mut *state
        else {
            return Err(SandboxControlPublishError);
        };
        if !Arc::ptr_eq(&current.0, &lease.0) {
            return Err(SandboxControlPublishError);
        }
        Ok(update(value))
    }

    /// Release only the exact current lease. A stale lease is inert.
    pub fn release(&self, lease: &SandboxControlPublicationLease) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let PublicationSlotState::Open(active) = &mut *state else {
            return;
        };
        if active
            .as_ref()
            .is_some_and(|(current, _)| Arc::ptr_eq(&current.0, &lease.0))
        {
            *active = None;
        }
    }

    /// Permanently close admission and retain the same cleanup participant for
    /// every closer, including a retry after a cancelled disposal future.
    /// Poison recovery is deliberate here: disposal must remain absorbing even
    /// after another local task panicked while holding the mutex.
    pub fn close(&self) -> Option<T>
    where
        T: Clone,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let PublicationSlotState::Closed(active) = &mut *state {
            if let Some((_, value)) = active {
                cleanup(value);
            }
            *active = None;
        }
    }
}

/// Stable framing failures. Payload bytes and serde diagnostics are omitted so
/// a credential-bearing response cannot enter an error or log accidentally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SandboxControlProtocolError {
    #[error("Sandbox control frame is invalid")]
    InvalidFrame,
    #[error("Sandbox control message is invalid")]
    InvalidMessage,
    #[error("Sandbox control transport is unavailable")]
    Transport,
}

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), SandboxControlProtocolError>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize,
{
    let payload = Zeroizing::new(
        serde_json::to_vec(value).map_err(|_| SandboxControlProtocolError::InvalidMessage)?,
    );
    if payload.len() > MAX_SANDBOX_CONTROL_FRAME_BYTES {
        return Err(SandboxControlProtocolError::InvalidFrame);
    }
    let length =
        u32::try_from(payload.len()).map_err(|_| SandboxControlProtocolError::InvalidFrame)?;
    writer
        .write_all(&length.to_be_bytes())
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)?;
    writer
        .write_all(&payload)
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)?;
    writer
        .flush()
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, SandboxControlProtocolError>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut encoded_length = [0_u8; 4];
    reader
        .read_exact(&mut encoded_length)
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)?;
    let length = usize::try_from(u32::from_be_bytes(encoded_length))
        .ok()
        .filter(|length| *length <= MAX_SANDBOX_CONTROL_FRAME_BYTES)
        .ok_or(SandboxControlProtocolError::InvalidFrame)?;
    let mut payload = Zeroizing::new(vec![0_u8; length]);
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)?;
    serde_json::from_slice(&payload).map_err(|_| SandboxControlProtocolError::InvalidMessage)
}

/// Serve exactly one request/response exchange on a provider-owned stream.
pub async fn serve_one<C>(
    channel: &mut C,
    service: &dyn SandboxControlService,
) -> Result<(), SandboxControlProtocolError>
where
    C: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let request: SandboxControlRequest = read_frame(channel).await?;
    match &request {
        SandboxControlRequest::RepositoryGitCredentialGet { query } => query.validate()?,
    }
    let response = service.handle(request).await;
    write_frame(channel, &response).await
}

struct StartedSandboxControlChannel<'a, C: ?Sized> {
    first_byte: Option<u8>,
    channel: &'a mut C,
}

impl<C> AsyncRead for StartedSandboxControlChannel<'_, C>
where
    C: AsyncRead + Unpin + ?Sized,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if buffer.remaining() > 0
            && let Some(first_byte) = self.first_byte.take()
        {
            buffer.put_slice(&[first_byte]);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut *self.channel).poll_read(context, buffer)
    }
}

impl<C> AsyncWrite for StartedSandboxControlChannel<'_, C>
where
    C: AsyncWrite + Unpin + ?Sized,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut *self.channel).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.channel).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.channel).poll_shutdown(context)
    }
}

/// Complete one exchange after a provider has observed its first request byte.
///
/// Providers use this to distinguish an indefinitely idle, pre-opened channel
/// from a bounded active exchange without duplicating the framing codec.
pub async fn serve_one_after_first_byte<C>(
    channel: &mut C,
    first_byte: u8,
    service: &dyn SandboxControlService,
) -> Result<(), SandboxControlProtocolError>
where
    C: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    serve_one(
        &mut StartedSandboxControlChannel {
            first_byte: Some(first_byte),
            channel,
        },
        service,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingService(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl SandboxControlService for CountingService {
        async fn handle(&self, _request: SandboxControlRequest) -> SandboxControlResponse {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            SandboxControlResponse::Unavailable
        }
    }

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

    #[tokio::test]
    async fn bounded_codec_is_closed_and_redacts_credential_debug() {
        /*
         * R8/R11/R14 cause/effect decision table:
         * C1=request is exact HTTPS host/path; C2=unknown message field or
         * invalid protocol; C3=frame is over the fixed bound; C4=response holds
         * a virtual capability. E1=lossless one-request decode; E2=reject before
         * handler; E3=no allocation beyond the bound; E4=Debug/error reveal no
         * username/password while the private wire remains lossless.
         * Rules: R8 C1&&!C2=>E1; R11 C2=>E2; R14 C3=>E3, C4=>E4.
         */
        let request = SandboxControlRequest::RepositoryGitCredentialGet {
            query: RepositoryGitCredentialQuery {
                protocol: "https".into(),
                host: "gateway.example.test".into(),
                path: "git/repository-a".into(),
            },
        };
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        write_frame(&mut writer, &request).await.unwrap();
        let decoded: SandboxControlRequest = read_frame(&mut reader).await.unwrap();
        assert_eq!(decoded, request, "R8/E1");

        let service = CountingService(std::sync::atomic::AtomicUsize::new(0));
        for invalid in [
            serde_json::json!({
                "type": "repository_git_credential_get",
                "query": {
                    "protocol": "http",
                    "host": "gateway.example.test",
                    "path": "git/repository-a"
                }
            }),
            serde_json::json!({
                "type": "repository_git_credential_get",
                "query": {
                    "protocol": "https",
                    "host": "gateway.example.test",
                    "path": "git/repository-a",
                    "unknown": true
                }
            }),
        ] {
            let (mut client, mut server) = tokio::io::duplex(1024);
            write_frame(&mut client, &invalid).await.unwrap();
            assert!(serve_one(&mut server, &service).await.is_err(), "R11/E2");
        }
        assert_eq!(
            service.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "R11/E2 handler never observes a rejected request"
        );

        let credential = SandboxControlResponse::RepositoryGitCredential {
            username: SandboxControlSecret::new("git-user").unwrap(),
            password: SandboxControlSecret::new("virtual-capability").unwrap(),
            expires_at_unix_ms: CapabilityExpiresAtUnixMs(500),
        };
        let debug = format!("{credential:?}");
        assert!(!debug.contains("git-user"), "R14/E4");
        assert!(!debug.contains("virtual-capability"), "R14/E4");
        let encoded = serde_json::to_vec(&credential).unwrap();
        let decoded: SandboxControlResponse = serde_json::from_slice(&encoded).unwrap();
        let SandboxControlResponse::RepositoryGitCredential {
            username, password, ..
        } = decoded
        else {
            panic!("R14/E4 credential response")
        };
        assert_eq!(username.expose_secret(), "git-user", "R14/E4 private wire");
        assert_eq!(
            password.expose_secret(),
            "virtual-capability",
            "R14/E4 private wire"
        );

        let oversized = u32::try_from(MAX_SANDBOX_CONTROL_FRAME_BYTES + 1)
            .unwrap()
            .to_be_bytes();
        let (mut writer, mut reader) = tokio::io::duplex(8);
        writer.write_all(&oversized).await.unwrap();
        assert_eq!(
            read_frame::<_, SandboxControlRequest>(&mut reader).await,
            Err(SandboxControlProtocolError::InvalidFrame),
            "R14/E3"
        );
    }

    #[test]
    fn credential_query_rejects_non_https_and_line_injection() {
        /* Credential validation table: C1=canonical HTTPS authority and
         * nonempty LF/CR/NUL-free secret; C2=non-HTTPS/empty/query injection;
         * C3=wire-deserialized empty, CRLF, or NUL secret; C4=valid credential
         * response is formatted for diagnostics. E1=C1 validates; E2=C2 and
         * C3 reject before use; E3=C4 redacts both fields. Rules CV1 C1=>E1;
         * CV2 C2=>E2; CV3 C3=>E2; CV4 C1+C4=>E3.
         */
        let query = |protocol: &str, host: &str, path: &str| RepositoryGitCredentialQuery {
            protocol: protocol.into(),
            host: host.into(),
            path: path.into(),
        };
        assert!(
            query("https", "gateway.test", "git/repo")
                .validate()
                .is_ok()
        );
        assert!(
            query("http", "gateway.test", "git/repo")
                .validate()
                .is_err()
        );
        assert!(
            query("https", "gateway.test\npassword=x", "git/repo")
                .validate()
                .is_err()
        );
        assert!(query("https", "gateway.test", "").validate().is_err());
        assert!(SandboxControlSecret::new("line\ninjection").is_err());
        assert!(SandboxControlSecret::new("").is_err());

        for encoded in [r#""""#, r#""line\r\nbreak""#, r#""prefix\u0000suffix""#] {
            assert!(
                serde_json::from_str::<SandboxControlSecret>(encoded).is_err(),
                "CV3/E2 {encoded}"
            );
        }
        let response: SandboxControlResponse = serde_json::from_value(serde_json::json!({
            "type": "repository_git_credential",
            "username": "git-user",
            "password": "valid-capability", // awaken-allow: secret -- inert redaction fixture
            "expires_at_unix_ms": 42
        }))
        .unwrap();
        let debug = format!("{response:?}");
        assert!(debug.contains("REDACTED"), "CV4/E3 marker");
        assert!(!debug.contains("git-user"), "CV4/E3 username");
        assert!(!debug.contains("valid-capability"), "CV4/E3 password");
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
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
        assert!(!Arc::ptr_eq(&first.0, &second.0), "S3 no identity reuse");
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
