// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Component regression over the original Session projection and MCP owner.
// @formal reason: This classifies the actual failure/cleanup assertions, not a formal-proof claim.
//! A real Managed claim loses its original local projection after ready admission.
use super::*;
use crate::host::tests::ManagedSessionRootFixture;
use crate::installed_session::InstalledSessions;
use crate::session_realizations::{SessionRealizationAccess, SessionRealizationSource};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct RemoveAfterReadyRead {
    original: Arc<InstalledSessions>,
    session: &'static str,
    armed: AtomicBool,
    admitted: AtomicBool,
    removals: AtomicUsize,
}

impl SessionRealizationSource for RemoveAfterReadyRead {
    fn session_ids(&self) -> Result<Vec<String>, crate::realization_error::RealizationLock> {
        SessionRealizationSource::session_ids(self.original.as_ref())
    }
    fn remove_session(
        &self,
        session: &str,
    ) -> Result<bool, crate::realization_error::RealizationLock> {
        SessionRealizationSource::remove_session(self.original.as_ref(), session)
    }
    fn acquire_realization<'a>(
        &'a self,
        session: &'a str,
        admit: &'a mut (dyn FnMut(SessionRealizationAccess<'_>) + Send),
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<
            awaken_session_execution_contract::SessionRealizationPermit,
            crate::realization_error::RealizationLock,
        >,
    > {
        Box::pin(async move {
            let permit = SessionRealizationSource::acquire_realization(
                self.original.as_ref(),
                session,
                admit,
            )
            .await?;
            if self.armed.load(Ordering::SeqCst) {
                self.admitted.store(true, Ordering::SeqCst);
            }
            Ok(permit)
        })
    }
    fn install(&self, session: &str) -> Result<(), crate::realization_error::RealizationLock> {
        SessionRealizationSource::install(self.original.as_ref(), session)
    }

    fn visit(
        &self,
        session: &str,
        visit: &mut dyn FnMut(SessionRealizationAccess<'_>),
    ) -> Result<bool, crate::realization_error::RealizationLock> {
        let mut was_ready = false;
        let result =
            SessionRealizationSource::visit(self.original.as_ref(), session, &mut |access| {
                was_ready = access.inputs.baseline.is_some() && access.run.session_dispatch();
                visit(access);
            });
        // The original lookup, callback and mutex release really completed.
        // The original public removal then destroys that exact installed Session.
        // Never alter the returned ready result or fabricate an absent callback.
        if result.is_ok()
            && session == self.session
            && was_ready
            && self.admitted.load(Ordering::SeqCst)
            && self.armed.swap(false, Ordering::SeqCst)
        {
            assert!(
                self.original.remove(session).is_some(),
                "original projection must actually be removed"
            );
            self.removals.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
    fn replace_frozen(
        &self,
        session: &str,
        update: &mut dyn FnMut(&mut awaken_session_realization::frozen_inputs::FrozenSessionInputs),
    ) -> Result<(), crate::realization_error::RealizationLock> {
        SessionRealizationSource::replace_frozen(self.original.as_ref(), session, update)
    }
    fn finish_realization_revocation(
        &self,
        session: &str,
        selected: &crate::session_realizations::SessionRealizationRevocationSelection,
        owner: &awaken_session_realization::environment_owner::SessionEnvironmentOwner,
        disposed: bool,
    ) -> bool {
        SessionRealizationSource::finish_realization_revocation(
            self.original.as_ref(),
            session,
            selected,
            owner,
            disposed,
        )
    }
    fn retain_publication(
        &self,
        session: &str,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), crate::realization_error::RealizationError> {
        SessionRealizationSource::retain_publication(self.original.as_ref(), session, publication)
    }
    fn publish_resource_requirements(
        &self,
        session: &str,
        workspace: &str,
        asserted: Option<&awaken_session_configuration_contract::resource::SessionRealizationLease>,
        compiled: crate::staged_resources::CompiledEffectiveInputs,
        versions: Vec<awaken_resource_contract::SkillVersion>,
        effect_key: String,
    ) -> Result<(), crate::realization_error::RealizationError> {
        SessionRealizationSource::publish_resource_requirements(
            self.original.as_ref(),
            session,
            workspace,
            asserted,
            compiled,
            versions,
            effect_key,
        )
    }
}

#[derive(Default)]
struct RecordUnexpectedStage(Mutex<Vec<crate::session_mcp::StageMcpAttachment>>);

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for RecordUnexpectedStage {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, awaken_session_types::RunError> {
        self.0.lock().unwrap().push(request);
        Err(awaken_session_types::RunError::classified(
            "unexpected_direct_stage",
            "Managed claim entered ordinary MCP",
        ))
    }
    async fn publish_mcp_generation(
        &self,
        _generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), awaken_session_types::RunError> {
        panic!("no generation may be published after original projection loss")
    }
    async fn drain_mcp_generation(
        &self,
        _generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), awaken_session_types::RunError> {
        panic!("no generation was staged for cleanup")
    }
}

#[tokio::test]
async fn managed_claim_projection_loss_never_enters_ordinary_mcp_staging() {
    use awaken_dispatch::{DispatchClaims as _, DispatchEnqueue as _, WorkerResolver as _};
    // M1 a genuine parent-affined child Queue claim has immutable Session authority.
    // M2 its real SQLite Root installed a complete canonical local projection.
    // M3 that original projection is actually removed after its ready read.
    // E1 return NotReady; E2 zero ordinary MCP calls and zero fabricated Root.
    let session = "managed-claim-projection-loss";
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let queue = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .unwrap(),
    );
    let root = ManagedSessionRootFixture::new().await;
    let spy = Arc::new(RecordUnexpectedStage::default());
    let mut dependencies = root.configure(
        super::super::test_support::adoption_dependencies()
            .await
            .with_ephemeral_dispatch(queue.clone())
            .with_session_execution(
                crate::SessionExecutionDependencies::default()
                    .with_mcp_attachment_realizer(spy.clone()),
            ),
    );
    let observed = Arc::new(std::sync::OnceLock::new());
    let observe = observed.clone();
    dependencies.realization_source = Some(Box::new(move |original| {
        let source = Arc::new(RemoveAfterReadyRead {
            original,
            session,
            armed: AtomicBool::new(false),
            admitted: AtomicBool::new(false),
            removals: AtomicUsize::new(0),
        });
        observe
            .set(source.clone())
            .unwrap_or_else(|_| panic!("one original source composition"));
        source
    }));
    let host = dependencies.build().unwrap();
    crate::test_application_bindings::select_standalone(&host).unwrap();
    let source = observed.get().unwrap();
    assert!(Arc::ptr_eq(
        &source.original,
        &host.run_assembly.installed_sessions
    ));
    assert!(
        host.capabilities.worker.session_control.is_none(),
        "exercise the real co-located admission path"
    );
    let activation = test_activation(
        "managed-projection-loss-child",
        "run-managed-projection-loss",
    );
    let managed = host.clone();
    root.install_projection(
        &managed,
        session,
        crate::host::empty_frozen_projection_for_snapshot(
            host.run_assembly.local_workspace(),
            deferred_environment(),
            &activation.snapshot,
        ),
    )
    .await
    .unwrap();
    let stage = crate::session_mcp::StageMcpAttachment {
        workspace_id: host.run_assembly.local_workspace().into(),
        generation: crate::test_mcp::mcp_fence(session, "docs", 1, "worker-a", 1, u64::MAX),
        realization_id: "realize-docs".into(),
        stage_idempotency_key: "stage-docs".into(),
        name: "docs".into(),
        target: awaken_agent_contract::McpTarget::parse_http("https://docs.example.test/mcp")
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let envelope = awaken_session_configuration_contract::SessionRuntimeProjection {
        environment: deferred_environment(),
        tools: Some(Default::default()),
        mcp_stages: Some(vec![stage]),
    };
    queue
        .enqueue(
            awaken_dispatch_contract::RunDispatch::new(activation)
                .for_session(awaken_agent_contract::agent::thread::Id(session.into()))
                .with_session_runtime(awaken_dispatch_contract::SessionRuntimeEnvelope::new(
                    envelope.encode().unwrap(),
                ))
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from(host.run_assembly.local_workspace()),
                )),
        )
        .await
        .unwrap();
    clock.set(host.run_assembly.clock().now().as_epoch_ms());
    let claimed = queue
        .claim("worker-a", 30_000, &Default::default())
        .await
        .unwrap()
        .unwrap();
    assert!(awaken_dispatch_execution::session_worker_resolver::claimed_session::has_session_projection_authority(&claimed));
    source.armed.store(true, Ordering::SeqCst);
    let error = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver")
        .worker_for_claimed(&claimed)
        .await
        .err()
        .expect("lost original projection cannot construct a Worker");
    assert_eq!(
        source.removals.load(Ordering::SeqCst),
        1,
        "M3 actual original removal"
    );
    let calls = spy.0.lock().unwrap().len();
    assert_eq!(
        calls, 0,
        "Managed claim used ordinary MCP after original projection loss: calls={calls}"
    );
    assert!(
        matches!(error, awaken_dispatch::Error::ResolutionNotReady(_)),
        "E1: {error}"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read(session, |_| ())
            .expect("installed Sessions")
            .is_none(),
        "E2 no replacement root or Direct installed Session was manufactured"
    );
}
