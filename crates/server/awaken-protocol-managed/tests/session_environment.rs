//! Session ↔ environment association (Managed Agents contract): the environment is
//! bound at session creation (defaulting to `env_local` when omitted), echoed on the
//! Session, and immutable for the session's lifetime — `POST /v1/sessions/{id}`
//! updates only `title`/`metadata`, so an `environment_id` sent to update rejects
//! the whole mutation and the Session keeps its create-time Environment snapshot.
//!
//! Scope note: this locks the WIRE/record association + immutability. It does not
//! assert Sandbox realization, which is covered by Runtime Host provisioning tests.

mod support;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_application_contract::StepOutcome;
use awaken_session_error_contract::RunError;
use awaken_session_execution_contract::{OutcomeDrive, SessionRuntime, ToolPermissionDecision};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

struct AcceptingFake;

#[async_trait::async_trait]
impl SessionRuntime for AcceptingFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(CoordinationRuntime for AcceptingFake);

awaken_session_execution_contract::neutral_role!(BudgetRuntime for AcceptingFake);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for AcceptingFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for AcceptingFake);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for AcceptingFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for AcceptingFake);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for AcceptingFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for AcceptingFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_runtime_contract::authority_lease::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        let _ = awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for AcceptingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_agent_contract::agent::message::Message>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        // Causes: C1 these association tests commit no Run; C2 GET refreshes
        // through the sole atomic recovery port. Effect E1 is `None`, preserving
        // the authored Session snapshot. Decision rule R1=C1+C2=>E1.
        // Constraints/invariants: the fake never reconstructs recovery state
        // from parallel message/lifecycle reads.
        Ok(None)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_error_contract::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for AcceptingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
    }

    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("unused"))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> support::Call<serde_json::Value> {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    support::Call::new(status, value)
}

async fn app() -> (Router, String) {
    let (environment_authoring, environment_execution) =
        awaken_protocol_managed_testkit::environment_components();
    let environment = environment_authoring
        .application()
        .create(awaken_environment_contract::CreateEnvironmentCommand {
            command_id: "session-environment-test".into(),
            name: "Custom".into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: awaken_environment_contract::EnvironmentConfig::SelfHosted,
            sandbox_policy: None,
        })
        .await
        .expect("publish Environment through the Control application");
    (
        support::edge_stamped(awaken_protocol_managed::router(std::sync::Arc::new(
            support::state(AcceptingFake).with_environments(environment_execution),
        ))),
        environment.id,
    )
}

#[tokio::test]
async fn environment_is_pinned_at_creation() {
    // Causes: C1 a Control-published current Environment id is supplied; C2 the
    // SDK-required id is omitted. Effects: E1 C1 is frozen and echoed; E2 C2 is
    // rejected before Session creation. Constraints/invariants: Managed never
    // turns a missing Environment into ambient local execution. Decision rules:
    // R1=C1=>E1; R2=C2=>E2.
    let (app, environment_id) = app().await;

    // Explicit environment is echoed on the session.
    let session = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": environment_id })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(session["environment_id"], environment_id);

    // Omitting the SDK-required field fails before Session creation.
    let defaulted = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" })))
        .await
        .expect(StatusCode::BAD_REQUEST);
    assert_eq!(defaulted["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn environment_is_immutable_across_update() {
    let (app, environment_id) = app().await;

    let session = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": environment_id })),
    )
    .await
    .expect(StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();

    // Causes: C1 update omits the immutable Environment and supplies a title; C2
    // update supplies another Environment plus an otherwise-valid title.
    // Effects: E1 C1 changes only the title; E2 C2 rejects the whole mutation and
    // a subsequent GET preserves both pinned Environment and old title.
    // Constraints/invariants: one create-time Environment snapshot owns the
    // Session lifetime; update has no alternate rebinding path.
    //
    // Decision table:
    // | environment_id | title | status | environment | title |
    // |----------------|-------|--------|-------------|-------|
    // | absent         | set   | 200    | env_a       | set   |
    // | env_b          | set   | 400    | env_a       | old   |
    let renamed = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({ "title": "renamed" })),
    )
    .await
    .expect_for(StatusCode::OK, "C1/E1");
    assert_eq!(renamed["environment_id"], environment_id, "C1/E1");
    assert_eq!(renamed["title"], "renamed", "C1/E1");

    let updated = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({ "title": "must-not-apply", "environment_id": "env_b" })),
    )
    .await
    .expect(StatusCode::BAD_REQUEST);
    assert_eq!(updated["error"]["type"], "invalid_request_error");

    // And a fresh GET still reports the create-time environment.
    let got = call(&app, "GET", &format!("/v1/sessions/{id}"), None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(got["environment_id"], environment_id);
    assert_eq!(got["title"], "renamed", "C2/E2");
}
