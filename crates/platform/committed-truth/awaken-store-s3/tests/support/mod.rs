#![allow(dead_code)] // Shared fixture builders are consumed by different integration binaries.
use awaken_agent_contract::{
    agent::{
        message::{Id as MessageId, Message, Role},
        run::Id as RunId,
        thread::Id as ThreadId,
    },
    thread::commit::{
        operation::{CommitOperation, CommitOperationId, commit_payload_hash},
        staged::{RunDisposition, ThreadCommit},
    },
};
use awaken_dispatch_contract::RunDispatch;
use awaken_lease_testkit::ManualClock;
use awaken_store_s3::{Limits, S3Storage, SessionKey, SessionStorage};
use futures::stream::BoxStream;
use object_store::{path::Path, *};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

pub(crate) fn dispatch(run: &str, thread: &str, session: &str) -> RunDispatch {
    let mut activation = awaken_dispatch_testkit::worker_http::activation("s3");
    activation.run_id = RunId(run.into());
    activation.thread_id = ThreadId(thread.into());
    let mut request = RunDispatch::new(activation).with_worker_codec_requirements();
    if thread != session {
        request.session_thread_id = Some(ThreadId(session.into()));
    }
    request
}
pub(crate) fn operation(
    run: &str,
    thread: &str,
    ordinal: u64,
    version: u64,
    text: &str,
) -> CommitOperation {
    let commit = ThreadCommit::assemble(
        ThreadId(thread.into()),
        RunDisposition::running(RunId(run.into())),
        ordinal == 0,
        vec![Message::text(
            MessageId(format!("{run}-{ordinal}")),
            Role::Assistant,
            text,
        )],
        Vec::new(),
        Vec::new(),
    );
    CommitOperation {
        operation_id: CommitOperationId::new(RunId(run.into()), ordinal),
        expected_thread_version: version,
        payload_hash: commit_payload_hash(&commit).unwrap(),
        commit,
    }
}
/// Use the same storage laws against an explicit S3 endpoint or isolated memory.
/// Every fixture gets a separate physical prefix; reopening retains its transport.
pub(crate) fn object_transport() -> Arc<dyn ObjectStore> {
    let endpoint = match std::env::var("AWAKEN_TEST_S3_ENDPOINT") {
        Ok(endpoint) => endpoint,
        Err(std::env::VarError::NotPresent) => {
            return Arc::new(object_store::memory::InMemory::new());
        }
        Err(error) => panic!("invalid S3 fixture endpoint: {error}"),
    };
    let bucket = std::env::var("AWAKEN_TEST_S3_BUCKET")
        .expect("an explicit S3 fixture endpoint requires its task-owned bucket");
    let transport = awaken_object_storage::s3_transport(
        &bucket,
        Some("us-east-1"),
        Some(&endpoint),
        endpoint.starts_with("http://"),
    )
    .expect("configured S3 fixture transport must initialize");
    Arc::new(object_store::prefix::PrefixStore::new(
        transport,
        Path::from(format!("awaken-fixture-{}", uuid::Uuid::new_v4())),
    ))
}
pub(crate) fn engine(store: Arc<dyn ObjectStore>, clock: Arc<ManualClock>) -> S3Storage {
    S3Storage::new(
        store,
        "test",
        clock,
        Limits {
            recent_commits: 2,
            ..Default::default()
        },
    )
    .unwrap()
}
pub(crate) async fn setup() -> (Arc<FaultStore>, Arc<ManualClock>, S3Storage, SessionStorage) {
    let store = Arc::new(FaultStore::default());
    let clock = Arc::new(ManualClock::new(1_000));
    let engine = engine(store.clone(), clock.clone());
    let session = engine.session(SessionKey::new("tenant", "session").unwrap());
    session.create().await.unwrap();
    (store, clock, engine, session)
}
pub(crate) async fn activate(
    session: &SessionStorage,
    run: &str,
    thread: &str,
) -> awaken_store_s3::Claim {
    session
        .accept(dispatch(run, thread, session.key().session_id()))
        .await
        .unwrap();
    session.claim(run, "worker-one", 1_000).await.unwrap()
}
#[derive(Debug)]
pub(crate) struct FaultStore {
    pub inner: Arc<dyn ObjectStore>,
    /// 1: head succeeds but response is lost; 2: head fails before applying; 3: reads fail.
    pub fault: AtomicU64,
    /// Confirmed, unapplied head precondition failures, unlike ambiguous transport loss.
    pub cas_conflicts: AtomicU64,
    pub hold_next_head: AtomicBool,
    pub head_entered: tokio::sync::Notify,
    pub head_resume: tokio::sync::Notify,
    pub hold_next_delete: AtomicBool,
    pub delete_entered: Arc<tokio::sync::Notify>,
    pub delete_resume: Arc<tokio::sync::Notify>,
    pub ranges: awaken_sync::LeafLock<Vec<(String, GetRange)>>,
}
impl Default for FaultStore {
    fn default() -> Self {
        Self {
            inner: object_transport(),
            fault: AtomicU64::new(0),
            cas_conflicts: AtomicU64::new(0),
            hold_next_head: AtomicBool::new(false),
            head_entered: tokio::sync::Notify::new(),
            head_resume: tokio::sync::Notify::new(),
            hold_next_delete: AtomicBool::new(false),
            delete_entered: Default::default(),
            delete_resume: Default::default(),
            ranges: Default::default(),
        }
    }
}
impl std::fmt::Display for FaultStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FaultStore")
    }
}
fn fault_error() -> object_store::Error {
    object_store::Error::Generic {
        store: "fault",
        source: std::io::Error::other("injected transport loss").into(),
    }
}
#[async_trait::async_trait]
impl ObjectStore for FaultStore {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if path.as_ref().ends_with("/head.json")
            && self.hold_next_head.swap(false, Ordering::SeqCst)
        {
            self.head_entered.notify_one();
            self.head_resume.notified().await;
        }
        if path.as_ref().ends_with("/head.json")
            && self
                .cas_conflicts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
        {
            return Err(object_store::Error::Precondition {
                path: path.to_string(),
                source: std::io::Error::other("injected CAS conflict").into(),
            });
        }
        let fault = if path.as_ref().ends_with("/head.json") {
            self.fault.swap(0, Ordering::SeqCst)
        } else {
            0
        };
        if fault == 2 {
            return Err(fault_error());
        }
        let result = self.inner.put_opts(path, payload, options).await?;
        if fault == 1 {
            Err(fault_error())
        } else {
            Ok(result)
        }
    }
    async fn get_opts(&self, path: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        if self.fault.load(Ordering::SeqCst) == 3 {
            return Err(fault_error());
        }
        if let Some(range) = options.range.clone() {
            self.ranges.lock().push((path.to_string(), range));
        }
        self.inner.get_opts(path, options).await
    }
    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }
    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        if !self.hold_next_delete.swap(false, Ordering::SeqCst) {
            return self.inner.delete_stream(locations);
        }
        use futures::StreamExt;
        let inner = self.inner.clone();
        let entered = self.delete_entered.clone();
        let resume = self.delete_resume.clone();
        futures::stream::once(async move {
            entered.notify_one();
            resume.notified().await;
            inner.delete_stream(locations)
        })
        .flatten()
        .boxed()
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}
