// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Component regression over the original Session projection and MCP owner.
// @formal reason: This classifies the actual failure/cleanup assertions, not a formal-proof claim.
#![cfg(test)]
//! Actual D240 removal followed by a legal successor, through the public Host API.

use crate::installed_session::InstalledSessions;
use crate::session_realizations::{SessionRealizationAccess, SessionRealizationSource};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const CONTROLLER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
const VISIT_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Copy)]
enum GatePoint {
    AfterCanonicalFinish,
    AfterAbsentVisit,
}

struct FinishGate {
    reached: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    released: Mutex<bool>,
    release: Condvar,
    actual_true: AtomicUsize,
    actual_absent: AtomicUsize,
    point: GatePoint,
}

impl FinishGate {
    fn after_actual_true(&self) {
        if !matches!(self.point, GatePoint::AfterCanonicalFinish) {
            return;
        }
        self.actual_true.fetch_add(1, Ordering::SeqCst);
        self.pause();
    }

    fn after_actual_absence(&self) {
        if matches!(self.point, GatePoint::AfterAbsentVisit)
            && self.actual_absent.fetch_add(1, Ordering::SeqCst) == 0
        {
            self.pause();
        }
    }

    fn pause(&self) {
        if let Some(reached) = self.reached.lock().unwrap().take() {
            let _ = reached.send(());
        }
        let released = self.released.lock().unwrap();
        let (released, _) = self
            .release
            .wait_timeout_while(released, VISIT_GATE_TIMEOUT, |released| !*released)
            .unwrap();
        let was_released = *released;
        drop(released);
        assert!(
            was_released,
            "true-CAS gate exceeded its bounded controller budget"
        );
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

struct ReleaseOnDrop(Arc<FinishGate>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Delegates every operation to the same original allocation. Only the return
/// of a successful real removal is delayed; its result and authority are unchanged.
struct GateAfterCanonicalFinish {
    inner: Arc<InstalledSessions>,
    session: &'static str,
    gate: Arc<FinishGate>,
}

#[cfg(test)]
impl SessionRealizationSource for GateAfterCanonicalFinish {
    fn session_ids(&self) -> Result<Vec<String>, crate::realization_error::RealizationLock> {
        SessionRealizationSource::session_ids(self.inner.as_ref())
    }

    fn acquire_realization<'a>(
        &'a self,
        session: &'a str,
        admit: &'a mut (dyn FnMut(SessionRealizationAccess<'_>) + Send),
    ) -> futures_util::future::BoxFuture<
        'a,
        awaken_session_realization_contract::SessionRealizationPermit,
    > {
        SessionRealizationSource::acquire_realization(self.inner.as_ref(), session, admit)
    }

    fn install(&self, session: &str) -> Result<(), crate::realization_error::RealizationLock> {
        SessionRealizationSource::install(self.inner.as_ref(), session)
    }

    fn visit(
        &self,
        session: &str,
        visit: &mut dyn FnMut(SessionRealizationAccess<'_>),
    ) -> Result<bool, crate::realization_error::RealizationLock> {
        let mut visited = false;
        let result = SessionRealizationSource::visit(self.inner.as_ref(), session, &mut |access| {
            visited = true;
            visit(access);
        });
        // The real source completed its original lookup and released its map
        // mutex. An unvisited successful non-creating lookup is actual absence;
        // no callback, output value or authority has been manufactured here.
        if result.is_ok() && !visited && session == self.session {
            self.gate.after_actual_absence();
        }
        result
    }

    fn replace_frozen(
        &self,
        session: &str,
        update: &mut dyn FnMut(&mut awaken_session_realization::frozen_inputs::FrozenSessionInputs),
    ) -> Result<(), crate::realization_error::RealizationLock> {
        SessionRealizationSource::replace_frozen(self.inner.as_ref(), session, update)
    }

    fn remove_session(
        &self,
        session: &str,
    ) -> Result<bool, crate::realization_error::RealizationLock> {
        let actual = SessionRealizationSource::remove_session(self.inner.as_ref(), session)?;
        if actual && session == self.session {
            self.gate.after_actual_true();
        }
        Ok(actual)
    }

    fn finish_realization_revocation(
        &self,
        session: &str,
        selected: &crate::session_realizations::SessionRealizationRevocationSelection,
        expected_owner: &crate::environment_owner::SessionEnvironmentOwner,
        disposed: bool,
    ) -> bool {
        let actual = SessionRealizationSource::finish_realization_revocation(
            self.inner.as_ref(),
            session,
            selected,
            expected_owner,
            disposed,
        );
        if actual && session == self.session {
            self.gate.after_actual_true();
        }
        actual
    }

    fn retain_publication(
        &self,
        session: &str,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), crate::realization_error::RealizationError> {
        SessionRealizationSource::retain_publication(self.inner.as_ref(), session, publication)
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
            self.inner.as_ref(),
            session,
            workspace,
            asserted,
            compiled,
            versions,
            effect_key,
        )
    }
}

fn projection() -> awaken_session_realization_contract::FrozenSessionProjection {
    let holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "test.worker",
    );
    let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                environment_id: "env".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                    "env-fingerprint".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_credential_contract::CredentialRealizationProfile {
                    inference_holder: holder.clone(),
                    mcp_holder: holder.clone(),
                    resource_holder: holder,
                },
            },
            runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: "agent-a".into(),
            agent_revision: None,
            model_override: None,
            model: "model".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    awaken_session_realization_contract::FrozenSessionProjection {
        workspace_id: "workspace".into(),
        revision: awaken_session_types::SessionRevision(2),
        baseline,
        agent_publication: None,
        environment_materialization: None,
        environment: Default::default(),
        resource_revision: 0,
        resources: Default::default(),
        previous_resource_manifest: Some(
            awaken_session_configuration_contract::resource::SessionResourceManifest::new(
                "workspace",
                awaken_session_configuration_contract::resource::ResolvedSessionResources::default(
                ),
            ),
        ),
        tools: Default::default(),
        mcp: Vec::new(),
        request_context: Vec::new(),
    }
}

async fn original_relay(host: &Arc<crate::SharedHost>) -> awaken_session_mcp::McpRelay {
    let authority =
        Arc::downgrade(host) as std::sync::Weak<dyn awaken_session_mcp::McpGenerationAuthority>;
    host.mcp.relay_or_start(authority).await.unwrap().clone()
}

async fn retire_component_current(
    host: &crate::SharedHost,
    session: &str,
) -> Result<bool, crate::realization_error::RealizationError> {
    host.realization_executor
        .environments()
        .retire_session_environment_for_revocation(
            session,
            (&host.mcp, host.installed_sessions.as_ref()),
            None,
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_retirement_cannot_remove_a_successor_session_relay_route() {
    tokio::time::timeout(CONTROLLER_TIMEOUT, async {
        // R1: original canonical removal actually returns true, after releasing the
        // slot lock. R2: a new allocation is admitted by the production projection
        // installer before the old public finish returns. R3: old successful cleanup
        // must not remove the new tenure's real relay route. No fabricated removal result is accepted.
        let session = "successful-retirement-successor-route";
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let gate = Arc::new(FinishGate {
            reached: Mutex::new(Some(reached_tx)),
            released: Mutex::new(false),
            release: Condvar::new(),
            actual_true: AtomicUsize::new(0),
            actual_absent: AtomicUsize::new(0),
            point: GatePoint::AfterCanonicalFinish,
        });
        let release = ReleaseOnDrop(gate.clone());
        let mut dependencies =
            crate::HostDependencies::ephemeral(Arc::new(crate::host::MemoryHostModel), "model")
                .await;
        let observed = Arc::new(std::sync::OnceLock::new());
        let observed_by_factory = observed.clone();
        let original_gate = gate.clone();
        dependencies.realization_source = Some(Box::new(move |canonical| {
            observed_by_factory
                .set(canonical.clone())
                .unwrap_or_else(|_| panic!("the canonical projection factory runs exactly once"));
            Arc::new(GateAfterCanonicalFinish {
                inner: canonical,
                session,
                gate: original_gate,
            })
        }));
        let host = dependencies
            .build()
            .expect("unchanged validated constructor");
        let canonical = observed
            .get()
            .expect("original constructor allocation observed")
            .clone();
        assert!(Arc::ptr_eq(&host.installed_sessions, &canonical));
        canonical.install(session);
        assert!(Arc::ptr_eq(
            &host
                .realization_executor
                .sessions()
                .realization_lock(session)
                .expect("installed Session"),
            &canonical
                .realizations()
                .realization_lock(session)
                .expect("installed Session"),
        ));
        let lease_a = crate::session_resource::SessionRealizationLease {
            owner: "worker".into(),
            runtime_incarnation: "worker/boot".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        };
        let generation_a = crate::test_mcp::mcp_fence(
            session,
            "docs",
            1,
            &lease_a.runtime_incarnation,
            lease_a.epoch,
            lease_a.expires_at_unix_ms,
        );
        let frozen = projection();
        tokio::time::timeout(INSTALL_TIMEOUT, async {
            let sessions = canonical.realizations();
            let _permit = sessions.acquire_realization(session).await;
            host.install_frozen_session_projection(
                session,
                frozen.clone(),
                None,
                true,
                Some(
                    awaken_session_realization::test_support::test_observed_lease(lease_a.clone()),
                ),
            )
            .await
            .expect("A uses the production frozen projection installer");
        })
        .await
        .expect("bounded complete original A projection install");
        let original = canonical
            .read(session, |slot| slot.realization.execution.phase.clone())
            .expect("A canonical allocation");
        let relay = original_relay(&host).await;
        let selected = host.begin_session_realization_revocation();
        let retiring = host.clone();
        let finish = tokio::spawn(async move {
            retiring
                .finish_session_realization_revocation(selected)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx)
            .await
            .expect("real canonical finish must reach actual true")
            .expect("post-removal witness");
        assert_eq!(gate.actual_true.load(Ordering::SeqCst), 1);
        assert!(
            canonical.read(session, |_| ()).is_none(),
            "A was actually removed"
        );
        assert!(
            !finish.is_finished(),
            "old successful public finish is gated"
        );
        let lease_b = crate::session_resource::SessionRealizationLease {
            epoch: awaken_lease_contract::fence::Epoch::try_from(lease_a.epoch)
                .unwrap()
                .next()
                .unwrap()
                .get(),
            ..lease_a
        };
        tokio::time::timeout(INSTALL_TIMEOUT, async {
            let sessions = canonical.realizations();
            let _permit = sessions.acquire_realization(session).await;
            host.install_frozen_session_projection(
                session,
                frozen,
                None,
                true,
                Some(
                    awaken_session_realization::test_support::test_observed_lease(lease_b.clone()),
                ),
            )
            .await
            .expect("B uses the same production installer after actual A removal");
        })
        .await
        .expect("bounded complete successor B projection install");
        let successor = canonical
            .read(session, |slot| slot.realization.execution.phase.clone())
            .expect("B canonical allocation");
        assert!(
            !Arc::ptr_eq(&original, &successor),
            "same Thread, new allocation"
        );
        let generation_b = crate::test_mcp::mcp_fence(
            session,
            "docs",
            2,
            &lease_b.runtime_incarnation,
            lease_b.epoch,
            lease_b.expires_at_unix_ms,
        );
        assert_eq!(generation_a.session_id(), generation_b.session_id());
        assert_ne!(generation_a.tenure(), generation_b.tenure());
        let (upstream, seen) = tokio::time::timeout(HTTP_TIMEOUT, crate::test_mcp::start(None))
            .await
            .expect("bounded real MCP fixture listener startup");
        let material = awaken_session_mcp::McpTransportMaterial {
            name: "docs".into(),
            prompts_as_skills: false,
            transport: awaken_session_mcp::McpTransportMaterialKind::Http {
                url: upstream,
                bearer: None,
                refresh: None,
            },
        };
        assert_eq!(
            host.mcp_generation_root_authority(&generation_b),
            Some(true)
        );
        assert!(relay.stage_route(&generation_b, &material));
        let url = relay.route_url(&generation_b).unwrap();
        let client = reqwest::Client::new();
        let invoke = || {
            let request = client
                .post(url.clone())
                .json(&serde_json::json!({
                    "jsonrpc":"2.0", "id":1, "method":"tools/list",
                }))
                .send();
            async move {
                tokio::time::timeout(HTTP_TIMEOUT, request)
                    .await
                    .expect("bounded complete real successor HTTP witness")
            }
        };
        assert_eq!(invoke().await.unwrap().status(), reqwest::StatusCode::OK);
        let before = seen.lock().unwrap().len();
        assert_eq!(before, 1, "B reaches the real upstream before old finish");
        drop(release);
        let retired = tokio::time::timeout(std::time::Duration::from_secs(5), finish)
            .await
            .expect("post-removal gate is released")
            .unwrap()
            .unwrap();
        let status = invoke().await.unwrap().status();
        let after = seen.lock().unwrap().len();
        assert_eq!(
            retired, 1,
            "actual true retirement must contribute exactly one"
        );
        assert_eq!(gate.actual_true.load(Ordering::SeqCst), 1);
        assert_eq!(
            host.mcp_generation_root_authority(&generation_b),
            Some(true)
        );
        assert!(Arc::ptr_eq(
            &successor,
            &canonical
                .read(session, |slot| slot.realization.execution.phase.clone())
                .unwrap(),
        ));
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "successful old retirement must preserve the successor relay route"
        );
        assert_eq!(after, before + 1, "B still reaches the real upstream");
    })
    .await
    .expect("bounded complete true-CAS race controller");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_component_retirement_cannot_close_or_remove_a_later_session() {
    tokio::time::timeout(CONTROLLER_TIMEOUT, async {
        // Component entrance with no selected Session: its original source
        // actually observes absence. B then installs through the production
        // projection installer before that old lookup returns to its caller.
        // Cleanup of absence must not subsequently select B by Thread name.
        let session = "absent-retirement-successor";
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let gate = Arc::new(FinishGate {
            reached: Mutex::new(Some(reached_tx)), released: Mutex::new(false),
            release: Condvar::new(), actual_true: AtomicUsize::new(0),
            actual_absent: AtomicUsize::new(0), point: GatePoint::AfterAbsentVisit,
        });
        let release = ReleaseOnDrop(gate.clone());
        let observed = Arc::new(std::sync::OnceLock::new());
        let factory_observed = observed.clone();
        let factory_gate = gate.clone();
        let mut dependencies = crate::HostDependencies::ephemeral(
            Arc::new(crate::NoModelConfiguredExecutor), "stub",
        ).await;
        dependencies.realization_source = Some(Box::new(move |canonical| {
            factory_observed.set(canonical.clone()).unwrap_or_else(|_| panic!("one canonical source factory"));
            Arc::new(GateAfterCanonicalFinish { inner: canonical, session, gate: factory_gate })
        }));
        let host = dependencies.build().unwrap();
        let canonical = observed.get().unwrap().clone();
        assert!(Arc::ptr_eq(&canonical, &host.installed_sessions));
        assert!(canonical.read(session, |_| ()).is_none());
        let retiring = host.clone();
        let finish = tokio::spawn(async move { retire_component_current(&retiring, session).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), reached_rx)
            .await.expect("actual absent source lookup reaches its post-return gate")
            .expect("original lookup witness");
        assert_eq!(gate.actual_absent.load(Ordering::SeqCst), 1);
        assert!(canonical.read(session, |_| ()).is_none());
        assert!(!finish.is_finished());
        let lease = crate::session_resource::SessionRealizationLease {
            owner: "worker".into(), runtime_incarnation: "worker/boot".into(),
            epoch: 2, expires_at_unix_ms: u64::MAX,
        };
        tokio::time::timeout(INSTALL_TIMEOUT, async {
            let sessions = canonical.realizations();
            let _permit = sessions.acquire_realization(session).await;
            host.install_frozen_session_projection(
                session, projection(), None, true,
                Some(awaken_session_realization::test_support::test_observed_lease(lease.clone())),
            ).await.expect("B uses the original complete projection installer after real absence");
        }).await.expect("bounded complete successor installation");
        let successor = canonical.read(session, |slot| slot.realization.execution.phase.clone()).unwrap();
        let generation = crate::test_mcp::mcp_fence(
            session, "docs", 1, &lease.runtime_incarnation, lease.epoch, lease.expires_at_unix_ms,
        );
        assert_eq!(host.mcp_generation_root_authority(&generation), Some(true));
        drop(release);
        let retired = tokio::time::timeout(std::time::Duration::from_secs(5), finish)
            .await.expect("absence gate released").unwrap().unwrap();
        let remains = canonical.read(session, |slot| Arc::ptr_eq(&successor, &slot.realization.execution.phase)).unwrap_or(false);
        let authority = host.mcp_generation_root_authority(&generation);
        assert!(remains && authority == Some(true),
            "retirement that observed actual absence selected the later Session: remains={remains}, authority={authority:?}");
        assert!(!retired, "absence contributes no successful retirement");
        assert_eq!(gate.actual_absent.load(Ordering::SeqCst), 1);
    }).await.expect("bounded actual-absence counterexample controller");
}
