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
use awaken_protocol_managed::{ManagedState, router, tunnels_router};
use awaken_session_contract::{
    OutcomeDrive, RunError, SessionRuntime, StepOutcome, ToolPermissionDecision,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// The narrowest Runtime that lets the routes mount; no refused case below
/// reaches it, which is the point — refusal happens before any domain work.
struct AcceptingFake;

#[async_trait::async_trait]
impl SessionRuntime for AcceptingFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_runtime_contract::authority_lease::AuthorityProofStarted,
        awaken_session_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        let _ = awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }
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
    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &str,
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
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

fn managed() -> Router {
    router(Arc::new(ManagedState::new(AcceptingFake)))
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
