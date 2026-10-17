// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-executor/src/host/session/environment_realization/fence_source_tests.rs
// @formal owner crates/server/awaken-session-executor/src/realization_composed_tests/session_realizations/execution_authority.rs
// @formal reason: Original lease and direct provider-boundary fixtures, reachable only through cfg(test) or explicit test-support. The provider entry returns the original private kernel Future; it introduces no execution grant or alternate effect implementation. The in-memory installed-Session registry implements the same SessionRealizationSource port for the realization crate's absence and poison decision tables and grants no authority.
//! Original lease observation fixtures, never execution authority.

/// Host fixtures use the actual Control-clock coordinate for live replies.
/// Deliberately rejects expired input: an expiry test must construct and age
/// its request observation explicitly, never silently manufacture fresh TTL.
use crate::realization_error::RealizationError;
pub fn test_lease_receipt(
    lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
) -> awaken_session_realization_contract::SessionRealizationLeaseReceipt {
    awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
        lease,
        wall_clock_now_ms(),
    )
    .expect("live Control-clock fixture lease")
}

pub fn test_observed_lease(
    lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
) -> awaken_session_realization_contract::ObservedSessionRealizationLease {
    let started = awaken_authority_clock::authority_proof_started().unwrap();
    test_lease_receipt(lease).observed(started)
}

/// Deliberately aged evidence for negative Host tests. Keep the exact durable
/// fence bytes, but exhaust the local proof on the real platform clock before
/// returning it. This is never the fallback for a failed live fixture.
pub fn test_expired_observed_lease(
    lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
) -> awaken_session_realization_contract::ObservedSessionRealizationLease {
    let started = awaken_authority_clock::authority_proof_started().unwrap();
    let proof = awaken_session_realization_contract::SessionRealizationLeaseReceipt {
        lease,
        remaining_ms: std::num::NonZeroU64::new(1).unwrap(),
    }
    .observed(started);
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert!(
        proof
            .remaining_at(awaken_authority_clock::authority_proof_started().unwrap())
            .is_none()
    );
    proof
}

/// Direct fixture access to the original provider-boundary Future. W1-W7 in
/// Host's fence_source_tests exercise its real warmup/admission/recheck order;
/// this adds no poll layer, state, provider or execution grant.
pub fn create_session_environment<'a>(
    executor: &'a crate::realization_executor::RealizationExecutor,
    provider: &'a crate::session_environment::SessionEnvironmentProvider,
    spec: &'a awaken_provisioning_contract::SandboxSpec,
    effect_fence: Option<&'a awaken_provisioning_contract::SandboxEffectFence>,
    authority: Option<&'a dyn awaken_provisioning_contract::CurrentExecutionAuthority>,
    source_handle: Option<&'a awaken_provisioning_contract::SandboxHandle>,
    intent: awaken_session_environment_contract::EnvironmentRealizationKind,
) -> impl std::future::Future<
    Output = Result<crate::session_environment::SessionEnvironment, RealizationError>,
> + Send
+ 'a {
    executor
        .environments()
        .create_session_environment_with_provider_effect(
            executor.peers(),
            provider,
            spec,
            effect_fence,
            authority,
            source_handle,
            intent,
        )
}

/// The current wall-clock time in Unix milliseconds, for fixtures: they sample the canonical wall-clock port. Production
/// realization continues to read the Host-selected clock already injected.
pub fn wall_clock_now_ms() -> u64 {
    use awaken_time::WallClockExt as _;
    let clock = awaken_time::system_clock();
    clock.now_ms()
}

/// An in-memory installed-Session registry for the realization crate's own
/// decision tests. It implements the same [`SessionRealizationSource`] port
/// the host implements: frozen-input writes and the realization permit are the
/// install path, every other operation leaves an absent Session absent, and a
/// poisoned registry denies.
///
/// [`SessionRealizationSource`]: crate::session_realizations::SessionRealizationSource
#[derive(Default)]
pub struct InMemorySessionRealizations {
    sessions: std::sync::Mutex<std::collections::HashMap<String, InMemorySession>>,
}

#[derive(Default)]
struct InMemorySession {
    inputs: std::sync::Arc<crate::frozen_inputs::FrozenSessionInputs>,
    realization: crate::realization_state::SessionRealization,
    run: InMemoryRun,
}

#[derive(Default)]
struct InMemoryRun {
    context: bool,
    session_dispatch: bool,
    claim: Option<awaken_dispatch_contract::RunClaim>,
}

impl crate::session_realizations::SessionRunQuery for InMemoryRun {
    fn holds_context(&self) -> bool {
        self.context
    }

    fn has_active_run(&self) -> bool {
        false
    }

    fn session_dispatch(&self) -> bool {
        self.session_dispatch
    }

    fn dispatch_claim(&self) -> Option<&awaken_dispatch_contract::RunClaim> {
        self.claim.as_ref()
    }

    fn drop_context(&mut self) {
        self.context = false;
    }

    fn enter_session_dispatch(&mut self) {
        self.session_dispatch = true;
    }

    fn install_dispatch_claim(&mut self, claim: awaken_dispatch_contract::RunClaim) {
        self.claim = Some(claim);
    }
}

type Registry = std::collections::HashMap<String, InMemorySession>;

impl InMemorySessionRealizations {
    fn registry(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Registry>, crate::realization_error::RealizationLock>
    {
        self.sessions
            .lock()
            .map_err(|_| crate::realization_error::RealizationLock::InstalledSessions)
    }

    /// The realization executor's view of this registry.
    pub fn realizations(
        self: &std::sync::Arc<Self>,
    ) -> crate::session_realizations::SessionRealizations {
        crate::session_realizations::SessionRealizations::new(self.clone())
    }

    /// Whether `session` is installed.
    pub fn contains(&self, session: &str) -> bool {
        self.registry()
            .is_ok_and(|registry| registry.contains_key(session))
    }

    /// Install `session` and edit its realization state, as a fixture does.
    pub fn install_realization(
        &self,
        session: &str,
        edit: impl FnOnce(&mut crate::realization_state::SessionRealization),
    ) {
        let mut registry = self.registry().expect("fixture registry is healthy");
        edit(&mut registry.entry(session.to_owned()).or_default().realization);
    }

    /// Panic while holding the registry lock, exactly as a failed writer does.
    pub fn poison(&self) {
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = self.sessions.lock();
            panic!("a registry writer failed");
        }));
        assert!(poisoned.is_err() && self.sessions.is_poisoned());
    }
}

impl crate::session_realizations::SessionRealizationSource for InMemorySessionRealizations {
    fn session_ids(&self) -> Result<Vec<String>, crate::realization_error::RealizationLock> {
        let mut ids = self.registry()?.keys().cloned().collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    }

    fn remove_session(
        &self,
        session: &str,
    ) -> Result<bool, crate::realization_error::RealizationLock> {
        let removed = self.registry()?.remove(session);
        if let Some(removed) = &removed {
            removed.realization.end_on_removal();
        }
        Ok(removed.is_some())
    }

    fn acquire_realization<'a>(
        &'a self,
        session: &'a str,
        admit: &'a mut (
                    dyn FnMut(crate::session_realizations::SessionRealizationAccess<'_>) + Send
                ),
    ) -> futures_util::future::BoxFuture<
        'a,
        awaken_session_realization_contract::SessionRealizationPermit,
    > {
        Box::pin(async move {
            awaken_session_realization_contract::SessionRealizationPermit::acquire(
                &awaken_session_types::SessionId::from(session),
                || {
                    let mut registry = self.registry().expect("permit fixture registry is healthy");
                    let installed = registry.entry(session.to_owned()).or_default();
                    let phase = installed.realization.execution().phase.clone();
                    async move {
                        let owner = phase.clone().lock_owned().await;
                        (phase, owner)
                    }
                },
                |identity| {
                    let mut registry = self.registry().expect("permit fixture registry is healthy");
                    let installed = registry.get_mut(session)?;
                    std::sync::Arc::ptr_eq(&installed.realization.execution().phase, identity).then(
                        || {
                            admit(crate::session_realizations::SessionRealizationAccess {
                                inputs: &installed.inputs,
                                realization: &mut installed.realization,
                                run: &mut installed.run,
                            })
                        },
                    )
                },
            )
            .await
            .0
        })
    }

    fn install(&self, session: &str) -> Result<(), crate::realization_error::RealizationLock> {
        self.registry()?.entry(session.to_owned()).or_default();
        Ok(())
    }

    fn visit(
        &self,
        session: &str,
        visit: &mut dyn FnMut(crate::session_realizations::SessionRealizationAccess<'_>),
    ) -> Result<bool, crate::realization_error::RealizationLock> {
        let mut registry = self.registry()?;
        let Some(installed) = registry.get_mut(session) else {
            return Ok(false);
        };
        visit(crate::session_realizations::SessionRealizationAccess {
            inputs: &installed.inputs,
            realization: &mut installed.realization,
            run: &mut installed.run,
        });
        Ok(true)
    }

    fn finish_realization_revocation(
        &self,
        session: &str,
        selected: &crate::session_realizations::SessionRealizationRevocationSelection,
        expected_owner: &crate::environment_owner::SessionEnvironmentOwner,
        disposed: bool,
    ) -> bool {
        let Ok(mut registry) = self.registry() else {
            return false;
        };
        let completed = registry.get(session).is_some_and(|installed| {
            selected.matches_completion(&installed.realization, expected_owner, disposed)
        });
        if completed {
            registry.remove(session);
        }
        completed
    }

    fn replace_frozen(
        &self,
        session: &str,
        update: &mut dyn FnMut(&mut crate::frozen_inputs::FrozenSessionInputs),
    ) -> Result<(), crate::realization_error::RealizationLock> {
        let mut registry = self.registry()?;
        let installed = registry.entry(session.to_owned()).or_default();
        let mut next = installed.inputs.as_ref().clone();
        update(&mut next);
        installed.inputs = std::sync::Arc::new(next);
        Ok(())
    }

    fn retain_publication(
        &self,
        session: &str,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), crate::realization_error::RealizationError> {
        let mut registry = self.registry()?;
        let installed = registry.entry(session.to_owned()).or_default();
        installed.inputs.validate_publication(publication)?;
        if let Some(publication) = publication {
            let mut next = installed.inputs.as_ref().clone();
            next.published_snapshot = Some(publication.clone());
            installed.inputs = std::sync::Arc::new(next);
        }
        Ok(())
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
        let mut registry = self.registry()?;
        let installed = registry
            .get_mut(session)
            .ok_or_else(|| crate::realization_error::RealizationError::session_absent(session))?;
        installed
            .realization
            .validate_preparation_publication(asserted)?;
        let mut next = installed.inputs.as_ref().clone();
        next.workspace = Some(workspace.to_owned());
        installed.inputs = std::sync::Arc::new(next);
        installed.realization.staging_mut().resources = compiled.staged;
        installed.realization.staging_mut().memory_bindings = compiled.memory_bindings;
        installed.realization.staging_mut().skills = Some(versions);
        installed.realization.staging_mut().effect_key = Some(effect_key);
        Ok(())
    }
}
