//! The write-plane half of the cell's worker seam: a database-less worker pushes
//! its committed facts (a neutral [`ThreadCommit`]) to the cell server, which
//! applies them through the thread's single-writer [`Coordinator`]. The worker
//! never touches the store — the server stays the sole writer of committed truth.
//!
//! Paired with the dispatch transport (control plane): a worker claims a run over
//! the dispatch transport, drives it, then commits its facts here.

use std::sync::Arc;

use axum::extract::{Extension, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{Value, json};

use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_run_ingress::{
    ClaimedRunCommit, DispatchQueue, RunClaim, WorkerDirectory, WorkerIdentity, WorkerState,
};

use crate::host::{HostError, SharedHost};
use crate::worker_http::respond;
use crate::worker_security::{
    HeaderWorkerAuthenticator, VerifiedWorkerContext, WORKER_ID_HEADER, WorkerRequestAuthenticator,
};

#[async_trait::async_trait]
trait CommitApplier: Send + Sync {
    async fn apply(&self, commit: ThreadCommit) -> Result<CommitRecord, HostError>;
}

struct HostCommitApplier(Arc<SharedHost>);

#[async_trait::async_trait]
impl CommitApplier for HostCommitApplier {
    async fn apply(&self, commit: ThreadCommit) -> Result<CommitRecord, HostError> {
        apply_commit(&self.0, commit).await
    }
}

struct CommitIngestState {
    applier: Arc<dyn CommitApplier>,
    /// An injected store is used by isolated compositions and conformance tests.
    /// The legacy facade leaves it empty and resolves the process store lazily.
    dispatch: Option<Arc<dyn DispatchQueue>>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Option<Arc<dyn WorkerDirectory>>,
}

/// The worker-facing commit-ingest router. Mount it on a cell server alongside the
/// dispatch transport; a database-less worker's [`RemoteCoordinator`] posts here.
pub fn commit_ingest_router(host: Arc<SharedHost>) -> Router {
    commit_ingest_router_from_parts(
        Arc::new(HostCommitApplier(host)),
        None,
        Arc::new(HeaderWorkerAuthenticator),
        None,
        true,
    )
}

/// Production-facing commit router. It exposes only the atomic claimed-commit
/// operation and requires the caller-provided worker authenticator. The legacy
/// unclaimed `/v1/worker/commit` route remains confined to
/// [`commit_ingest_router`] for source compatibility.
pub fn claimed_commit_ingest_router(
    host: Arc<SharedHost>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
) -> Router {
    commit_ingest_router_from_parts(
        Arc::new(HostCommitApplier(host)),
        Some(dispatch),
        authenticator,
        None,
        false,
    )
}

/// Registered-worker variant that validates the current incarnation before
/// accepting the claim owner carried by an atomic commit.
pub fn claimed_commit_ingest_router_with_directory(
    host: Arc<SharedHost>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
) -> Router {
    commit_ingest_router_from_parts(
        Arc::new(HostCommitApplier(host)),
        Some(dispatch),
        authenticator,
        Some(directory),
        false,
    )
}

fn commit_ingest_router_from_parts(
    applier: Arc<dyn CommitApplier>,
    dispatch: Option<Arc<dyn DispatchQueue>>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Option<Arc<dyn WorkerDirectory>>,
    allow_unclaimed: bool,
) -> Router {
    let state = Arc::new(CommitIngestState {
        applier,
        dispatch,
        authenticator,
        directory,
    });
    let router = Router::new().route(
        "/v1/worker/commit-claimed",
        axum::routing::post(commit_claimed),
    );
    let router = if allow_unclaimed {
        router.route("/v1/worker/commit", axum::routing::post(commit_ingest))
    } else {
        router
    };
    router
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate_commit_worker,
        ))
        .with_state(state)
}

async fn authenticate_commit_worker(
    State(state): State<Arc<CommitIngestState>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    match state.authenticator.authenticate(&parts).await {
        Ok(worker) => {
            let mut request = Request::from_parts(parts, body);
            request.extensions_mut().insert(worker);
            next.run(request).await
        }
        Err(error) => unauthorized(error.to_string()).into_response(),
    }
}

async fn commit_ingest(
    State(state): State<Arc<CommitIngestState>>,
    Extension(_worker): Extension<VerifiedWorkerContext>,
    Json(commit): Json<ThreadCommit>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        let record = state.applier.apply(commit).await?;
        Ok(serde_json::to_value(record).expect("CommitRecord serializes"))
    }
    .await;
    respond(result)
}

#[derive(serde::Deserialize)]
struct ClaimedCommitRequest {
    claim: RunClaim,
    commit: ThreadCommit,
    #[serde(default)]
    identity: Option<WorkerIdentity>,
}

/// Atomically validate a remote worker's claim and apply its ThreadCommit while
/// the dispatch authority guard is live. Reclaim/settle/cancel cannot enter the
/// store between the validation and the commit.
async fn commit_claimed(
    State(state): State<Arc<CommitIngestState>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimedCommitRequest>,
) -> (StatusCode, Json<Value>) {
    if let Some(directory) = &state.directory {
        let Some(identity) = request.identity.as_ref() else {
            return unauthorized("registered worker identity is required".to_string());
        };
        if worker.worker_id() != identity.worker_id {
            return unauthorized("authenticated worker identity does not match".to_string());
        }
        let current = match directory.current(&identity.worker_id).await {
            Ok(current) => current,
            Err(error) => return unauthorized(error.to_string()),
        };
        if !current.as_ref().is_some_and(|record| {
            &record.snapshot.identity == identity
                && record.snapshot.state != WorkerState::Dead
                && record.snapshot.expires_at_ms > unix_now_ms()
        }) || request.claim.owner != identity.lease_owner()
        {
            return unauthorized("worker incarnation does not own the claim".to_string());
        }
    } else if worker.worker_id() != request.claim.owner {
        return unauthorized("authenticated worker does not own the claim".to_string());
    }
    let result = async {
        let store = match &state.dispatch {
            Some(store) => Arc::clone(store),
            None => crate::dispatch_backend::shared_durable_store(None)? as Arc<dyn DispatchQueue>,
        };
        let guard = store
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let Some(_guard) = guard else {
            return Err(HostError::bad_request("run claim is stale"));
        };
        let record = state.applier.apply(request.commit).await?;
        Ok(serde_json::to_value(record).expect("CommitRecord serializes"))
    }
    .await;
    respond(result)
}

fn unauthorized(message: String) -> (StatusCode, Json<Value>) {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message })))
}

async fn apply_commit(
    host: &Arc<SharedHost>,
    commit: ThreadCommit,
) -> Result<CommitRecord, HostError> {
    // Resolve the thread's single-writer coordinator and apply the worker's
    // staged commit through it — the server is the sole writer of committed truth.
    let thread = commit.thread_id.0.clone();
    let ctx = host.ctx_for(&thread, None).await?;
    // Idempotent redelivery (at-least-once → exactly-once effect): if this run's
    // fact is already terminal, an earlier delivery landed.
    if let Some(existing) = RunStore::get(&*ctx.commit, commit.run_id())
        && matches!(existing.state, RunState::Ended(_))
    {
        return Ok(CommitRecord { sequence: 0 });
    }
    ctx.commit
        .commit(commit)
        .await
        .map_err(|error| HostError::internal(error.to_string()))
}

/// A database-less worker's [`Coordinator`]: `commit` posts the staged
/// [`ThreadCommit`] to a cell server's [`commit_ingest_router`], which applies it
/// through the thread's single writer. The worker holds only an HTTP client.
pub struct RemoteCoordinator {
    base_url: String,
    client: reqwest::Client,
    worker_id: String,
}

/// Atomic claimed-run commit used by a database-less worker. Unlike
/// [`RemoteCoordinator`], this sends the claim and commit in one request.
pub struct RemoteClaimedRunCommit {
    base_url: String,
    client: reqwest::Client,
    identity: Option<WorkerIdentity>,
}

impl RemoteClaimedRunCommit {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            identity: None,
        }
    }

    /// Use a caller-configured client (for example one carrying a worker mTLS
    /// identity) instead of the default client.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    #[must_use]
    pub fn with_worker_identity(mut self, identity: WorkerIdentity) -> Self {
        self.identity = Some(identity);
        self
    }
}

#[async_trait::async_trait]
impl ClaimedRunCommit for RemoteClaimedRunCommit {
    async fn commit(
        &self,
        claim: &RunClaim,
        commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError> {
        let authenticated_worker = self
            .identity
            .as_ref()
            .map_or(claim.owner.as_str(), |identity| identity.worker_id.as_str());
        let response = self
            .client
            .post(format!("{}/v1/worker/commit-claimed", self.base_url))
            .header(WORKER_ID_HEADER, authenticated_worker)
            .json(&json!({ "claim": claim, "commit": commit, "identity": self.identity }))
            .send()
            .await
            .map_err(|error| CommitError::Rejected(format!("claimed commit transport: {error}")))?;
        if !response.status().is_success() {
            return Err(CommitError::Rejected(format!(
                "claimed commit server returned {}",
                response.status()
            )));
        }
        response
            .json()
            .await
            .map_err(|error| CommitError::Rejected(format!("commit record decode: {error}")))
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

impl RemoteCoordinator {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            worker_id: "awaken-worker".to_string(),
        }
    }

    #[must_use]
    pub fn with_worker_id(mut self, worker_id: impl Into<String>) -> Self {
        self.worker_id = worker_id.into();
        self
    }

    /// Use a caller-configured client (for example one carrying a worker mTLS
    /// identity) instead of the default client.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }
}

#[async_trait::async_trait]
impl Coordinator for RemoteCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        let resp = self
            .client
            .post(format!("{}/v1/worker/commit", self.base_url))
            .header(WORKER_ID_HEADER, &self.worker_id)
            .json(&commit)
            .send()
            .await
            .map_err(|e| CommitError::Rejected(format!("commit transport: {e}")))?;
        if !resp.status().is_success() {
            return Err(CommitError::Rejected(format!(
                "commit ingest server returned {}",
                resp.status()
            )));
        }
        resp.json()
            .await
            .map_err(|e| CommitError::Rejected(format!("commit record decode: {e}")))
    }
}

#[cfg(test)]
mod postgres_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::RunDisposition;
    use awaken_run_ingress::{
        ClaimedRunCommit, DispatchOutcome, PostgresDispatchStore, RunDispatch, SettleOutcome,
    };
    use awaken_runtime_contract::activation::RunActivation;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
    };
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use sqlx::Executor;

    use super::*;

    struct OkModel;

    #[async_trait::async_trait]
    impl LlmExecutor for OkModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    struct BlockingFirstCommit {
        inner: Arc<dyn CommitApplier>,
        first: AtomicBool,
        entered: tokio::sync::Barrier,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl CommitApplier for BlockingFirstCommit {
        async fn apply(&self, commit: ThreadCommit) -> Result<CommitRecord, HostError> {
            if self.first.swap(false, Ordering::SeqCst) {
                self.entered.wait().await;
                self.release.notified().await;
            }
            self.inner.apply(commit).await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn commit_claimed_postgres_guard_blocks_reclaim_until_http_commit_finishes() {
        let Ok(database_url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
            eprintln!("skip: AWAKEN_TEST_DATABASE_URL unset");
            return;
        };
        const SCHEMA: &str = "t_runtime_host_claimed_commit";
        let admin = sqlx::PgPool::connect(&database_url)
            .await
            .expect("connect postgres test admin");
        admin
            .execute(format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE").as_str())
            .await
            .expect("drop old claimed-commit schema");
        admin
            .execute(format!("CREATE SCHEMA {SCHEMA}").as_str())
            .await
            .expect("create claimed-commit schema");
        admin.close().await;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .after_connect(|connection, _metadata| {
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path = {SCHEMA}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .expect("connect isolated claimed-commit pool");
        let store = Arc::new(
            PostgresDispatchStore::with_pool(pool)
                .await
                .expect("migrate postgres dispatch store"),
        );

        let run = RunId("pg-http-claimed".to_string());
        store
            .enqueue(RunDispatch::new(activation(&run.0, "pg-http-thread")))
            .await
            .expect("enqueue claimed-commit run");
        let first = store
            .claim_run(&run, "worker-a", 100, 0)
            .await
            .expect("first claim")
            .expect("run is claimable");

        let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
        let blocking = Arc::new(BlockingFirstCommit {
            inner: Arc::new(HostCommitApplier(host.clone())),
            first: AtomicBool::new(true),
            entered: tokio::sync::Barrier::new(2),
            release: tokio::sync::Notify::new(),
        });
        let router = commit_ingest_router_from_parts(
            blocking.clone(),
            Some(store.clone() as Arc<dyn DispatchQueue>),
            Arc::new(HeaderWorkerAuthenticator),
            None,
            false,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind claimed-commit server");
        let address = listener.local_addr().expect("claimed-commit address");
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve claimed-commit router");
        });
        let remote = Arc::new(RemoteClaimedRunCommit::new(format!("http://{address}")));
        let first_claim = RunClaim::from(&first.lease);
        let committing = {
            let remote = remote.clone();
            let claim = first_claim.clone();
            tokio::spawn(async move {
                remote
                    .commit(
                        &claim,
                        ended_commit("pg-http-claimed", "pg-http-thread", "worker-a"),
                    )
                    .await
            })
        };
        blocking.entered.wait().await;

        let early = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            store.claim_run(&run, "worker-b", 100, 200),
        )
        .await
        .expect("reclaim does not hang behind a SKIP LOCKED row")
        .expect("reclaim query succeeds");
        assert!(
            early.is_none(),
            "the row locked by /commit-claimed cannot be re-owned"
        );

        blocking.release.notify_one();
        committing
            .await
            .expect("HTTP commit task joins")
            .expect("current HTTP claimed commit succeeds");

        // Dropping sqlx::Transaction queues ROLLBACK on its connection; it does
        // not wait for PostgreSQL to acknowledge the rollback. The HTTP response
        // can therefore win a very small race with the row-lock release. Poll as
        // a real worker would, but keep the test strictly bounded.
        let recovered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(claimed) = store
                    .claim_run(&run, "worker-b", 100, 200)
                    .await
                    .expect("reclaim after commit")
                {
                    break claimed;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("guard release makes the expired run reclaimable within five seconds");
        assert_eq!(recovered.lease.epoch, first.lease.epoch + 1);
        assert!(
            remote
                .commit(
                    &first_claim,
                    ended_commit("pg-http-claimed", "pg-http-thread", "stale"),
                )
                .await
                .is_err(),
            "the old owner is fenced at the HTTP commit boundary"
        );
        assert_eq!(
            store
                .settle(&run, first.lease.epoch, DispatchOutcome::Done, &[])
                .await
                .expect("stale settle verdict"),
            SettleOutcome::Fenced
        );
        remote
            .commit(
                &RunClaim::from(&recovered.lease),
                ended_commit("pg-http-claimed", "pg-http-thread", "worker-b"),
            )
            .await
            .expect("current recovery owner can commit");
        assert_eq!(
            store
                .settle(&run, recovered.lease.epoch, DispatchOutcome::Done, &[],)
                .await
                .expect("current settle verdict"),
            SettleOutcome::Applied
        );
        let messages = host.committed_messages("pg-http-thread").await;
        assert_eq!(messages.len(), 1, "redelivery has one committed effect");
        assert_eq!(messages[0].text_content(), "worker-a");
    }

    fn activation(run: &str, thread: &str) -> RunActivation {
        let fingerprint = CatalogFingerprint("pg-http-fingerprint".to_string());
        RunActivation::new(
            RunId(run.to_string()),
            ThreadId(thread.to_string()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("pg-http-snapshot".to_string()),
                metadata: Default::default(),
                root_agent_id: AgentId("pg-http-agent".to_string()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: fingerprint.clone(),
                    instructions: "test".to_string(),
                    max_steps: 2,
                    delegation_limits: Default::default(),
                    model_binding: ModelBinding::new("provider", "model", "backend"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint,
            },
            Vec::new(),
        )
    }

    fn ended_commit(run: &str, thread: &str, text: &str) -> ThreadCommit {
        ThreadCommit {
            thread_id: ThreadId(thread.to_string()),
            run: RunDisposition::ended(RunId(run.to_string()), EndCause::NaturalEnd),
            messages: vec![Message::text(
                MessageId(format!("message-{text}")),
                Role::Assistant,
                text,
            )],
            state: Vec::new(),
            events: Vec::new(),
        }
    }
}
