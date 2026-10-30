// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Physical effect authorization is a view of the existing Worker and Session
//! owners, never another lease or lifecycle. Lock order: Worker -> installed
//! Sessions -> GO. A poisoned lock denies the effect.

use crate::realization_error::{AuthorityLoss, RealizationError};
use std::sync::{Arc, Weak};
use std::{io, time::Duration};

use awaken_provisioning_contract::{CurrentExecutionAuthority, SandboxEffectFence};
use awaken_session_configuration_contract::resource::SessionRealizationLease;

use crate::realization_state::SessionRealization;
use crate::session_environment::AuthorizedSessionEnvironmentEffect;
#[cfg(target_os = "linux")]
use crate::session_environment::SessionEnvironment;
use crate::session_realizations::{SessionRealizationSource, SessionRealizations};

/// The authority every physical Environment effect of the realization executor
/// runs under: the Worker's registry proof (absent on a direct Host) intersected
/// with the Session's live realization lease.
#[derive(Clone)]
pub struct ExecutionAuthority {
    sessions: SessionRealizations,
    /// Worker-owned registry proof. Direct Hosts have no Worker registration;
    /// every registered Worker installs this before enabling dispatch.
    worker: Option<Arc<dyn CurrentExecutionAuthority>>,
}

pub struct SessionExecutionAuthority {
    worker: Option<Arc<dyn CurrentExecutionAuthority>>,
    sessions: Weak<dyn SessionRealizationSource>,
    session: String,
    realization: Arc<tokio::sync::Mutex<()>>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    asserted: SessionRealizationLease,
    target: SessionEffectTarget,
}

/// The same authority guards two different targets. A physical execution
/// additionally needs an exact enrolled Environment; a provider effect may
/// precede Environment creation and instead pins its immutable operation.
enum SessionEffectTarget {
    #[cfg(target_os = "linux")]
    Execution(Weak<SessionEnvironment>),
    Provider(SandboxEffectFence),
}

fn denied() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}

impl CurrentExecutionAuthority for SessionExecutionAuthority {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
        self.with_current_session(&mut |_, remaining| effect(remaining))
    }

    // Linux has an Execution target that this effect must reject; other hosts
    // only have Provider. Keep one match for the shared authorization path.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(clippy::infallible_destructuring_match)
    )]
    fn with_current_effect(
        &self,
        asserted: &SandboxEffectFence,
        effect: &mut dyn FnMut(&SandboxEffectFence, Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        let initial = match &self.target {
            SessionEffectTarget::Provider(initial) => initial,
            #[cfg(target_os = "linux")]
            SessionEffectTarget::Execution(_) => return Err(denied()),
        };
        if !initial.same_effect_identity(asserted)
            || asserted.expires_at_unix_ms() < initial.expires_at_unix_ms()
        {
            return Err(denied());
        }
        let mut invoked = false;
        self.with_current_session(&mut |lease, remaining| {
            let current = lease
                .sandbox_effect_fence(initial.operation_id())
                .map_err(io::Error::other)?;
            if invoked
                || !asserted.same_effect_identity(&current)
                || current.expires_at_unix_ms() < asserted.expires_at_unix_ms()
            {
                return Err(denied());
            }
            invoked = true;
            effect(&current, remaining)
        })?;
        if !invoked {
            return Err(denied());
        }
        Ok(())
    }
}

impl SessionExecutionAuthority {
    /// Original composed tests exercise the actual installed Session, Worker proof and
    /// Environment Arc. This helper installs no registry or authority.
    #[cfg(all(target_os = "linux", any(test, feature = "test-support")))]
    pub fn for_test(
        execution: &ExecutionAuthority,
        session: &str,
        realization: &SessionRealization,
        asserted: SessionRealizationLease,
        environment: &Arc<SessionEnvironment>,
    ) -> Self {
        Self {
            worker: execution.worker.clone(),
            sessions: execution.sessions.downgrade(),
            session: session.into(),
            realization: realization.execution().phase.clone(),
            lifecycle: realization.environment().lifecycle.clone(),
            asserted,
            target: SessionEffectTarget::Execution(Arc::downgrade(environment)),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn asserted_for_test(&self) -> &SessionRealizationLease {
        &self.asserted
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn asserted_for_test_mut(&mut self) -> &mut SessionRealizationLease {
        &mut self.asserted
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn worker_for_test_mut(&mut self) -> &mut Option<Arc<dyn CurrentExecutionAuthority>> {
        &mut self.worker
    }

    fn with_current_session(
        &self,
        effect: &mut dyn FnMut(&SessionRealizationLease, Duration) -> io::Result<()>,
    ) -> io::Result<()> {
        match &self.worker {
            Some(worker) => worker
                .with_current(&mut |remaining| self.with_session_current(effect, Some(remaining))),
            None => self.with_session_current(effect, None),
        }
    }
    fn with_session_current(
        &self,
        effect: &mut dyn FnMut(&SessionRealizationLease, Duration) -> io::Result<()>,
        worker_remaining: Option<Duration>,
    ) -> io::Result<()> {
        if worker_remaining.is_some_and(|remaining| remaining.is_zero()) {
            return Err(denied());
        }
        let sessions = self.sessions.upgrade().ok_or_else(denied)?;
        let mut outcome = Err(denied());
        sessions
            .visit(&self.session, &mut |access| {
                outcome = self.effect_under_lock(access.realization, effect, worker_remaining);
            })
            .map_err(|_| denied())?;
        outcome
    }

    fn effect_under_lock(
        &self,
        realization: &SessionRealization,
        effect: &mut dyn FnMut(&SessionRealizationLease, Duration) -> io::Result<()>,
        worker_remaining: Option<Duration>,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(&self.realization, &realization.execution().phase)
            || !Arc::ptr_eq(&self.lifecycle, &realization.environment().lifecycle)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "installed Session was replaced",
            ));
        }
        match &self.target {
            #[cfg(target_os = "linux")]
            SessionEffectTarget::Execution(expected) => {
                let environment = expected.upgrade().ok_or_else(denied)?;
                let owned = realization
                    .environment()
                    .owner
                    .execution_environment()
                    .ok_or_else(denied)?;
                if !Arc::ptr_eq(&environment, owned) {
                    return Err(denied());
                }
            }
            SessionEffectTarget::Provider(_) => {}
        }
        let Some(session_remaining) = realization
            .execution()
            .authority
            .remaining_for(&self.asserted)
        else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Session realization proof is no longer live",
            ));
        };
        let remaining =
            worker_remaining.map_or(session_remaining, |worker| worker.min(session_remaining));
        // Both original guards remain held through the synchronous effect.
        // Provider conversion anchors BOOTTIME before this callback, so a
        // wait for either mutex cannot mint additional physical lifetime.
        effect(
            realization
                .execution()
                .authority
                .lease()
                .ok_or_else(denied)?,
            remaining,
        )
    }
}

impl ExecutionAuthority {
    pub fn new(
        sessions: SessionRealizations,
        worker: Option<Arc<dyn CurrentExecutionAuthority>>,
    ) -> Self {
        Self { sessions, worker }
    }

    /// The installed Sessions the authority reads the live lease from.
    pub fn sessions(&self) -> &SessionRealizations {
        &self.sessions
    }

    /// Lock order: Worker proof -> installed Sessions -> synchronous provider grant.
    /// No async owner lock or external I/O may be held by this callback.
    /// A denial is a [`RealizationError`]; the effect keeps its own error type.
    pub fn authorize_worker_effect<T, E: From<RealizationError>>(
        &self,
        effect: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let Some(authority) = &self.worker else {
            return effect();
        };
        let mut effect = Some(effect);
        let mut result = None;
        authority
            .with_current(&mut |remaining| {
                if remaining.is_zero() {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                let effect = effect
                    .take()
                    .ok_or_else(|| io::Error::other("authority callback repeated"))?;
                result = Some(effect());
                Ok(())
            })
            .map_err(|error| {
                RealizationError::authority_lost(
                    AuthorityLoss::WorkerDenied,
                    format!("Worker execution authority denied: {error}"),
                )
            })?;
        result.ok_or_else(|| RealizationError::invariant("Worker authority omitted the effect"))?
    }

    fn capture(
        &self,
        session: &str,
        realization: &SessionRealization,
        asserted: &SessionRealizationLease,
        target: SessionEffectTarget,
    ) -> SessionExecutionAuthority {
        SessionExecutionAuthority {
            worker: self.worker.clone(),
            sessions: self.sessions.downgrade(),
            session: session.into(),
            realization: realization.execution().phase.clone(),
            lifecycle: realization.environment().lifecycle.clone(),
            asserted: asserted.clone(),
            target,
        }
    }

    /// Capture one pre-Environment target through the existing authority
    /// implementation. The returned source and synchronous effect port share
    /// the same weak Session view, lease, Worker proof and held-lock decision.
    pub fn provider_effect_authority(
        &self,
        session: &str,
        initial: &SandboxEffectFence,
    ) -> Result<impl CurrentExecutionAuthority + use<>, RealizationError> {
        let mut capture = Some(|realization: &SessionRealization| {
            let lease = realization.execution().authority.lease().ok_or_else(|| {
                RealizationError::authority_lost(
                    AuthorityLoss::Closed,
                    "Session realization proof is no longer live",
                )
            })?;
            let current = lease
                .sandbox_effect_fence(initial.operation_id())
                .map_err(RealizationError::from)?;
            if !initial.same_effect_identity(&current)
                || current.expires_at_unix_ms() < initial.expires_at_unix_ms()
            {
                return Err(RealizationError::authority_lost(
                    AuthorityLoss::GenerationChanged,
                    "Session realization generation changed",
                ));
            }
            Ok(self.capture(
                session,
                realization,
                lease,
                SessionEffectTarget::Provider(initial.clone()),
            ))
        });
        let mut outcome = None;
        self.sessions.source().visit(session, &mut |access| {
            outcome = capture.take().map(|capture| capture(access.realization));
        })?;
        outcome.ok_or_else(|| RealizationError::session_absent(session))?
    }

    /// Called while the caller holds the installed Sessions' lock and has
    /// enrolled the exact candidate. Terminal-only owners with no live proof stay
    /// selected-but-unbound; they may clean up, but can never start work.
    #[cfg(target_os = "linux")]
    pub fn bind(
        &self,
        session: &str,
        realization: &SessionRealization,
        environment: &Arc<SessionEnvironment>,
    ) -> Result<(), RealizationError> {
        let Some(scoped) = environment.scoped_execution() else {
            return Ok(());
        };
        if !scoped.needs_execution_authority() {
            return Ok(());
        }
        let Some(asserted) = realization.execution().authority.lease() else {
            return Ok(());
        };
        scoped
            .bind_execution_authority(Arc::new(self.capture(
                session,
                realization,
                asserted,
                SessionEffectTarget::Execution(Arc::downgrade(environment)),
            )))
            .map_err(RealizationError::from)?;
        Ok(())
    }
}

impl ExecutionAuthority {
    /// Compile a new request from the installed projection. This is input
    /// construction only; the binding authority must still admit it durably.
    pub fn new_environment_effect_intent(
        &self,
        thread: &str,
        kind: awaken_session_contract::SessionEnvironmentEffectKind,
        source_binding: Option<&str>,
    ) -> Result<awaken_session_contract::SessionEnvironmentEffectIntent, RealizationError> {
        let (realization, environment_fingerprint) = self
            .sessions
            .modify_session(thread, |installed| {
                (
                    installed.realization.execution().authority.lease().cloned(),
                    installed
                        .inputs
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.environment.config_fingerprint.0.clone()),
                )
            })?
            .unwrap_or_default();
        Ok(
            awaken_session_contract::SessionEnvironmentEffectIntent::new(thread, kind, realization)
                .for_frozen_coordinates(source_binding.map(str::to_owned), environment_fingerprint),
        )
    }

    /// Re-enter current authority after I/O while preserving the Root's exact
    /// original intent. Renewal is a delivery grant, not a new receipt identity.
    pub fn current_environment_receipt(
        &self,
        binding: String,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<awaken_session_contract::SessionEnvironmentReceipt, RealizationError> {
        match (&effect.provider_fence, &effect.provider_authority) {
            (Some(initial), Some(authority)) => {
                awaken_provisioning_contract::with_current_sandbox_effect(
                    initial,
                    authority.as_ref(),
                    |_| Ok(()),
                )
                .map_err(RealizationError::from)?;
            }
            (None, None) => {}
            _ => {
                return Err(RealizationError::invariant(
                    "Environment effect lacks its captured authority",
                ));
            }
        }
        awaken_session_contract::SessionEnvironmentReceipt::from_intent(
            effect.authorization.intent(),
            binding,
        )
        .map_err(RealizationError::from)
    }
}
