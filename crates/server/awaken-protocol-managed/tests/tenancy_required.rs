//! Tenancy is a *required* ingress coordinate (ADR-0051).
//!
//! The defect this pins down: the tenant handle used to arrive as an optional
//! request-local value, and each handler decided for itself what its absence
//! meant. Nine of them decided it meant the seeded `DEFAULT_SCOPE`, so a request
//! that no edge had scoped still produced a durable write — owned by a tenant
//! nobody authenticated as, and invisible to every authenticated one.
//!
//! These cases drive the real routers with NO edge stamp and assert refusal, so
//! reintroducing any per-handler default turns them red. Every one of them
//! passes on the defaulting code only if the refusal is removed: see the
//! sibling `edge_stamped` fixtures, which supply the stamp a composition root
//! installs, for the admitted half of the same decision table.

mod support;

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_protocol_managed::test_support::EmptyManagedTunnelApplication;
use awaken_protocol_managed::{router, tunnels_router};
use awaken_session_application_contract::StepOutcome;
use awaken_session_error_contract::RunError;
use awaken_session_execution_contract::{OutcomeDrive, SessionRuntime, ToolPermissionDecision};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// The narrowest Runtime that lets the routes mount; no refused case below
/// reaches it, which is the point — refusal happens before any domain work.
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

fn managed() -> Router {
    router(Arc::new(support::state(AcceptingFake)))
}

async fn status_of(app: &Router, method: &str, uri: &str, body: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(json) => {
            builder = builder.header("content-type", "application/json");
            Body::from(json.to_owned())
        }
        None => Body::empty(),
    };
    app.clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .expect("router response")
        .status()
}

const CREATE_SESSION: &str = r#"{"agent":"coder","environment_id":"env_local"}"#;

/// Cause/effect decision table for the ingress tenancy coordinate.
///
/// Causes: C1 the trusted edge stamped a non-empty `WorkspaceScope`; C2 the
/// route is tenant-owned (reads or writes a Workspace-scoped resource).
/// Effects: E1 the request is refused with `404`, before any domain work and
/// without disclosing whether the addressed resource exists; E2 the request
/// reaches its handler and answers on its own merits (any status but the
/// tenancy `404`).
///
/// | Rule | C1 | C2 | Effect |
/// |------|----|----|--------|
/// | R1   | F  | T  | E1     |
/// | R2   | T  | T  | E2     |
///
/// R1 is the regression fence: on the pre-fix code each of these routes
/// silently resolved to `DEFAULT_SCOPE` and answered `200`/`201` instead.
#[tokio::test]
async fn an_unstamped_request_is_refused_on_every_tenant_owned_route() {
    let managed = managed();
    let tunnels = tunnels_router(Arc::new(EmptyManagedTunnelApplication));

    // R1 — no edge stamp anywhere. Each route is refused.
    for (method, uri, body) in [
        // The collection list: previously returned the seeded tenant's sessions.
        ("GET", "/v1/sessions", None),
        // The durable write: previously created a session owned by "default".
        ("POST", "/v1/sessions", Some(CREATE_SESSION)),
        // The per-id ownership guard: previously compared against "default".
        ("GET", "/v1/sessions/sess_unknown", None),
        (
            "POST",
            "/v1/sessions/sess_unknown/events",
            Some(r#"{"events":[]}"#),
        ),
    ] {
        assert_eq!(
            status_of(&managed, method, uri, body).await,
            StatusCode::NOT_FOUND,
            "R1 {method} {uri} must refuse an unstamped request"
        );
    }
    assert_eq!(
        status_of(&tunnels, "GET", "/v1/tunnels", None).await,
        StatusCode::NOT_FOUND,
        "R1 GET /v1/tunnels must refuse an unstamped request"
    );

    // R2 — the same routes behind the one composition-root stamp are no longer
    // answered by the tenancy refusal.
    let stamped = support::edge_stamped(managed);
    assert_ne!(
        status_of(&stamped, "GET", "/v1/sessions", None).await,
        StatusCode::NOT_FOUND,
        "R2 a stamped list is served"
    );
    assert_ne!(
        status_of(&stamped, "POST", "/v1/sessions", Some(CREATE_SESSION)).await,
        StatusCode::NOT_FOUND,
        "R2 a stamped create is served"
    );
}

/// An empty stamp is not a stamp. A composition that inserts `WorkspaceScope("")`
/// (or whitespace) has resolved no tenant, and must be indistinguishable from
/// having resolved none at all — otherwise `""` becomes a third, invisible
/// tenant that owns every write made through a misconfigured edge.
#[tokio::test]
async fn an_empty_edge_stamp_is_indistinguishable_from_an_absent_one() {
    // Same table as above with C1 refined: C1a absent, C1b present-but-blank,
    // C1c present and non-empty. R1a/R1b => E1; R1c => E2.
    for blank in ["", "   ", "\t"] {
        let app = managed().layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            blank.to_string(),
        )));
        assert_eq!(
            status_of(&app, "GET", "/v1/sessions", None).await,
            StatusCode::NOT_FOUND,
            "R1b a blank stamp ({blank:?}) is refused"
        );
    }
    let app = managed().layer(axum::Extension(awaken_tenancy::WorkspaceScope(
        "wrkspc_real".into(),
    )));
    assert_ne!(
        status_of(&app, "GET", "/v1/sessions", None).await,
        StatusCode::NOT_FOUND,
        "R1c a real stamp is served"
    );
}
