//! Linux Namespace realization of the provider-neutral Sandbox control port.

use super::*;
use awaken_sandbox_control::{
    PublishedSandboxControlService, SANDBOX_CONTROL_DIRECTORY_PATH, SandboxControlPublicationLease,
    SandboxControlPublicationSlot, SandboxControlPublishError, SandboxControlService,
    SandboxControlServiceKind, SandboxControlServicePublisher,
};
#[cfg(target_os = "linux")]
use awaken_sandbox_control::{REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH, serve_one};
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
const PRIVATE_CONTROL_HOST_PARENT: &str = ".awaken-control";

#[cfg(target_os = "linux")]
fn ensure_private_directory(path: &std::path::Path) -> Result<(), pc::SandboxError> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(err(
                "Namespace control rendezvous is not a private directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path).map_err(err)?;
        }
        Err(error) => return Err(err(error)),
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(err)?;
    let metadata = std::fs::symlink_metadata(path).map_err(err)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || metadata.mode() & 0o777 != 0o700 {
        return Err(err(
            "Namespace control rendezvous lost its private directory identity",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn private_control_host_directory(
    root: &IsolatedRoot,
) -> Result<PathBuf, pc::SandboxError> {
    let root_path = root.root();
    let parent = root_path
        .parent()
        .ok_or_else(|| err("Namespace root has no trusted parent"))?;
    let sandbox_name = root_path
        .file_name()
        .ok_or_else(|| err("Namespace root has no stable name"))?;
    let rendezvous_parent = parent.join(PRIVATE_CONTROL_HOST_PARENT);
    ensure_private_directory(&rendezvous_parent)?;
    let host = rendezvous_parent.join(sandbox_name);
    ensure_private_directory(&host)?;
    Ok(host)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn private_control_host_directory(
    _root: &IsolatedRoot,
) -> Result<PathBuf, pc::SandboxError> {
    Err(err(
        "Namespace Sandbox control services require the Linux namespace provider",
    ))
}

pub(super) fn private_control_mount(host: PathBuf) -> RenderMount {
    RenderMount {
        host,
        dest: SANDBOX_CONTROL_DIRECTORY_PATH.into(),
        read_only: true,
        boundary: RenderMountBoundary::PrivateRendezvous,
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
fn socket_identity(path: &std::path::Path) -> Result<SocketIdentity, SandboxControlPublishError> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

    let metadata = std::fs::symlink_metadata(path).map_err(|_| SandboxControlPublishError)?;
    if !metadata.file_type().is_socket() {
        return Err(SandboxControlPublishError);
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(target_os = "linux")]
fn unlink_if_identity(path: &std::path::Path, expected: SocketIdentity) {
    if socket_identity(path).ok() == Some(expected) {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct NamespaceControlPublicationState {
    cancel: CancellationToken,
    task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    socket: PathBuf,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl NamespaceControlPublicationState {
    async fn close_task(&self) {
        self.cancel.cancel();
        // Serialize every closer through completion of the same JoinHandle.
        // `None` cannot be observed while another closer is still joining.
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

struct ActiveNamespaceControlPublication {
    state: NamespaceControlPublicationState,
    #[cfg(target_os = "linux")]
    owned_socket: Option<SocketIdentity>,
}

#[derive(Default)]
pub(super) struct NamespaceControlPublicationRegistry {
    slot: SandboxControlPublicationSlot<ActiveNamespaceControlPublication>,
}

impl NamespaceControlPublicationRegistry {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn reserve(
        &self,
        socket: PathBuf,
    ) -> Result<
        (
            SandboxControlPublicationLease,
            NamespaceControlPublicationState,
        ),
        SandboxControlPublishError,
    > {
        let state = NamespaceControlPublicationState {
            cancel: CancellationToken::new(),
            task: Arc::new(tokio::sync::Mutex::new(None)),
            socket,
        };
        let lease = self.slot.reserve(ActiveNamespaceControlPublication {
            state: state.clone(),
            #[cfg(target_os = "linux")]
            owned_socket: None,
        })?;
        Ok((lease, state))
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn owns(&self, lease: &SandboxControlPublicationLease) -> bool {
        self.slot.owns(lease)
    }

    #[cfg(target_os = "linux")]
    fn record_owned_socket(
        &self,
        lease: &SandboxControlPublicationLease,
        state: &NamespaceControlPublicationState,
        identity: SocketIdentity,
    ) -> Result<(), SandboxControlPublishError> {
        self.record_owned_socket_with(lease, state, identity, || {})
    }

    #[cfg(target_os = "linux")]
    fn record_owned_socket_with(
        &self,
        lease: &SandboxControlPublicationLease,
        state: &NamespaceControlPublicationState,
        identity: SocketIdentity,
        while_active_locked: impl FnOnce(),
    ) -> Result<(), SandboxControlPublishError> {
        // Generation admission and inode ownership become visible in one active
        // critical section. Disposal can observe neither an ownerless active
        // generation nor an owned inode after clearing that generation.
        self.slot.with_current_mut(lease, |current| {
            if state.cancel.is_cancelled() {
                return Err(SandboxControlPublishError);
            }
            while_active_locked();
            current.owned_socket = Some(identity);
            Ok(())
        })?
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn release_reservation(&self, lease: &SandboxControlPublicationLease) {
        self.slot.release(lease);
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn cleanup_owned_if_active(
        &self,
        lease: &SandboxControlPublicationLease,
        _state: &NamespaceControlPublicationState,
    ) {
        if self
            .slot
            .with_current_mut(lease, |_active| {
                #[cfg(target_os = "linux")]
                if let Some(identity) = _active.owned_socket.take() {
                    unlink_if_identity(&_state.socket, identity);
                }
            })
            .is_ok()
        {
            self.slot.release(lease);
        }
    }

    pub(super) async fn close_for_dispose(&self) {
        if let Some(active) = self.slot.close() {
            active.state.close_task().await;
            #[cfg(target_os = "linux")]
            if let Some(identity) = active.owned_socket {
                unlink_if_identity(&active.state.socket, identity);
            }
        }
    }
}

#[cfg(target_os = "linux")]
struct OwnedNamespaceControlListener {
    listener: tokio::net::UnixListener,
    registry: Arc<NamespaceControlPublicationRegistry>,
    lease: SandboxControlPublicationLease,
    state: NamespaceControlPublicationState,
}

#[cfg(target_os = "linux")]
impl Drop for OwnedNamespaceControlListener {
    fn drop(&mut self) {
        // `Drop::drop` runs before the listener field is released. Keeping that
        // descriptor alive prevents its inode from being recycled between the
        // ownership check and unlink; a replacement path therefore cannot be
        // mistaken for this generation even when the task exits unexpectedly.
        self.registry
            .cleanup_owned_if_active(&self.lease, &self.state);
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct NamespaceControlPublication {
    registry: Arc<NamespaceControlPublicationRegistry>,
    lease: SandboxControlPublicationLease,
    state: NamespaceControlPublicationState,
}

impl Drop for NamespaceControlPublication {
    fn drop(&mut self) {
        self.state.abort_task();
        self.registry
            .cleanup_owned_if_active(&self.lease, &self.state);
    }
}

#[async_trait]
impl PublishedSandboxControlService for NamespaceControlPublication {
    async fn close(&self) {
        self.state.close_task().await;
        self.registry
            .cleanup_owned_if_active(&self.lease, &self.state);
    }
}

#[cfg(target_os = "linux")]
async fn bind_namespace_control_listener(
    socket: &std::path::Path,
) -> Result<(tokio::net::UnixListener, SocketIdentity), SandboxControlPublishError> {
    match std::fs::symlink_metadata(socket) {
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt as _;
            if !metadata.file_type().is_socket() {
                return Err(SandboxControlPublishError);
            }
            let observed = socket_identity(socket)?;
            match tokio::time::timeout(
                std::time::Duration::from_millis(250),
                tokio::net::UnixStream::connect(socket),
            )
            .await
            {
                Ok(Ok(_)) | Err(_) => return Err(SandboxControlPublishError),
                Ok(Err(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    // The failed connect proves only the inode we observed is
                    // stale. A concurrent publisher may already have replaced
                    // the path, so never unlink by name alone.
                    unlink_if_identity(socket, observed);
                }
                Ok(Err(_)) => return Err(SandboxControlPublishError),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(SandboxControlPublishError),
    }
    let listener =
        tokio::net::UnixListener::bind(socket).map_err(|_| SandboxControlPublishError)?;
    let identity = socket_identity(socket)?;
    use std::os::unix::fs::PermissionsExt as _;
    if std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o777)).is_err() {
        unlink_if_identity(socket, identity);
        return Err(SandboxControlPublishError);
    }
    Ok((listener, identity))
}

#[cfg(target_os = "linux")]
async fn publish_namespace_control_service(
    sandbox: &NamespaceSandbox,
    service: Arc<dyn SandboxControlService>,
) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError> {
    let control_directory = sandbox
        .control_directory
        .as_ref()
        .ok_or(SandboxControlPublishError)?;
    let socket_name = std::path::Path::new(REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH)
        .file_name()
        .ok_or(SandboxControlPublishError)?;
    let socket = control_directory.join(socket_name);
    if socket.parent() != Some(control_directory.as_path()) {
        return Err(SandboxControlPublishError);
    }
    let (lease, state) = sandbox.control_publication.reserve(socket.clone())?;
    let (listener, identity) = match bind_namespace_control_listener(&socket).await {
        Ok(bound) => bound,
        Err(error) => {
            // Reservation never implies filesystem ownership. A bind failure
            // cannot unlink an ambient live socket it did not create.
            sandbox.control_publication.release_reservation(&lease);
            return Err(error);
        }
    };
    if sandbox
        .control_publication
        .record_owned_socket(&lease, &state, identity)
        .is_err()
    {
        unlink_if_identity(&socket, identity);
        sandbox.control_publication.release_reservation(&lease);
        return Err(SandboxControlPublishError);
    }
    let task_cancel = state.cancel.clone();
    let permits = Arc::new(tokio::sync::Semaphore::new(16));
    let owned_listener = OwnedNamespaceControlListener {
        listener,
        registry: sandbox.control_publication.clone(),
        lease: lease.clone(),
        state: state.clone(),
    };
    let task = tokio::spawn(async move {
        let mut requests = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                () = task_cancel.cancelled() => break,
                completed = requests.join_next(), if !requests.is_empty() => {
                    let _ = completed;
                }
                accepted = owned_listener.listener.accept() => {
                    let Ok((mut channel, _)) = accepted else {
                        break;
                    };
                    let Ok(permit) = permits.clone().try_acquire_owned() else {
                        continue;
                    };
                    let service = service.clone();
                    let request_cancel = task_cancel.clone();
                    requests.spawn(async move {
                        let _permit = permit;
                        tokio::select! {
                            biased;
                            () = request_cancel.cancelled() => {}
                            _ = tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                serve_one(&mut channel, service.as_ref()),
                            ) => {}
                        }
                    });
                }
            }
        }
        requests.abort_all();
        while requests.join_next().await.is_some() {}
    });
    {
        let mut task_slot = state.task.lock().await;
        if state.cancel.is_cancelled() || !sandbox.control_publication.owns(&lease) {
            task.abort();
            sandbox
                .control_publication
                .cleanup_owned_if_active(&lease, &state);
            return Err(SandboxControlPublishError);
        }
        *task_slot = Some(task);
    }
    Ok(Box::new(NamespaceControlPublication {
        registry: sandbox.control_publication.clone(),
        lease,
        state,
    }))
}

#[cfg(not(target_os = "linux"))]
async fn publish_namespace_control_service(
    _sandbox: &NamespaceSandbox,
    _service: Arc<dyn SandboxControlService>,
) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError> {
    Err(SandboxControlPublishError)
}

#[async_trait]
impl SandboxControlServicePublisher for NamespaceSandbox {
    async fn publish_sandbox_control_service(
        &self,
        kind: SandboxControlServiceKind,
        service: Arc<dyn SandboxControlService>,
    ) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError> {
        if kind != SandboxControlServiceKind::RepositoryGitCredential
            || !self.control_services.contains(&kind)
        {
            return Err(SandboxControlPublishError);
        }
        publish_namespace_control_service(self, service).await
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    struct DeniedService;

    #[async_trait]
    impl SandboxControlService for DeniedService {
        async fn handle(
            &self,
            _request: awaken_sandbox_control::SandboxControlRequest,
        ) -> awaken_sandbox_control::SandboxControlResponse {
            awaken_sandbox_control::SandboxControlResponse::Denied
        }
    }

    fn demanded_spec(scope: &str) -> pc::SandboxSpec {
        pc::SandboxSpec {
            scope: scope.into(),
            isolation: pc::IsolationClass::Namespace,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            control_services: std::collections::BTreeSet::from([
                SandboxControlServiceKind::RepositoryGitCredential,
            ]),
        }
    }

    #[tokio::test]
    async fn reservation_and_inode_fences_never_unlink_an_unowned_socket() {
        /* Namespace publication cause/effect table:
         * C1=reservation followed by a live foreign bind; C2=this generation
         * owns a socket whose listener FD remains live through cleanup;
         * C3=the path is replaced with a different inode;
         * C4=a stale generation closes after a replacement. E1=bind failure
         * releases only the reservation; E2=cleanup removes only matching
         * dev+ino; E3=replacement remains; E4=stale close cannot affect the
         * active generation. Rules: N1 C1=>E1; N2 C2=>E2; N3 C2+C3=>E3;
         * N4 C4=>E4.
         */
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        let registry = Arc::new(NamespaceControlPublicationRegistry::default());

        let foreign = tokio::net::UnixListener::bind(&socket).unwrap();
        let (reservation, _) = registry.reserve(socket.clone()).unwrap();
        assert!(
            bind_namespace_control_listener(&socket).await.is_err(),
            "N1"
        );
        registry.release_reservation(&reservation);
        assert!(socket.exists(), "N1/E1");
        drop(foreign);
        std::fs::remove_file(&socket).unwrap();

        let (first_lease, first) = registry.reserve(socket.clone()).unwrap();
        let (listener, identity) = bind_namespace_control_listener(&socket).await.unwrap();
        registry
            .record_owned_socket(&first_lease, &first, identity)
            .unwrap();
        let owned_listener = OwnedNamespaceControlListener {
            listener,
            registry: registry.clone(),
            lease: first_lease.clone(),
            state: first.clone(),
        };
        std::fs::remove_file(&socket).unwrap();
        let replacement = tokio::net::UnixListener::bind(&socket).unwrap();
        assert_ne!(
            socket_identity(&socket).unwrap(),
            identity,
            "N3 distinct live inode"
        );
        drop(owned_listener);
        assert!(socket.exists(), "N3/E3");

        let (second_lease, second) = registry.reserve(socket.clone()).unwrap();
        let replacement_identity = socket_identity(&socket).unwrap();
        registry
            .record_owned_socket(&second_lease, &second, replacement_identity)
            .unwrap();
        registry.cleanup_owned_if_active(&first_lease, &first);
        assert!(registry.owns(&second_lease), "N4/E4");
        assert!(socket.exists(), "N4/E4");
        drop(replacement);
        registry.cleanup_owned_if_active(&second_lease, &second);
    }

    #[tokio::test]
    async fn disposal_closes_the_active_generation_and_forbids_reopen() {
        /* N5: C1=active reserved generation; C2=environment disposal.
         * E1=active generation is cancelled; E2=every later reserve fails.
         */
        let registry = NamespaceControlPublicationRegistry::default();
        let directory = tempfile::tempdir().unwrap();
        let (_, state) = registry
            .reserve(directory.path().join("control.sock"))
            .unwrap();
        registry.close_for_dispose().await;
        assert!(state.cancel.is_cancelled(), "N5/E1");
        assert!(
            registry
                .reserve(directory.path().join("replacement.sock"))
                .is_err(),
            "N5/E2"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inode_registration_and_disposal_share_one_active_critical_section() {
        /* Registration/disposal race table:
         * C1=the generation has bound an inode; C2=registration pauses while
         * holding the active lock; C3=dispose attempts the absorbing close.
         * E1=dispose cannot clear an ownerless generation;
         * E2=after registration releases, dispose sees and unlinks that exact
         * dev+ino; E3=no stale socket remains. Rule N8 C1+C2+C3=>E1+E2+E3.
         */
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let identity = socket_identity(&socket).unwrap();
        let registry = Arc::new(NamespaceControlPublicationRegistry::default());
        let (lease, state) = registry.reserve(socket.clone()).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let recording = tokio::task::spawn_blocking({
            let registry = registry.clone();
            let lease = lease.clone();
            let state = state.clone();
            move || {
                registry.record_owned_socket_with(&lease, &state, identity, || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
            }
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("N8 registration owns active lock");

        let disposal = tokio::spawn({
            let registry = registry.clone();
            async move { registry.close_for_dispose().await }
        });
        tokio::task::yield_now().await;
        assert!(!disposal.is_finished(), "N8/E1");
        release_tx.send(()).unwrap();
        recording.await.unwrap().unwrap();
        disposal.await.unwrap();
        assert!(!socket.exists(), "N8/E2/E3");
        drop(listener);
    }

    #[tokio::test]
    async fn concurrent_namespace_close_and_dispose_wait_for_one_join_completion() {
        /* Close-join table: C1=the publication task ignores cancellation at a
         * barrier; C2=one close awaits its JoinHandle; C3=that close future is
         * cancelled; C4=dispose arrives. E1=the JoinHandle remains recorded and
         * C4 cannot remove/reopen; E2=dispose completes only after task exit.
         * Rule N9 C1+C2+C3+C4=>E1+E2.
         */
        let registry = Arc::new(NamespaceControlPublicationRegistry::default());
        let directory = tempfile::tempdir().unwrap();
        let (_, state) = registry
            .reserve(directory.path().join("control.sock"))
            .unwrap();
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
            "N9 cancelled close"
        );
        assert!(state.task.lock().await.is_some(), "N9/E1");
        let disposal = tokio::spawn({
            let registry = registry.clone();
            async move { registry.close_for_dispose().await }
        });
        tokio::task::yield_now().await;
        assert!(!disposal.is_finished(), "N9/E1");
        release.notify_one();
        disposal.await.unwrap();
        assert!(state.task.lock().await.is_none(), "N9/E2");
        assert!(
            registry
                .reserve(directory.path().join("replacement.sock"))
                .is_err(),
            "N9 disposal remains irreversible"
        );
    }

    #[tokio::test]
    async fn namespace_publication_serves_the_real_codec_and_disposes_irreversibly() {
        /* N6/N7 cause/effect table:
         * C1=Linux Namespace with exact typed demand; C2=one framed request;
         * C3=the first lease closes and a replacement publishes; C4=stale
         * first-lease drop; C5=Sandbox disposal. E1=one response through the
         * private external rendezvous; E2=one new socket generation; E3=stale
         * drop leaves it intact; E4=listener closes before tree removal and can
         * never reopen. Rules: N6 C1+C2=>E1; N7 C3=>E2; N7 C3+C4=>E3;
         * N7 C5=>E4.
         */
        let base = tempfile::tempdir().unwrap();
        let provider = NamespaceProvider::new(base.path());
        let sandbox = provider
            .create_sandbox(&demanded_spec("namespace-control"))
            .await
            .unwrap();
        let socket = sandbox
            .control_directory
            .as_ref()
            .unwrap()
            .join("repository-git-credential.sock");
        assert!(
            !socket.starts_with(sandbox.root.root()),
            "N6 private external path"
        );

        let first = SandboxControlServicePublisher::publish_sandbox_control_service(
            &sandbox,
            SandboxControlServiceKind::RepositoryGitCredential,
            Arc::new(DeniedService),
        )
        .await
        .unwrap();
        let mut channel = tokio::net::UnixStream::connect(&socket).await.unwrap();
        awaken_sandbox_control::write_frame(
            &mut channel,
            &awaken_sandbox_control::SandboxControlRequest::RepositoryGitCredentialGet {
                query: awaken_sandbox_control::RepositoryGitCredentialQuery {
                    protocol: "https".into(),
                    host: "gateway.test".into(),
                    path: "git/repository".into(),
                },
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(
                awaken_sandbox_control::read_frame::<
                    _,
                    awaken_sandbox_control::SandboxControlResponse,
                >(&mut channel)
                .await
                .unwrap(),
                awaken_sandbox_control::SandboxControlResponse::Denied
            ),
            "N6/E1"
        );
        first.close().await;
        assert!(!socket.exists(), "N7 old socket removed");

        let second = SandboxControlServicePublisher::publish_sandbox_control_service(
            &sandbox,
            SandboxControlServiceKind::RepositoryGitCredential,
            Arc::new(DeniedService),
        )
        .await
        .unwrap();
        drop(first);
        assert!(socket.exists(), "N7/E3");
        pc::Sandbox::dispose(&sandbox).await.unwrap();
        assert!(!socket.exists(), "N7/E4");
        assert!(
            SandboxControlServicePublisher::publish_sandbox_control_service(
                &sandbox,
                SandboxControlServiceKind::RepositoryGitCredential,
                Arc::new(DeniedService),
            )
            .await
            .is_err(),
            "N7/E4 no reopen"
        );
        drop(second);
    }
}
