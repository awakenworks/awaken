//! `A2aRunExecutor`: a remote A2A agent (Coze / any A2A HTTP endpoint) driven as a
//! peer [`RunAttemptExecutor`]. Like the ACP executor it is *a second implementation*
//! of the one attempt port — no local process, no model loop of our own: it dials the
//! endpoint on `Backend::Remote(endpoint)`, sends the run's prompt as an A2A
//! `message:send`, and commits the returned task's reply through the same commit
//! boundary as the native and ACP paths.
//!
//! Boundaries: runtime plane; depends only on the foundation contracts + the A2A
//! protocol crate. The dial endpoint comes from the resolved backend, so this
//! executor stays config-free; a [`TransportResolver`] is the one injected seam.
//! The host resolves publication-pinned transport authentication behind that
//! port; tests may substitute an anonymous or scripted transport.
//! Each outbound message first commits a versioned Prepared operation to the
//! existing Run StateCell; only the exact returned task advances it to Accepted.

use std::sync::Arc;

use async_trait::async_trait;
#[cfg(test)]
use awaken_agent_contract::agent::awaiting::ResumeTicket;
#[cfg(test)]
use awaken_agent_contract::agent::message::Message;
#[cfg(test)]
use awaken_agent_contract::agent::message::{Id as MessageId, Role};
#[cfg(test)]
use awaken_agent_contract::agent::run::Record as RunRecord;
use awaken_agent_contract::agent::run::{EndCause, Failure, RunState};
#[cfg(test)]
use awaken_agent_contract::agent::state::Command as StateCommand;
#[cfg(test)]
use awaken_agent_contract::agent::state::{MergePolicy, Scope};
use awaken_agent_contract::thread::commit::RunDisposition;
#[cfg(test)]
use awaken_protocol_a2a::Task;
#[cfg(test)]
use awaken_protocol_a2a::TaskState;
use awaken_protocol_a2a::client::{get_task, send_message};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{
    AttemptRecoveryMode, Cancellation, Error, ExecutorCapabilities, Result, RunAttemptExecutor,
    RunExecutor, Wait, verify_attempt_ownership,
};
use awaken_runtime_contract::permission::ToolCapabilityNarrowing;
use awaken_runtime_contract::resolved::ResolvedModelCandidate;
#[cfg(test)]
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::resume::{ResumeCommand, validate_resume};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
#[cfg(test)]
use awaken_runtime_contract::terminal::CommittedTerminalRun;

mod message_identity;
mod run_commit;
mod task_driver;
mod task_projection;
mod task_state;

pub use awaken_protocol_a2a::{HttpTransport, Transport};
use message_identity::*;
use run_commit::*;
use task_projection::*;
use task_state::*;

/// Resolves one publication-pinned remote candidate into a live transport.
///
/// Implementations may materialize only the exact claim binding carried by
/// `context`; selecting credentials, endpoints or fallback candidates is outside
/// this port. The executor never sees plaintext material.
#[async_trait]
pub trait TransportResolver: Send + Sync {
    async fn resolve(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &RuntimeRunContext,
    ) -> std::result::Result<Arc<dyn Transport>, String>;
}

/// Drives a remote A2A agent as a [`RunAttemptExecutor`].
pub struct A2aRunExecutor {
    transport_resolver: Arc<dyn TransportResolver>,
}

impl A2aRunExecutor {
    #[must_use]
    pub fn new(transport_resolver: Arc<dyn TransportResolver>) -> Self {
        Self { transport_resolver }
    }

    /// Anonymous HTTP is a test compatibility fixture. Production composition
    /// must inject a resolver that verifies the publication's Agent Card
    /// security fingerprint and claim-frozen credential authority.
    #[cfg(test)]
    #[must_use]
    pub fn over_http() -> Self {
        Self::new(Arc::new(AnonymousHttpTransportResolver))
    }
}

#[cfg(test)]
struct AnonymousHttpTransportResolver;

#[cfg(test)]
#[async_trait]
impl TransportResolver for AnonymousHttpTransportResolver {
    async fn resolve(
        &self,
        candidate: &ResolvedModelCandidate,
        _context: &RuntimeRunContext,
    ) -> std::result::Result<Arc<dyn Transport>, String> {
        let endpoint = candidate
            .binding()
            .backend_ref
            .strip_prefix("a2a:")
            .ok_or_else(|| "anonymous A2A fixture received a non-remote candidate".to_string())?;
        Ok(Arc::new(HttpTransport::new(endpoint)))
    }
}

mod executor;
#[cfg(test)]
use executor::ensure_supported_narrowing;
#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
    use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
    use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
    use awaken_protocol_a2a::Artifact;
    use awaken_protocol_a2a::client::Response;
    use awaken_protocol_a2a::types::{
        Message as A2aMessage, MessageKind, MessageRole, Part as A2aPart, TaskKind, TaskStatus,
    };
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::runtime_context::{
        AttemptOwnershipError, AttemptOwnershipVerifier,
    };
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use awaken_runtime_contract::terminal::{RunTerminalObserver, RunTerminalObserverError};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(super) struct ScriptedTransport {
        replies: Mutex<std::collections::VecDeque<std::result::Result<Response, String>>>,
        pub(super) seen: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl ScriptedTransport {
        pub(super) fn new(replies: Vec<std::result::Result<Response, String>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Transport for ScriptedTransport {
        async fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<Vec<u8>>,
        ) -> std::result::Result<Response, String> {
            self.seen.lock().unwrap().push((
                method.to_string(),
                path.to_string(),
                body.map(|body| String::from_utf8_lossy(&body).into_owned()),
            ));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("script has a reply")
        }
    }

    pub(super) fn response(body: &str) -> std::result::Result<Response, String> {
        Ok(Response::new(200, body.as_bytes().to_vec()))
    }

    struct FixedTransportResolver {
        transport: Arc<dyn Transport>,
        resolutions: AtomicUsize,
    }

    impl FixedTransportResolver {
        fn new(transport: Arc<dyn Transport>) -> Self {
            Self {
                transport,
                resolutions: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl TransportResolver for FixedTransportResolver {
        async fn resolve(
            &self,
            _candidate: &ResolvedModelCandidate,
            _context: &RuntimeRunContext,
        ) -> std::result::Result<Arc<dyn Transport>, String> {
            self.resolutions.fetch_add(1, Ordering::SeqCst);
            Ok(self.transport.clone())
        }
    }

    pub(super) fn scripted_executor(transport: Arc<ScriptedTransport>) -> A2aRunExecutor {
        A2aRunExecutor::new(Arc::new(FixedTransportResolver::new(transport)))
    }

    #[derive(Clone, Copy)]
    pub(super) enum OwnershipDecision {
        Current,
        Lost,
        Unavailable,
    }

    pub(super) struct ScriptedOwnership {
        decisions: Mutex<VecDeque<OwnershipDecision>>,
    }

    impl ScriptedOwnership {
        pub(super) fn new(decisions: impl IntoIterator<Item = OwnershipDecision>) -> Self {
            Self {
                decisions: Mutex::new(decisions.into_iter().collect()),
            }
        }
    }

    #[async_trait]
    impl AttemptOwnershipVerifier for ScriptedOwnership {
        async fn verify_current(&self) -> std::result::Result<(), AttemptOwnershipError> {
            match self
                .decisions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(OwnershipDecision::Current)
            {
                OwnershipDecision::Current => Ok(()),
                OwnershipDecision::Lost => Err(AttemptOwnershipError::Lost),
                OwnershipDecision::Unavailable => {
                    Err(AttemptOwnershipError::Unavailable("authority down".into()))
                }
            }
        }
    }

    #[derive(Default)]
    pub(super) struct Rec(pub(super) Mutex<Vec<ThreadCommit>>);
    #[async_trait]
    impl Coordinator for Rec {
        async fn commit(&self, c: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
            let mut commits = self.0.lock().unwrap();
            commits.push(c);
            Ok(CommitRecord {
                sequence: commits.len() as u64,
            })
        }
    }

    impl CommittedThreadView for Rec {
        fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|commit| &commit.thread_id == thread_id)
                .flat_map(|commit| commit.messages.clone())
                .collect()
        }

        fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|commit| commit.run.run_id() == run_id)
                .and_then(|commit| commit.run.resume_ticket().cloned())
        }

        fn open_wait_for_thread(&self, thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
            let commits = self.0.lock().ok()?;
            let latest = commits
                .iter()
                .rev()
                .find(|commit| &commit.thread_id == thread_id)?;
            let ticket = latest.run.resume_ticket()?.clone();
            (ticket.thread_id == *thread_id).then(|| (latest.run.run_id().clone(), ticket))
        }

        fn run(&self, run_id: &RunId) -> Option<RunRecord> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|commit| commit.run.run_id() == run_id)
                .map(|commit| RunRecord {
                    id: run_id.clone(),
                    thread_id: commit.thread_id.clone(),
                    state: commit.run.state(),
                })
        }

        fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|commit| &commit.thread_id == thread_id)
                .map(|commit| RunRecord {
                    id: commit.run.run_id().clone(),
                    thread_id: thread_id.clone(),
                    state: commit.run.state(),
                })
        }

        fn run_state(&self, run_id: &RunId) -> Option<RunState> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|commit| commit.run.run_id() == run_id)
                .map(|commit| commit.run.state())
        }

        fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|commit| &commit.thread_id == thread_id)
                .flat_map(|commit| commit.state.clone())
                .collect()
        }
    }

    #[derive(Default)]
    struct TerminalRec(Mutex<Vec<CommittedTerminalRun>>);

    #[async_trait]
    impl RunTerminalObserver for TerminalRec {
        fn observer_id(&self) -> &str {
            "a2a-terminal-test"
        }

        async fn observe(
            &self,
            terminal: &CommittedTerminalRun,
        ) -> std::result::Result<(), RunTerminalObserverError> {
            self.0.lock().unwrap().push(terminal.clone());
            Ok(())
        }
    }

    pub(super) fn activation(backend_ref: &str) -> RunActivation {
        RunActivation {
            run_id: RunId("r".into()),
            thread_id: ThreadId("t".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("p", "m", backend_ref),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: vec![Message::text(MessageId("u".into()), Role::User, "go")],
            delegation_origin: None,
            model_ref_override: None,
            data_subject_id: None,
            tool_capability_narrowing: Default::default(),
        }
    }

    #[test]
    fn remote_execution_fails_closed_when_tool_denial_cannot_be_proven() {
        let restricted = activation("a2a:https://agent.example").without_tools();
        let error = ensure_supported_narrowing(&restricted, &RuntimeRunContext::new())
            .expect_err("an opaque remote Agent cannot enforce local tool denial");
        assert!(error.to_string().contains("cannot prove enforcement"));

        let context = RuntimeRunContext::new().with_tool_permission_policy(Arc::new(
            awaken_runtime_contract::permission::DenyAllTools::new("attempt restriction"),
        ));
        assert!(
            ensure_supported_narrowing(&activation("a2a:https://agent.example"), &context).is_err(),
            "process-local narrowing must not be silently ignored either"
        );
    }

    pub(super) struct RejectingTransportResolver;

    #[async_trait]
    impl TransportResolver for RejectingTransportResolver {
        async fn resolve(
            &self,
            _candidate: &ResolvedModelCandidate,
            _context: &RuntimeRunContext,
        ) -> std::result::Result<Arc<dyn Transport>, String> {
            Err("claim-frozen remote credential is unavailable".into())
        }
    }

    #[tokio::test]
    async fn transport_resolution_failure_never_falls_back_to_anonymous_http() {
        // Cause/effect: C1 snapshot selects Remote; C2 the injected host resolver
        // rejects its claim-frozen transport authority. Effect E1 return the
        // resolver error before any A2A request; forbidden effect E2 is building
        // an anonymous HttpTransport inside the executor.
        //
        // Decision table: C1+!C2 -> E1 and zero wire side effects. The successful
        // C1+C2 path is covered by every scripted transport execution test.
        let error = A2aRunExecutor::new(Arc::new(RejectingTransportResolver))
            .execute(
                activation("a2a:https://agent.example"),
                RuntimeRunContext::new(),
            )
            .await
            .expect_err("transport authority is mandatory");
        assert!(
            error
                .to_string()
                .contains("claim-frozen remote credential is unavailable")
        );
    }

    #[tokio::test]
    async fn a2a_delivers_the_same_post_commit_terminal_extension_contract() {
        let activation = activation("a2a:https://example.test");
        let observer = Arc::new(TerminalRec::default());
        let context = RuntimeRunContext::new()
            .with_commit(Arc::new(Rec::default()))
            .with_terminal_observer(observer.clone());

        let state = finish_terminal(&context, &activation, Vec::new(), EndCause::NaturalEnd)
            .await
            .unwrap();

        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(
            observer.0.lock().unwrap().as_slice(),
            &[CommittedTerminalRun {
                run_id: RunId("r".into()),
                thread_id: ThreadId("t".into()),
                cause: EndCause::NaturalEnd,
            }]
        );
    }

    // ---- pure translation units (task_reply / end_cause_of) ----

    /// A text agent message for the A2A wire shape (helper for task fixtures).
    fn a2a_msg(text: &str) -> A2aMessage {
        A2aMessage {
            kind: MessageKind::Message,
            task_id: None,
            context_id: None,
            message_id: "m".into(),
            role: MessageRole::Agent,
            parts: vec![A2aPart::text(text)],
            extensions: Vec::new(),
            metadata: None,
            reference_task_ids: Vec::new(),
        }
    }

    /// A returned task with the given status message, history, and artifacts (each
    /// artifact is its list of text parts). Every fixture completes; task_reply
    /// selection is what these exercise.
    fn task_with(status_msg: Option<&str>, history: &[&str], artifacts: &[&[&str]]) -> Task {
        Task {
            kind: TaskKind::Task,
            id: "t".into(),
            context_id: "c".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: status_msg.map(a2a_msg),
                timestamp: None,
            },
            history: history.iter().map(|t| a2a_msg(t)).collect(),
            artifacts: artifacts
                .iter()
                .map(|parts| Artifact {
                    artifact_id: "artifact".into(),
                    name: None,
                    description: None,
                    extensions: Vec::new(),
                    metadata: None,
                    parts: parts.iter().map(|t| A2aPart::text(*t)).collect(),
                })
                .collect(),
            metadata: None,
        }
    }

    #[test]
    fn task_reply_joins_multiple_artifacts_with_newlines() {
        // Two durable artifacts: their texts are the reply, joined by "\n".
        assert_eq!(
            task_reply(&task_with(None, &[], &[&["a1"], &["a2"]])),
            "a1\na2"
        );
    }

    #[test]
    fn task_reply_skips_text_empty_artifacts_and_falls_to_status() {
        // An artifact with no text parts is not a reply → fall to the status message.
        assert_eq!(
            task_reply(&task_with(Some("status body"), &["h"], &[&[]])),
            "status body"
        );
    }

    #[test]
    fn task_reply_prefers_status_message_over_history() {
        // No artifacts, but a non-empty status message wins over the last history msg.
        assert_eq!(
            task_reply(&task_with(Some("status"), &["h0", "h1"], &[])),
            "status"
        );
    }

    #[test]
    fn task_reply_empty_status_text_falls_through_to_last_history() {
        // A status message present but text-empty is not a reply → last history msg.
        assert_eq!(
            task_reply(&task_with(Some(""), &["h0", "last"], &[])),
            "last"
        );
    }

    #[test]
    fn task_reply_of_an_empty_task_is_the_empty_string() {
        // No artifacts, no status message, no history → empty reply (no panic).
        assert_eq!(task_reply(&task_with(None, &[], &[])), "");
    }

    #[test]
    fn end_cause_maps_each_task_state_honestly() {
        // Only a completed remote task is a natural end; the rest must not be
        // projected as success (G26).
        assert_eq!(end_cause_of(&TaskState::Completed), EndCause::NaturalEnd);
        assert_eq!(end_cause_of(&TaskState::Canceled), EndCause::Cancelled);
        assert_eq!(end_cause_of(&TaskState::Working), EndCause::Indeterminate);
        assert_eq!(
            end_cause_of(&TaskState::InputRequired),
            EndCause::Indeterminate
        );
        assert_eq!(
            end_cause_of(&TaskState::AuthRequired),
            EndCause::Indeterminate
        );
        assert!(matches!(
            end_cause_of(&TaskState::Failed),
            EndCause::Error(Failure::Inference { ref code, .. }) if code == "a2a_task_failed"
        ));
    }

    /// Drives the real executor against a **real A2A HTTP server** over a real TCP
    /// socket (no mock transport): `over_http()` dials the endpoint on
    /// `Backend::Remote`, the server answers a real A2A task, and the reply is
    /// committed. The server's fixed reply stands for the external remote agent —
    /// the transport, socket, and parse path are all real.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dials_a_real_http_a2a_server_and_commits_the_reply() {
        const REPLY: &str = r#"{"task":{"kind":"task","id":"t-1","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"a","role":"agent","parts":[{"kind":"text","text":"real remote reply"}]}}}}"#;

        // A real HTTP server on an ephemeral port; a fallback answers the A2A
        // `message:send` POST (the `:` in the path is a matchit param char, so a
        // fallback is simpler than a literal route and just as real).
        let app = axum::Router::new().fallback(|| async { REPLY });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // The production executor dials it over the real `HttpTransport` (ureq).
        let exec = A2aRunExecutor::over_http();
        let rec = Arc::new(Rec::default());
        let state = exec
            .execute(
                activation(&format!("a2a:http://{addr}")),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();

        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        let commits = rec.0.lock().unwrap();
        assert_eq!(
            commits.last().unwrap().messages[0].text_content(),
            "real remote reply"
        );
    }

    #[test]
    fn advertises_remote_abort_and_durable_wait() {
        let caps = A2aRunExecutor::over_http().capabilities();
        assert_eq!(caps.cancellation, Cancellation::RemoteAbort);
        assert_eq!(caps.wait, Wait::Both);
    }

    #[derive(Default)]
    struct FailingRec;
    #[async_trait]
    impl Coordinator for FailingRec {
        async fn commit(&self, _c: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
            Err(CommitError::Rejected("boom".into()))
        }
    }

    /// A real A2A HTTP server answering every `message:send` with `reply`; returns
    /// the `a2a:` backend ref that dials it.
    async fn serve(reply: &'static str) -> String {
        let app = axum::Router::new().fallback(move || async move { reply });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("a2a:http://{addr}")
    }

    #[tokio::test]
    async fn a_non_remote_backend_fails_closed() {
        // Reached without a remote endpoint is a wiring fault → fail closed, no dial.
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation("native"),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            state,
            RunState::Ended(EndCause::Error(Failure::Inference { ref code, .. })) if code == "a2a_config"
        ));
        assert_eq!(
            rec.0
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .messages
                .last()
                .unwrap()
                .text_content(),
            "backend is not an A2A endpoint"
        );
    }

    #[tokio::test]
    async fn a_commit_error_propagates() {
        let err = A2aRunExecutor::over_http()
            .execute(
                activation("native"),
                RuntimeRunContext::new().with_commit(Arc::new(FailingRec)),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Commit(_)),
            "a rejected commit surfaces as Error::Commit"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn artifacts_are_preferred_over_status_and_history() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"status msg"}]}},"artifacts":[{"artifactId":"a1","parts":[{"kind":"text","text":"artifact body"}]}]}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(
            rec.0.lock().unwrap().last().unwrap().messages[0].text_content(),
            "artifact body"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_last_is_the_final_reply_fallback() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"completed"},"history":[{"kind":"message","messageId":"h0","role":"agent","parts":[{"kind":"text","text":"first"}]},{"kind":"message","messageId":"h1","role":"agent","parts":[{"kind":"text","text":"last history"}]}]}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(
            rec.0.lock().unwrap().last().unwrap().messages[0].text_content(),
            "last history"
        );
    }

    #[tokio::test]
    async fn a_transport_error_vetoes_physical_ack_without_inventing_a_terminal_fact() {
        // Causes: C1 Prepared commits; C2 send is entered; C3 response is lost.
        // Effects: E1 retain Prepared as the only durable owner; E2 veto ACK;
        // E3 issue exactly one request and commit no Accepted/terminal fiction.
        //
        // | Rule | Prepared | Send | Response | Effects |
        // |---|---|---|---|---|
        // | R1 | committed | entered | lost | E1+E2+E3 |
        // | R2 | rejected | none | n/a | zero IO (sibling) |
        // The String transport seam cannot prove even "connection refused" was
        // pre-dispatch; HTTP/decode siblings cover the other error partitions.
        let transport = Arc::new(ScriptedTransport::new(vec![Err("response lost".into())]));
        let rec = Arc::new(Rec::default());
        let context = RuntimeRunContext::new().with_commit(rec.clone());
        let latch = context.physical_quiescence.clone();
        let error = scripted_executor(transport.clone())
            .execute(activation("a2a:http://remote.invalid"), context)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("response lost"));
        assert!(!latch.can_acknowledge_return());
        assert_eq!(
            rec.0.lock().unwrap().len(),
            1,
            "the pre-send Prepared boundary remains durable"
        );
        assert_eq!(transport.seen.lock().unwrap().len(), 1);
    }

    /// A real A2A server that records the inbound request (uri + body) and answers a
    /// completed task; returns the backend ref and the shared capture slot.
    async fn serve_capturing() -> (String, Arc<Mutex<Option<(String, String)>>>) {
        const REPLY: &str = r#"{"task":{"kind":"task","id":"t-1","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"a","role":"agent","parts":[{"kind":"text","text":"ok"}]}}}}"#;
        let cap: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let slot = cap.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let slot = slot.clone();
            async move {
                let uri = req.uri().to_string();
                let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                *slot.lock().unwrap() = Some((uri, String::from_utf8_lossy(&bytes).into_owned()));
                REPLY
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("a2a:http://{addr}"), cap)
    }

    /// The outbound `message:send` carries the A2A correlation fields the remote
    /// agent keys on — a wrong contextId/messageId silently breaks real agents while
    /// a body-ignoring server test still passes, so assert the real serialized wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_outbound_message_send_carries_the_correlation_fields() {
        let (backend, cap) = serve_capturing().await;
        let rec = Arc::new(Rec::default());
        A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();

        let (uri, body) = cap.lock().unwrap().clone().expect("a request was captured");
        assert!(uri.ends_with("/v1/a2a/message:send"), "path: {uri}");
        assert!(
            body.contains(r#""contextId":"t""#),
            "contextId = thread id: {body}"
        );
        let wire: serde_json::Value = serde_json::from_str(&body).expect("captured JSON request");
        assert_eq!(
            wire["message"]["messageId"],
            stable_message_id(&RunId("r".into()), None).unwrap(),
            "messageId is the canonical Run identity: {body}"
        );
        assert!(
            body.contains(r#""kind":"message""#),
            "message union: {body}"
        );
        assert!(body.contains(r#""role":"user""#), "user role: {body}");
        assert!(body.contains(r#""text":"go""#), "the prompt text: {body}");
        assert!(
            body.contains(r#""agentId":null"#),
            "agentId is null: {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a2a_projects_normalized_context_but_commits_only_durable_input() {
        // Cause/effect graph: C1 frozen instructions, C2 request-only Session
        // context, and C3 durable User input. Effects: E1 the wire prompt orders
        // C1 then C2 then C3; E2 Thread commits contain C3 and the remote reply
        // but never C2. Rule N1 C1+C2+C3 -> E1+E2. This is the A2A row of the
        // Native/ACP/A2A normalized-input equivalence table.
        let (backend, cap) = serve_capturing().await;
        let mut activation = activation(&backend);
        activation.snapshot.resolved_spec.instructions = "follow frozen policy".into();
        let request_context = Message::text(
            MessageId("session-context:resource-r2".into()),
            Role::System,
            "use resource revision two",
        );
        let rec = Arc::new(Rec::default());
        let mut context = RuntimeRunContext::new().with_commit(rec.clone());
        context.request_context.push(Message::text(
            MessageId("session-baseline:historical".into()),
            Role::System,
            "obsolete resource revision",
        ));
        context.request_context.push(request_context);

        A2aRunExecutor::over_http()
            .execute(activation, context)
            .await
            .expect("normalized A2A execution");

        let body = cap
            .lock()
            .unwrap()
            .as_ref()
            .expect("wire request")
            .1
            .clone();
        let policy = body.find("follow frozen policy").expect("C1/E1");
        let resource = body.find("use resource revision two").expect("C2/E1");
        let input = body.find("go").expect("C3/E1");
        assert!(policy < resource && resource < input, "N1/E1: {body}");
        assert!(
            !body.contains("obsolete resource revision"),
            "N1/E1 legacy derived context reached A2A: {body}"
        );
        let committed = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .flat_map(|commit| commit.messages.iter())
            .map(Message::text_content)
            .collect::<Vec<_>>();
        assert!(committed.iter().any(|text| text == "go"), "N1/E2");
        assert!(
            committed
                .iter()
                .all(|text| !text.contains("resource revision two")),
            "N1/E2 request context leaked into Thread truth: {committed:?}"
        );
    }

    /// A real A2A server that answers every request with a fixed HTTP status + body.
    async fn serve_status(status: u16, body: &'static str) -> String {
        let code = axum::http::StatusCode::from_u16(status).unwrap();
        let app = axum::Router::new().fallback(move || async move { (code, body) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("a2a:http://{addr}")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_http_failures_veto_ack_without_fabricating_remote_completion() {
        // C1 HTTP 500 / C2 HTTP 200 malformed body. Both were admitted sends,
        // neither is a remote terminal receipt => diagnostic + veto + no commit.
        // This table exercises the real HTTP adapter, not just a fake transport.
        for backend in [
            serve_status(500, "upstream boom").await,
            serve("this is not a2a json at all").await,
        ] {
            let rec = Arc::new(Rec::default());
            let context = RuntimeRunContext::new().with_commit(rec.clone());
            let latch = context.physical_quiescence.clone();
            assert!(
                A2aRunExecutor::over_http()
                    .execute(activation(&backend), context)
                    .await
                    .is_err()
            );
            assert!(!latch.can_acknowledge_return());
            assert_eq!(
                rec.0.lock().unwrap().len(),
                1,
                "the pre-send Prepared boundary remains durable"
            );
        }
    }

    /// FMECA / cause-effect design for the external commit capability:
    /// C1=A2A returns a terminal task; C2=CommitCoordinator is absent/present.
    /// E1=no terminal state escapes; E2=input/output/state commit atomically.
    /// Constraint: an A2A result is external and can never be an ephemeral Run.
    /// Decision table: R1 C1+!C2=>E1/commit error; R2 C1+C2=>E2/success (covered
    /// by `a_completed_task_commits_reply_and_end`). This test owns R1 and guards
    /// against reintroducing the former optional no-op path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_commit_coordinator_fails_before_reporting_terminal() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"ok"}]}}}}"#,
        )
        .await;
        // RuntimeRunContext::new() carries no commit coordinator.
        let error = A2aRunExecutor::over_http()
            .execute(activation(&backend), RuntimeRunContext::new())
            .await
            .expect_err("R1/E1");
        assert!(error.to_string().contains("requires a CommitCoordinator"));
    }

    /// A remote task returned in the `failed` state is an execution fault, not a
    /// natural end — regression against silently projecting a remote failure as
    /// success (G26). The agent's message is still committed for the reader.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_remote_task_ends_in_error_not_success() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"failed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"model exploded"}]}}}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            state,
            RunState::Ended(EndCause::Error(Failure::Inference { ref code, .. }))
                if code == "a2a_task_failed"
        ));
        assert_eq!(
            rec.0.lock().unwrap().last().unwrap().messages[0].text_content(),
            "model exploded"
        );
    }

    /// A remote task returned in the `canceled` state ends as a cancellation, not a
    /// natural end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_canceled_remote_task_ends_cancelled() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"canceled"}}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    }

    #[tokio::test]
    async fn a_working_remote_task_is_polled_to_its_real_terminal_state() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"working"}}}"#,
            ),
            response(
                r#"{"kind":"task","id":"t","contextId":"c","status":{"state":"completed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"finished"}]}}}"#,
            ),
        ]));
        let rec = Arc::new(Rec::default());
        let state = scripted_executor(transport.clone())
            .execute(
                activation("a2a:http://remote.invalid"),
                RuntimeRunContext::new()
                    .with_commit(rec.clone())
                    .with_reader(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        assert_eq!(
            transport
                .seen
                .lock()
                .unwrap()
                .iter()
                .map(|(_, path, _)| path.as_str())
                .collect::<Vec<_>>(),
            vec!["/v1/a2a/message:send", "/v1/a2a/tasks/t"]
        );
    }

    #[tokio::test]
    async fn a2a_creation_and_polling_require_live_attempt_ownership() {
        // Causes: the fixtures below establish `a2a creation and polling require live attempt
        // ownership` with the concrete inputs, state, dependencies, and failure triggers used by
        // this case.
        // Constraints/invariants: the current fenced attempt and committed context are
        // authoritative; remote protocol state cannot become a parallel Run or transcript truth.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1=attempt authority is absent/current/lost/down;
        // C2=the task completes in message:send or remains pollable; C3=authority
        // is lost after Prepared but before send. Effects: E1=resolve
        // transport once and send exactly one message; E2=zero resolver/HTTP
        // calls; E3=retain Prepared but issue zero outbound requests.
        // Absence is the direct/embedded compatibility path.
        //
        // | Rule | Authority sequence       | Task       | Effect |
        // | O1   | absent                   | completed  | E1     |
        // | O2   | current                  | completed  | E1     |
        // | O3   | lost/down                | -          | E2     |
        // | O4   | current,current,lost     | -          | E3     |
        for (label, ownership) in [
            ("O1", None),
            (
                "O2",
                Some(
                    Arc::new(ScriptedOwnership::new([OwnershipDecision::Current]))
                        as Arc<dyn AttemptOwnershipVerifier>,
                ),
            ),
        ] {
            let transport = Arc::new(ScriptedTransport::new(vec![response(
                r#"{"task":{"kind":"task","id":"owned","contextId":"ctx","status":{"state":"completed"}}}"#,
            )]));
            let resolver = Arc::new(FixedTransportResolver::new(transport.clone()));
            let mut context = RuntimeRunContext::new().with_commit(Arc::new(Rec::default()));
            context.ownership = ownership;
            assert_eq!(
                A2aRunExecutor::new(resolver.clone())
                    .execute(activation("a2a:http://remote.invalid"), context)
                    .await
                    .expect("O1/O2 complete"),
                RunState::Ended(EndCause::NaturalEnd),
                "{label}/E1"
            );
            assert_eq!(resolver.resolutions.load(Ordering::SeqCst), 1, "{label}/E1");
            assert_eq!(transport.seen.lock().unwrap().len(), 1, "{label}/E1");
        }

        for (label, decision) in [
            ("O3-lost", OwnershipDecision::Lost),
            ("O3-down", OwnershipDecision::Unavailable),
        ] {
            let transport = Arc::new(ScriptedTransport::new(Vec::new()));
            let resolver = Arc::new(FixedTransportResolver::new(transport.clone()));
            let context = RuntimeRunContext::new()
                .with_commit(Arc::new(Rec::default()))
                .with_ownership(Arc::new(ScriptedOwnership::new([decision])));
            A2aRunExecutor::new(resolver.clone())
                .execute(activation("a2a:http://remote.invalid"), context)
                .await
                .expect_err("O3 stale authority fails before resolution");
            assert_eq!(resolver.resolutions.load(Ordering::SeqCst), 0, "{label}/E2");
            assert!(transport.seen.lock().unwrap().is_empty(), "{label}/E2");
        }

        let transport = Arc::new(ScriptedTransport::new(vec![response(
            r#"{"task":{"kind":"task","id":"working","contextId":"ctx","status":{"state":"working"}}}"#,
        )]));
        let resolver = Arc::new(FixedTransportResolver::new(transport.clone()));
        let context = RuntimeRunContext::new()
            .with_commit(Arc::new(Rec::default()))
            .with_ownership(Arc::new(ScriptedOwnership::new([
                OwnershipDecision::Current,
                OwnershipDecision::Current,
                OwnershipDecision::Lost,
            ])));
        A2aRunExecutor::new(resolver)
            .execute(activation("a2a:http://remote.invalid"), context)
            .await
            .expect_err("O4 ownership loss fences the first poll");
        assert_eq!(
            transport
                .seen
                .lock()
                .unwrap()
                .iter()
                .map(|(_, path, _)| path.as_str())
                .collect::<Vec<_>>(),
            Vec::<&str>::new(),
            "O4/E3"
        );
    }

    #[tokio::test]
    async fn a2a_resume_and_cancel_recheck_between_remote_operations() {
        // Causes: the fixtures below establish `a2a resume and cancel recheck between remote
        // operations` with the concrete inputs, state, dependencies, and failure triggers used by
        // this case.
        // Constraints/invariants: the current fenced attempt and committed context are
        // authoritative; remote protocol state cannot become a parallel Run or transcript truth.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1=a durable input-required task exists; C2=the
        // replacement authority stays current through resolver/operation or is
        // lost immediately before send/cancel. Effects: E1=resume sends once;
        // E2=cancel GETs then cancels once; E3=lost resume sends nothing; E4=lost
        // cancel performs its already-authorized GET but no cancel POST.
        // Successful current/absent rules are owned by the existing replacement
        // resume and durable-cancel tests; this table owns the between-operation
        // stale rules.
        //
        // | Rule | Operation | Authority sequence       | Effect |
        // | O1   | resume    | current,lost             | E3     |
        // | O2   | cancel    | current,current,lost     | E4     |
        let resume_transport = Arc::new(ScriptedTransport::new(vec![response(
            r#"{"task":{"kind":"task","id":"resume-task","contextId":"resume-ctx","status":{"state":"input-required"}}}"#,
        )]));
        let resume_rec = Arc::new(Rec::default());
        let resume_activation = activation("a2a:http://remote.invalid");
        let base_resume_context = || {
            RuntimeRunContext::new()
                .with_commit(resume_rec.clone())
                .with_reader(resume_rec.clone())
        };
        assert_eq!(
            scripted_executor(resume_transport.clone())
                .execute(resume_activation.clone(), base_resume_context())
                .await
                .expect("seed resume boundary"),
            RunState::Awaiting
        );
        let ticket = resume_rec
            .resume_ticket(&resume_activation.run_id)
            .expect("durable resume ticket");
        let command = ResumeCommand::from_ticket(&ticket, ResumeResult::Input("more".into()), 0);
        let stale_resume =
            base_resume_context().with_ownership(Arc::new(ScriptedOwnership::new([
                OwnershipDecision::Current,
                OwnershipDecision::Lost,
            ])));
        scripted_executor(resume_transport.clone())
            .resume(resume_activation, command, stale_resume)
            .await
            .expect_err("O1 lost authority fences resume send");
        assert_eq!(resume_transport.seen.lock().unwrap().len(), 1, "O1/E3");

        let active = r#"{"kind":"task","id":"cancel-task","contextId":"cancel-ctx","status":{"state":"working"}}"#;
        let cancel_transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"cancel-task","contextId":"cancel-ctx","status":{"state":"input-required"}}}"#,
            ),
            response(active),
        ]));
        let cancel_rec = Arc::new(Rec::default());
        let cancel_activation = activation("a2a:http://remote.invalid");
        let base_cancel_context = || {
            RuntimeRunContext::new()
                .with_commit(cancel_rec.clone())
                .with_reader(cancel_rec.clone())
        };
        assert_eq!(
            scripted_executor(cancel_transport.clone())
                .execute(cancel_activation.clone(), base_cancel_context())
                .await
                .expect("seed cancellation boundary"),
            RunState::Awaiting
        );
        let stale_cancel =
            base_cancel_context().with_ownership(Arc::new(ScriptedOwnership::new([
                OwnershipDecision::Current,
                OwnershipDecision::Current,
                OwnershipDecision::Lost,
            ])));
        scripted_executor(cancel_transport.clone())
            .cancel(cancel_activation, stale_cancel)
            .await
            .expect_err("O2 lost authority fences cancel POST");
        assert_eq!(
            cancel_transport
                .seen
                .lock()
                .unwrap()
                .iter()
                .map(|(_, path, _)| path.as_str())
                .collect::<Vec<_>>(),
            vec!["/v1/a2a/message:send", "/v1/a2a/tasks/cancel-task"],
            "O2/E4"
        );
    }

    #[tokio::test]
    async fn replacement_executor_reattaches_by_the_committed_task_id() {
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"remote-7","contextId":"ctx-7","status":{"state":"working"}}}"#,
            ),
            Err("connection dropped after task creation".to_string()),
            response(
                r#"{"kind":"task","id":"remote-7","contextId":"ctx-7","status":{"state":"completed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"recovered"}]}}}"#,
            ),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };

        let first = scripted_executor(transport.clone())
            .execute(activation.clone(), context())
            .await;
        assert!(first.is_err(), "the first process loses its poll response");
        assert_eq!(rec.run_state(&activation.run_id), Some(RunState::Running));
        assert_eq!(
            restored_accepted_reference(&context(), &activation)
                .unwrap()
                .unwrap()
                .task_id,
            "remote-7"
        );

        let state = scripted_executor(transport.clone())
            .execute(activation, context())
            .await
            .expect("replacement reattaches");
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
        let paths: Vec<String> = transport
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/v1/a2a/message:send",
                "/v1/a2a/tasks/remote-7",
                "/v1/a2a/tasks/remote-7"
            ],
            "recovery performs no second message:send"
        );
    }

    #[tokio::test]
    async fn replacement_executor_resumes_input_on_the_committed_context() {
        // Causes: the fixtures below establish `replacement executor` with the concrete inputs,
        // state, dependencies, and failure triggers used by this case.
        // Constraints/invariants: the current fenced attempt and committed context are
        // authoritative; remote protocol state cannot become a parallel Run or transcript truth.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 a fresh remote task returns InputRequired with
        // one reply; C2 its durable resume returns Completed with another reply.
        // Effects: E1 both replies commit in the existing ThreadCommit stream;
        // E2 C1 owns canonical assistant Step 0; E3 C2 reads that committed
        // prefix and owns Step 1; E4 the resume addresses the committed context.
        //
        // | Rule | first boundary | resume boundary | Effects |
        // | A1   | InputRequired  | -               | E1+E2  |
        // | A2   | committed A1   | Completed       | E1+E3+E4 |
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"remote-input","contextId":"ctx-input","status":{"state":"input-required","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"which file?"}]}}}}"#,
            ),
            response(
                r#"{"task":{"kind":"task","id":"remote-finished","contextId":"ctx-input","status":{"state":"completed","message":{"kind":"message","messageId":"m2","role":"agent","parts":[{"kind":"text","text":"done"}]}}}}"#,
            ),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };

        assert_eq!(
            scripted_executor(transport.clone())
                .execute(activation.clone(), context())
                .await
                .unwrap(),
            RunState::Awaiting
        );
        let ticket = rec
            .resume_ticket(&activation.run_id)
            .expect("durable ticket");
        let command =
            ResumeCommand::from_ticket(&ticket, ResumeResult::Input("README.md".into()), 0);

        assert_eq!(
            scripted_executor(transport.clone())
                .resume(activation.clone(), command, context())
                .await
                .expect("replacement resumes"),
            RunState::Ended(EndCause::NaturalEnd)
        );
        let assistant_steps = rec
            .committed_messages(&activation.thread_id)
            .into_iter()
            .filter(|message| message.role == Role::Assistant)
            .map(|message| message.id.assistant_step_of(&activation.run_id))
            .collect::<Vec<_>>();
        assert_eq!(assistant_steps, [Some(0), Some(1)], "A1-A2/E1-E3");
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let body = seen[1].2.as_deref().expect("resume body");
        assert!(body.contains(r#""contextId":"ctx-input""#));
        assert!(body.contains(r#""text":"README.md""#));
    }

    #[tokio::test]
    async fn resume_rejects_context_rebinding_and_retains_the_prepared_operation() {
        // Causes: C1 the committed task reference owns context ctx-original;
        // C2 resume sends to that context; C3 the remote response instead names
        // ctx-rebound. Effects: E1 reject the response; E2 retain Prepared B
        // with task A as predecessor. A cannot be used as proof that B stopped.
        // The positive C1+C2+matching row is
        // covered by `replacement_executor_resumes_input_on_the_committed_context`.
        //
        // | Rule | committed context | response context | Effects |
        // |---|---|---|---|
        // | R1 | ctx-original | ctx-original | admit replacement |
        // | R2 | ctx-original | ctx-rebound | E1+E2 |
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"remote-input","contextId":"ctx-original","status":{"state":"input-required"}}}"#,
            ),
            response(
                r#"{"task":{"kind":"task","id":"remote-rebound","contextId":"ctx-rebound","status":{"state":"completed"}}}"#,
            ),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };

        assert_eq!(
            scripted_executor(transport.clone())
                .execute(activation.clone(), context())
                .await
                .unwrap(),
            RunState::Awaiting
        );
        let ticket = rec
            .resume_ticket(&activation.run_id)
            .expect("durable ticket");
        let command = ResumeCommand::from_ticket(&ticket, ResumeResult::Input("answer".into()), 0);
        let error = scripted_executor(transport)
            .resume(activation.clone(), command, context())
            .await
            .expect_err("a remote response cannot rebind the committed context");
        assert!(error.to_string().contains("changed context"), "R2/E1");

        let prepared = match restored_remote_operation(&context(), &activation)
            .expect("decode committed operation")
            .expect("Prepared B remains committed")
        {
            RemoteOperationState::Prepared(intent) => intent,
            RemoteOperationState::Accepted(_) => panic!("rebound B must not be accepted"),
        };
        assert_eq!(
            prepared.predecessor_task_id.as_deref(),
            Some("remote-input"),
            "R2/E2"
        );
        assert_eq!(prepared.context_id, "ctx-original", "R2/E2");
    }

    #[tokio::test]
    async fn durable_cancel_addresses_the_committed_remote_task() {
        // C1 exact waiting task + C2 delivered cancel + C3 exact terminal GET
        // => success; HTTP 204 alone is not proof. Preserve the reference.
        let active = r#"{"kind":"task","id":"remote-cancel","contextId":"ctx-cancel","status":{"state":"input-required"}}"#;
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(&format!(r#"{{"task":{active}}}"#)),
            response(active),
            Ok(Response::new(204, Vec::new())),
            response(&active.replace("input-required", "canceled")),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };
        assert_eq!(
            scripted_executor(transport.clone())
                .execute(activation.clone(), context())
                .await
                .unwrap(),
            RunState::Awaiting
        );

        scripted_executor(transport.clone())
            .cancel(activation, context())
            .await
            .expect("remote cancellation is delivered");
        let paths: Vec<String> = transport
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/v1/a2a/message:send",
                "/v1/a2a/tasks/remote-cancel",
                "/v1/a2a/tasks/remote-cancel:cancel",
                "/v1/a2a/tasks/remote-cancel"
            ]
        );
    }

    #[tokio::test]
    async fn cancel_rejects_a_fetch_response_for_another_task_before_remote_abort() {
        // Causes: C1 task remote-pinned is committed; C2 cancellation fetches
        // that exact id; C3 the peer responds with active task remote-other.
        // Effects: E1 reject the mismatched response; E2 never issue a cancel
        // for either id. The matching active row is covered by
        // `durable_cancel_addresses_the_committed_remote_task`.
        //
        // | Rule | requested id | response id | Effects |
        // |---|---|---|---|
        // | R1 | remote-pinned | remote-pinned | cancel remote-pinned |
        // | R2 | remote-pinned | remote-other | E1+E2 |
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"remote-pinned","contextId":"ctx-pinned","status":{"state":"input-required"}}}"#,
            ),
            response(
                r#"{"kind":"task","id":"remote-other","contextId":"ctx-pinned","status":{"state":"working"}}"#,
            ),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };
        assert_eq!(
            scripted_executor(transport.clone())
                .execute(activation.clone(), context())
                .await
                .unwrap(),
            RunState::Awaiting
        );

        let error = scripted_executor(transport.clone())
            .cancel(activation, context())
            .await
            .expect_err("mismatched task identity must fail closed");
        assert!(error.to_string().contains("identity changed"), "R2/E1");
        let paths = transport
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec!["/v1/a2a/message:send", "/v1/a2a/tasks/remote-pinned"],
            "R2/E2"
        );
    }

    #[tokio::test]
    async fn failed_remote_cancel_is_retryable_against_the_same_committed_task() {
        // R1 lost cancel reply + working observation => veto/error, exact
        // reference retained. R2 a new observer cancels/observes the SAME task
        // terminal => success; never repeat message:send or clear the old veto.
        let active = r#"{"kind":"task","id":"remote-retry","contextId":"ctx-retry","status":{"state":"working"}}"#;
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(&format!(r#"{{"task":{active}}}"#)),
            response(
                r#"{"kind":"task","id":"remote-retry","contextId":"ctx-retry","status":{"state":"input-required"}}"#,
            ),
            response(active),
            Err("remote cancel unavailable".to_string()),
            response(active),
            response(active),
            Ok(Response::new(204, Vec::new())),
            response(&active.replace("working", "canceled")),
        ]));
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = || {
            RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone())
        };
        assert_eq!(
            scripted_executor(transport.clone())
                .execute(activation.clone(), context())
                .await
                .unwrap(),
            RunState::Awaiting
        );

        let error = scripted_executor(transport.clone())
            .cancel(activation.clone(), context())
            .await
            .expect_err("an unconfirmed remote cancel must remain retryable");
        assert!(error.to_string().contains("remote cancel unavailable"));
        scripted_executor(transport.clone())
            .cancel(activation, context())
            .await
            .expect("retry addresses the same durable task");

        let paths: Vec<String> = transport
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, path, _)| path.clone())
            .collect();
        assert_eq!(
            paths,
            vec![
                "/v1/a2a/message:send",
                "/v1/a2a/tasks/remote-retry",
                "/v1/a2a/tasks/remote-retry",
                "/v1/a2a/tasks/remote-retry:cancel",
                "/v1/a2a/tasks/remote-retry",
                "/v1/a2a/tasks/remote-retry",
                "/v1/a2a/tasks/remote-retry:cancel",
                "/v1/a2a/tasks/remote-retry",
            ]
        );
    }

    /// A remote task in `input-required` becomes a durable await boundary. The
    /// executor must also commit the agent's partial message and resume ticket, so a
    /// reader sees the "I need more input" prompt and a replacement can continue it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_input_required_task_commits_the_partial_message() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"input-required","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"which file?"}]}}}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Awaiting);
        let commits = rec.0.lock().unwrap();
        assert_eq!(
            commits.last().unwrap().messages[0].text_content(),
            "which file?",
            "the partial 'input-required' prompt is committed for the reader"
        );
        assert!(commits.last().unwrap().run.resume_ticket().is_some());
    }

    /// The `auth-required` twin: a durable external-event await whose partial
    /// message and resume ticket are committed together.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_auth_required_task_commits_the_partial_message() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"auth-required","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"please authenticate"}]}}}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let state = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Awaiting);
        let commits = rec.0.lock().unwrap();
        assert_eq!(
            commits.last().unwrap().messages[0].text_content(),
            "please authenticate",
            "the partial 'auth-required' prompt is committed for the reader"
        );
        assert_eq!(
            commits
                .last()
                .unwrap()
                .run
                .resume_ticket()
                .unwrap()
                .reason(),
            awaken_agent_contract::agent::awaiting::AwaitReason::ExternalEvent
        );
    }

    mod operation_state;

    fn resume_command(activation: &RunActivation) -> ResumeCommand {
        ResumeCommand {
            operation_id: None,
            correlation_id: "missing-ticket".into(),
            run_id: activation.run_id.clone(),
            thread_id: activation.thread_id.clone(),
            snapshot_id: activation.snapshot.id.clone(),
            catalog_fingerprint: activation
                .snapshot
                .resolved_spec
                .catalog_fingerprint
                .clone(),
            result: ResumeResult::Input("answer".into()),
            context_messages: Vec::new(),
            now_ms: 0,
        }
    }

    #[tokio::test]
    async fn restore_skips_unrelated_state_and_honors_a_later_remove() {
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![StateCommand::set(
                Scope::Run,
                MergePolicy::Disjoint,
                "unrelated",
                serde_json::json!(true),
            )],
        )
        .await
        .unwrap();
        assert_eq!(
            restored_accepted_reference(&context, &activation).unwrap(),
            None
        );

        let reference = TaskReference {
            endpoint: "http://remote.invalid".into(),
            task_id: "task-7".into(),
            context_id: "ctx-7".into(),
        };
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![accepted_reference_state(&reference).unwrap()],
        )
        .await
        .unwrap();
        assert_eq!(
            restored_accepted_reference(&context, &activation).unwrap(),
            Some(reference)
        );
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![clear_remote_operation_state()],
        )
        .await
        .unwrap();
        assert_eq!(
            restored_accepted_reference(&context, &activation).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn physical_attempt_recovery_requires_an_exact_committed_task_binding() {
        // Cause C1: committed state contains a complete A2A Accepted task.
        // Cause C2: its endpoint matches the immutable publication binding.
        // Cause C3: state is Prepared without an accepted task.
        // Effect E1: only C1+C2 selects continuation handoff; all other rows
        // retain predecessor quiescence and cannot repeat `message:send`.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // |---|---|---|---|---|
        // | R1 | no | n/a | no | require predecessor quiescence |
        // | R2 | yes | no | no | require predecessor quiescence |
        // | R3 | yes | yes | no | continue durable remote operation |
        // | R4 | no | n/a | yes | require predecessor quiescence |
        let executor = A2aRunExecutor::over_http();
        let activation = activation("a2a:http://remote.invalid");
        let empty = RuntimeRunContext::new();
        assert_eq!(
            executor.physical_attempt_recovery(&activation, &empty),
            AttemptRecoveryMode::RequirePredecessorQuiescence,
            "R1"
        );

        let rec = Arc::new(Rec::default());
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![
                accepted_reference_state(&TaskReference {
                    endpoint: "http://other.invalid".into(),
                    task_id: "task-7".into(),
                    context_id: "ctx-7".into(),
                })
                .unwrap(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            executor.physical_attempt_recovery(&activation, &context),
            AttemptRecoveryMode::RequirePredecessorQuiescence,
            "R2"
        );
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![
                accepted_reference_state(&TaskReference {
                    endpoint: "http://remote.invalid".into(),
                    task_id: "task-7".into(),
                    context_id: "ctx-7".into(),
                })
                .unwrap(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            executor.physical_attempt_recovery(&activation, &context),
            AttemptRecoveryMode::ContinueDurableRemoteOperation,
            "R3"
        );
        let prepared = prepare_remote_operation(
            "http://remote.invalid",
            "ctx-7",
            Some("task-7"),
            "message-next",
            "next",
        )
        .unwrap();
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![prepared_operation_state(&prepared).unwrap()],
        )
        .await
        .unwrap();
        assert_eq!(
            executor.physical_attempt_recovery(&activation, &context),
            AttemptRecoveryMode::RequirePredecessorQuiescence,
            "R4"
        );
    }

    #[tokio::test]
    async fn resume_requires_reader_ticket_and_durable_task_in_that_order() {
        let activation = activation("a2a:http://remote.invalid");
        let executor = A2aRunExecutor::over_http();
        let command = resume_command(&activation);
        assert!(
            executor
                .resume(
                    activation.clone(),
                    command.clone(),
                    RuntimeRunContext::new()
                )
                .await
                .is_err()
        );

        let empty = Arc::new(Rec::default());
        assert!(
            executor
                .resume(
                    activation.clone(),
                    command,
                    RuntimeRunContext::new().with_reader(empty),
                )
                .await
                .is_err()
        );

        let rec = Arc::new(Rec::default());
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        let mut waiting = task_with(None, &[], &[]);
        waiting.status.state = TaskState::InputRequired;
        let ticket = awaiting_ticket(&activation, &waiting);
        commit_boundary(
            &context,
            &activation,
            RunDisposition::awaiting(ticket.clone()),
            Vec::new(),
            Vec::new(),
        )
        .await
        .unwrap();
        let state = executor
            .resume(
                activation,
                ResumeCommand::from_ticket(&ticket, ResumeResult::Input("answer".into()), 0),
                context,
            )
            .await
            .expect("missing durable task is a deterministic terminal Run failure");
        assert!(
            matches!(
                &state,
                RunState::Ended(EndCause::Error(Failure::Inference { code, .. }))
                    if code == "a2a_durable_state_invalid"
            ),
            "invalid durable state must not enter transient dispatch retry: {state:?}"
        );
    }

    #[tokio::test]
    async fn execute_preserves_a_task_pinned_to_another_endpoint() {
        // Causes: C1 a durable accepted task exists; C2 the immutable endpoint
        // differs. Effects: E1 reject before outbound IO; E2 retain the exact
        // task and veto physical ACK. Terminalizing would erase the only owner
        // of a possibly live remote effect.
        //
        // | Rule | C1 | C2 | Effects |
        // |---|---|---|---|
        // | R1 | yes | mismatch | E1+E2 |
        let rec = Arc::new(Rec::default());
        let activation = activation("a2a:http://remote.invalid");
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![
                accepted_reference_state(&TaskReference {
                    endpoint: "http://old.invalid".into(),
                    task_id: "task-7".into(),
                    context_id: "ctx-7".into(),
                })
                .unwrap(),
            ],
        )
        .await
        .unwrap();

        let latch = context.physical_quiescence.clone();
        let error = A2aRunExecutor::over_http()
            .execute(activation.clone(), context.clone())
            .await
            .expect_err("endpoint mismatch retains the remote obligation");
        assert!(error.to_string().contains("belongs to endpoint"), "R1/E1");
        assert!(!latch.can_acknowledge_return(), "R1/E2");
        assert_eq!(
            restored_accepted_reference(&context, &activation)
                .unwrap()
                .unwrap()
                .task_id,
            "task-7",
            "R1/E2"
        );
    }

    #[tokio::test]
    async fn cancel_without_a_reference_is_idempotent_and_terminal_is_a_noop() {
        let activation = activation("a2a:http://remote.invalid");
        let empty = Arc::new(Rec::default());
        scripted_executor(Arc::new(ScriptedTransport::new(Vec::new())))
            .cancel(
                activation.clone(),
                RuntimeRunContext::new().with_reader(empty),
            )
            .await
            .unwrap();

        let transport = Arc::new(ScriptedTransport::new(vec![response(
            r#"{"kind":"task","id":"task-7","contextId":"ctx-7","status":{"state":"completed"}}"#,
        )]));
        let rec = Arc::new(Rec::default());
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        commit_boundary(
            &context,
            &activation,
            RunDisposition::running(activation.run_id.clone()),
            Vec::new(),
            vec![
                accepted_reference_state(&TaskReference {
                    endpoint: "http://remote.invalid".into(),
                    task_id: "task-7".into(),
                    context_id: "ctx-7".into(),
                })
                .unwrap(),
            ],
        )
        .await
        .unwrap();
        scripted_executor(transport.clone())
            .cancel(activation, context)
            .await
            .unwrap();
        assert_eq!(transport.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_pre_cancelled_poll_commits_a_cancelled_terminal_boundary() {
        // R1 cooperative cancel + exact canceled observation => committed
        // Cancelled; the HTTP delivery response alone cannot select this row.
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(
                r#"{"task":{"kind":"task","id":"task-7","contextId":"ctx-7","status":{"state":"working"}}}"#,
            ),
            Ok(Response::new(204, Vec::new())),
            response(
                r#"{"kind":"task","id":"task-7","contextId":"ctx-7","status":{"state":"canceled"}}"#,
            ),
        ]));
        let cancellation = awaken_runtime_contract::CancellationToken::new();
        cancellation.cancel();
        let rec = Arc::new(Rec::default());
        let state = scripted_executor(transport)
            .execute(
                activation("a2a:http://remote.invalid"),
                RuntimeRunContext::new()
                    .with_commit(rec.clone())
                    .with_reader(rec)
                    .with_cancellation(cancellation),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::Cancelled));
    }
}
