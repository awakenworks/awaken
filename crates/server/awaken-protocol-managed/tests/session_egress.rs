//! `POST /v1/sessions` resolves the session's `environment_id` to its networking
//! policy and stages the exact `EnvironmentSnapshot`: semantic MCP/package
//! exceptions are resolved before the snapshot is frozen and the Runtime sees
//! only one canonical network fact. No coarse boolean becomes a second policy
//! authority.

mod support;

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_protocol_managed::{
    ManagedState, environment_authoring_router, environment_work_router,
};
use awaken_session_application_contract::StepOutcome;
use awaken_session_execution_contract::{OutcomeDrive, SessionRuntime, ToolPermissionDecision};
use awaken_session_types::RunError;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Records each complete projection's exact frozen network policy.
struct CapturingFake {
    egress: Arc<Mutex<Vec<awaken_session_configuration_contract::SessionNetworkPolicy>>>,
}

#[async_trait::async_trait]
impl SessionRuntime for CapturingFake {
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

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
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

awaken_session_execution_contract::neutral_role!(CoordinationRuntime for CapturingFake);

awaken_session_execution_contract::neutral_role!(BudgetRuntime for CapturingFake);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for CapturingFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for CapturingFake);

awaken_session_execution_contract::neutral_role!(ThreadReadRuntime for CapturingFake);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for CapturingFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for CapturingFake);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for CapturingFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for CapturingFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_types::RunError,
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
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_types::RunError> {
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
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        if let Some(init) = awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )? {
            self.egress.lock().unwrap().push(init.environment.network);
        }
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
    ) -> Result<(), awaken_session_types::RunError> {
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
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for CapturingFake {
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
    ) -> Result<u64, awaken_session_types::RunError> {
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
    ) -> Result<Option<awaken_session_execution_contract::OutcomeDrive>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for CapturingFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_mcp_contract::StageMcpAttachment,
    ) -> Result<awaken_session_mcp_contract::McpRealizationReceipt, RunError> {
        Ok(awaken_session_mcp_contract::McpRealizationReceipt {
            receipt_fingerprint: request.fingerprint(),
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
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
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    support::Call::new(status, value)
}

async fn realize_accepted_sessions(managed: &ManagedState) {
    // Cause: Cloud create returns a complete frozen root before the Sandbox
    // exists. Effect: the original supervisor, rather than HTTP create,
    // delivers its exact network policy to Runtime. Rule E1.
    let report = managed
        .session_application()
        .reconcile_session_realizations()
        .await;
    assert!(
        report.failures.is_empty(),
        "E1 accepted Session realizes: {:?}",
        report.failures
    );
}

#[tokio::test]
async fn session_carries_the_exact_frozen_environment_network() {
    let (environment_authoring, environment_execution) =
        awaken_protocol_managed_testkit::environment_components(awaken_time::system_clock());
    let egress = Arc::new(Mutex::new(Vec::new()));
    let managed = Arc::new(
        support::state_with_mcp(CapturingFake {
            egress: egress.clone(),
        })
        .await
        .with_environments(environment_execution.clone()),
    );
    // Mounted the way the composition root does: one trusted edge stamp in
    // front of every merged surface, never a per-handler default.
    let app = support::edge_stamped(
        awaken_protocol_managed::router(managed.clone())
            .merge(environment_authoring_router(environment_authoring))
            .merge(environment_work_router(
                environment_execution,
                awaken_time::system_clock(),
            )),
    );

    // A limited-networking environment.
    let limited = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "l", "config": { "type": "cloud", "networking": { "type": "limited" } } })),
    )
    .await.expect(StatusCode::OK);
    let limited_id = limited["id"].as_str().unwrap().to_string();

    // A session on it stages the exact closed policy.
    let _ = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": limited_id })),
    )
    .await
    .expect(StatusCode::OK);
    realize_accepted_sessions(&managed).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_configuration_contract::SessionNetworkPolicy::None)
    );

    // An unrestricted environment keeps the exact open policy.
    let open = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "o", "config": { "type": "cloud", "networking": { "type": "unrestricted" } } })),
    )
    .await.expect(StatusCode::OK);
    let open_id = open["id"].as_str().unwrap().to_string();
    let _ = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": open_id })),
    )
    .await
    .expect(StatusCode::OK);
    realize_accepted_sessions(&managed).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted)
    );

    // An omitted Environment is refused before any policy is frozen.
    //
    // This block used to claim an omitted Environment "defaults to host
    // network" and assert Unrestricted. It never tested that: `environment_id`
    // is a required field, the create answered 400, and the assertion was
    // reading the policy the *previous* session had frozen. Naming the status
    // is what surfaced it.
    let frozen_before = egress.lock().unwrap().len();
    let _ = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" })))
        .await
        .expect(StatusCode::BAD_REQUEST);
    assert_eq!(
        egress.lock().unwrap().len(),
        frozen_before,
        "a refused create freezes no network policy"
    );
}

/// Limited-network behavior cause graph:
/// C1 Environment is limited; C2 MCP exception enabled; C3 exact MCP target is
/// present at Session creation; C4 package-manager exception enabled; C5 the
/// published ACP execution selects Workload MCP custody. Worker custody never
/// authorizes a direct Sandbox-to-upstream exception (Environment unit row S3).
/// E1 the sole snapshot compiler adds exact selected hosts; E2 Runtime receives
/// that frozen canonical policy; E3 a disabled exception grants nothing.
///
/// | Rule | C2 | C3 | C4 | Runtime effect |
/// |---|---|---|---|---|
/// | N1 | F | T | F | explicit hosts only; MCP denied |
/// | N2 | T | T | F | Worker custody: no direct MCP host added |
/// | N3 | T | F | F | no ambient/default MCP host added |
/// | N4 | F | F | T | canonical public registry hosts added |
/// N1-N4 use C5=false (Native/Worker). C5=true direct Workload admission is
/// the Environment compiler's S6 row, avoiding another protocol policy oracle.
#[tokio::test]
async fn limited_network_exceptions_change_the_prepared_runtime_policy() {
    // N1-N4 retain Native execution: MCP is Worker-owned, so an enabled
    // exception still cannot grant a direct Sandbox route. C5=true Workload
    // behavior is owned by the Environment compiler's S6 decision row in
    // `snapshot_compilation_follows_the_network_and_runtime_decision_table`.
    let (environment_authoring, environment_execution) =
        awaken_protocol_managed_testkit::environment_components(awaken_time::system_clock());
    let egress = Arc::new(Mutex::new(Vec::new()));
    let managed = Arc::new(
        support::state_with_mcp(CapturingFake {
            egress: egress.clone(),
        })
        .await
        .with_environments(environment_execution.clone()),
    );
    // Mounted the way the composition root does: one trusted edge stamp in
    // front of every merged surface, never a per-handler default.
    let app = support::edge_stamped(
        awaken_protocol_managed::router(managed.clone())
            .merge(environment_authoring_router(environment_authoring))
            .merge(environment_work_router(
                environment_execution,
                awaken_time::system_clock(),
            )),
    );

    async fn environment(app: &Router, networking: Value) -> String {
        let value = call(
            app,
            "POST",
            "/v1/environments",
            Some(json!({
                "name": "limited",
                "config": { "type": "cloud", "networking": networking }
            })),
        )
        .await
        .expect(StatusCode::OK);
        value["id"].as_str().unwrap().to_string()
    }
    async fn create_session(
        app: &Router,
        managed: &ManagedState,
        environment_id: &str,
        with_mcp: bool,
    ) {
        let mut body = json!({ "agent": "a", "environment_id": environment_id });
        if with_mcp {
            body["agent"] = json!({
                "id": "a",
                "type": "agent_with_overrides",
                "mcp_servers": [{
                    "type": "url",
                    "name": "docs",
                    "url": "https://Docs.Example.test:443/rpc"
                }],
                "tools": [{"type": "mcp_toolset", "mcp_server_name": "docs"}]
            });
        }
        let _response = call(app, "POST", "/v1/sessions", Some(body))
            .await
            .expect_for(StatusCode::OK, "expected status");
        realize_accepted_sessions(managed).await;
    }

    let disabled = environment(
        &app,
        json!({
            "type": "limited",
            "allowed_hosts": ["api.example.test"],
            "allow_mcp_servers": false
        }),
    )
    .await;
    create_session(&app, &managed, &disabled, true).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(
            &awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example.test".into()]
            }
        ),
        "N1 disabled MCP exception has no runtime effect"
    );

    let enabled = environment(
        &app,
        json!({ "type": "limited", "allow_mcp_servers": true }),
    )
    .await;
    create_session(&app, &managed, &enabled, true).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_configuration_contract::SessionNetworkPolicy::None),
        "N2 Worker-owned MCP never authorizes Sandbox-to-upstream bypass"
    );
    create_session(&app, &managed, &enabled, false).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_session_configuration_contract::SessionNetworkPolicy::None),
        "N3 no declared MCP target means no ambient fallback"
    );

    let packages = environment(
        &app,
        json!({ "type": "limited", "allow_package_managers": true }),
    )
    .await;
    create_session(&app, &managed, &packages, false).await;
    let expected = awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
        hosts: awaken_environment_contract::PUBLIC_PACKAGE_REGISTRY_HOSTS
            .iter()
            .map(ToString::to_string)
            .collect(),
    }
    .normalized();
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&expected),
        "N4 package-manager exception reaches Runtime as the canonical catalog"
    );
}
