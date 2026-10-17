// @formal surface requirement=session.managed_worker.work_authority
// @formal reason: this view keeps Frozen inputs readonly and publishes resource
// @formal reason: requirements through the original slot lock and realization lease.
//! The realizer's view of the Sessions installed in this process.
//!
//! The host keeps each installed Session as three layers under one lock:
//! frozen inputs, the realization layer and the run layer. The realizer reaches
//! them only through [`SessionRealizations`]: the frozen inputs read-only, the
//! realization layer read/write, and of the run layer a narrow query plus two
//! named operations, each applied under the same lock as the realization edit
//! it accompanies.

use std::sync::Arc;

use crate::frozen_inputs::FrozenSessionInputs;
use crate::host_error::HostError;
use awaken_session_application_contract::DelegatedRun;

pub mod execution_authority;
pub mod projection_receipt;
pub mod resource_preparation;
pub use resource_preparation::ResourcePreparation;
pub mod state;
pub use execution_authority::{ExecutionAuthority, ExecutionAuthorityState};
pub use projection_receipt::FrozenProjectionReceipt;
pub use state::{SessionRealization, SessionRealizationAuthority};

/// An ephemeral selection of the existing allocation and closed authority.
/// It owns no new counter or admission law. Callers capture it while borrowing
/// the canonical Session mutation, and compare it again at exact completion.
pub struct SessionRealizationRevocationSelection {
    pub realization: Arc<tokio::sync::Mutex<()>>,
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    authority: SessionRealizationAuthority,
}

impl SessionRealizationRevocationSelection {
    /// Close and select the original authority under its canonical slot lock.
    pub fn close(
        realization: &mut SessionRealization,
        expected: Option<&awaken_session_configuration_contract::resource::SessionRealizationLease>,
    ) -> Option<Self> {
        realization
            .close_authority(expected)
            .then(|| Self::from_realization(realization))
    }

    /// Terminal absence closes this original physical root's generation,
    /// accepting only its monotonic lease renewal; ordinary exact-lease
    /// revocation keeps the strict [`Self::close`] policy. The slot owner calls
    /// this inside its canonical mutation, so the check, the closure and any
    /// Run controls it captures beside it observe one root.
    pub fn close_generation(
        realization: &mut SessionRealization,
        expected: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        phase: &Arc<tokio::sync::Mutex<()>>,
    ) -> Option<Self> {
        if !Arc::ptr_eq(phase, &realization.execution.phase) {
            return None;
        }
        let current = match &realization.execution.authority {
            SessionRealizationAuthority::Live(proof) => &proof.lease,
            SessionRealizationAuthority::Closed(Some(lease)) => lease,
            SessionRealizationAuthority::Uninstalled
            | SessionRealizationAuthority::Closed(None) => return None,
        };
        if !awaken_session_realization_contract::realization_lease_generation_authorizes(
            current, expected,
        ) {
            return None;
        }
        SessionRealizationRevocationSelection::close(realization, None)
    }

    /// Capture this original allocation during an already-held mutation.
    /// The selection is cleanup identity, not proof of process death.
    pub fn from_realization(realization: &SessionRealization) -> Self {
        Self {
            realization: realization.execution.phase.clone(),
            lifecycle: realization.environment.lifecycle.clone(),
            authority: realization.execution.authority.clone(),
        }
    }

    /// Check the complete selected allocation and physical completion shape.
    /// This pure rule must be consumed inside the canonical slot mutation;
    /// it grants neither disposal nor process-stop authority.
    pub fn matches_completion(
        &self,
        realization: &SessionRealization,
        expected_owner: &crate::environment_owner::SessionEnvironmentOwner,
        disposed: bool,
    ) -> bool {
        if !self.matches(realization) {
            return false;
        }
        use crate::environment_owner::{
            RetiringEnvironmentOwner, SessionEnvironmentOwner, SessionEnvironmentRestoration,
        };
        match (&realization.environment.owner, expected_owner) {
            (SessionEnvironmentOwner::Vacant, SessionEnvironmentOwner::Vacant) => true,
            (SessionEnvironmentOwner::Vacant, SessionEnvironmentOwner::Retiring(expected)) => {
                disposed && matches!(&expected.owned, RetiringEnvironmentOwner::Unbound(_))
            }
            (
                SessionEnvironmentOwner::Retiring(current),
                SessionEnvironmentOwner::Retiring(expected),
            ) => current.exact_matches(expected),
            (
                SessionEnvironmentOwner::Restoring(SessionEnvironmentRestoration::Awaiting {
                    request: current,
                }),
                SessionEnvironmentOwner::Restoring(SessionEnvironmentRestoration::Awaiting {
                    request: expected,
                }),
            ) => current == expected,
            _ => false,
        }
    }

    /// Require both original lock allocations and the original closed authority.
    pub fn matches(&self, realization: &SessionRealization) -> bool {
        Arc::ptr_eq(&self.realization, &realization.execution.phase)
            && Arc::ptr_eq(&self.lifecycle, &realization.environment.lifecycle)
            && self.authority == realization.execution.authority
    }
}

/// The one local Primary/delegated activity combination. Callers retain their
/// original Slot query adapter and already-held lifecycle scope; the delegated
/// future is polled only after Primary, even when Primary was active. This is
/// local activity, never a physical-stop proof or Environment authorization.
pub async fn primary_or_delegated_run_active(
    thread: &str,
    mut active: impl FnMut(&str) -> bool,
    delegated: impl std::future::Future<Output = Result<Vec<DelegatedRun>, HostError>>,
) -> Result<bool, HostError> {
    let primary_active = active(thread);
    let delegated = delegated.await?;
    // Evaluate children even when Primary was active: read errors/poison and
    // the original status-before-query order must not be short-circuited.
    let delegated_active = delegated
        .iter()
        .any(|run| run.status.occupies_parallel_slot() && active(&run.run_id.0));
    Ok(primary_active || delegated_active)
}

/// What the realizer may learn of a Session's run layer, and the only two
/// changes it may make to it.
pub trait SessionRunQuery {
    /// A run context is currently built for the Session.
    fn holds_context(&self) -> bool;
    /// The cached Runtime currently drives a physical Run (not merely a context).
    fn has_active_run(&self) -> bool;
    /// The Session's dispatch crosses the claimed Session realization boundary.
    fn session_dispatch(&self) -> bool;
    /// The durable dispatch claim the Session currently serves.
    fn dispatch_claim(&self) -> Option<&awaken_dispatch_contract::RunClaim>;
    /// The realization the run context was built on changed: drop the context
    /// so the next run rebuilds it.
    fn drop_context(&mut self);
    /// Mark the Session's dispatch as entered through the Session application.
    fn enter_session_dispatch(&mut self);
    fn install_dispatch_claim(&mut self, claim: awaken_dispatch_contract::RunClaim);
}

/// One installed Session as the realizer sees it, borrowed under the lock.
pub struct SessionRealizationAccess<'a> {
    pub inputs: &'a Arc<FrozenSessionInputs>,
    pub realization: &'a mut SessionRealization,
    pub run: &'a mut dyn SessionRunQuery,
}

/// Where the host keeps installed Sessions, as the realizer reaches them.
pub trait SessionRealizationSource: Send + Sync {
    /// Existing registry operations; all keep the original slot owner/lock.
    fn session_ids(&self) -> Vec<String>;
    fn remove_session(&self, session: &str) -> bool;

    /// Enter the installed-slot owner's canonical permit kernel. This is the
    /// same exclusion and replacement retry that Host admission uses.
    fn acquire_realization<'a>(
        &'a self,
        session: &'a str,
        admit: &'a mut (dyn FnMut(SessionRealizationAccess<'_>) + Send),
    ) -> futures_util::future::BoxFuture<
        'a,
        awaken_session_realization_contract::SessionRealizationPermit,
    >;

    /// Visit one Session under the lock; with `create`, an absent Session is
    /// installed empty first, otherwise an absent Session is not visited. A
    /// poisoned lock is reported, never unwound through.
    fn visit(
        &self,
        session: &str,
        create: bool,
        visit: &mut dyn FnMut(SessionRealizationAccess<'_>),
    ) -> Result<(), InstalledSessionsPoisoned>;

    /// Complete only the captured allocation/authority and exact physical owner.
    /// The canonical slot implementation performs the comparison and mutation
    /// together. Absence is false, never permission to select a later slot.
    fn finish_realization_revocation(
        &self,
        session: &str,
        selected: &SessionRealizationRevocationSelection,
        expected_owner: &crate::environment_owner::SessionEnvironmentOwner,
        disposed: bool,
    ) -> bool;

    /// Validate and retain an exact immutable publication under the same slot
    /// lock. This is the only publication write available to the selector;
    /// ordinary realization visitors still cannot mutate frozen inputs.
    /// Replace frozen inputs through their original slot owner.
    fn replace_frozen(&self, session: &str, update: &mut dyn FnMut(&mut FrozenSessionInputs));

    fn retain_publication(
        &self,
        session: &str,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), HostError>;

    /// Publish one completed requirements compilation under the original slot
    /// lock. Frozen inputs remain readonly to ordinary visit callbacks.
    fn publish_resource_requirements(
        &self,
        session: &str,
        workspace: &str,
        asserted: Option<&awaken_session_configuration_contract::resource::SessionRealizationLease>,
        compiled: crate::staged_resources::CompiledEffectiveInputs,
        versions: Vec<awaken_resource_contract::SkillVersion>,
        effect_key: String,
    ) -> Result<(), awaken_session_types::RunError>;
}

/// A writer panicked while holding the installed Sessions' lock.
#[derive(Debug)]
pub struct InstalledSessionsPoisoned;

/// The realizer's handle on installed Sessions (see the module docs).
#[derive(Clone)]
pub struct SessionRealizations(Arc<dyn SessionRealizationSource>);

impl SessionRealizations {
    /// This view never holds a second copy of the frozen inputs.
    pub fn replace_frozen_with(
        &self,
        session: &str,
        mut update: impl FnMut(&mut FrozenSessionInputs),
    ) {
        self.0.replace_frozen(session, &mut update);
    }

    pub fn session_ids(&self) -> Vec<String> {
        self.0.session_ids()
    }

    pub fn remove_session(&self, session: &str) -> bool {
        self.0.remove_session(session)
    }

    /// Hold the Session's current realization allocation, revalidating any
    /// removal/recreation during the wait through the canonical slot owner.
    pub async fn acquire_realization(
        &self,
        session: &str,
    ) -> awaken_session_realization_contract::SessionRealizationPermit {
        self.acquire_realization_with(session, |_| ()).await.0
    }

    /// Observe/admit through the slot owner's same acquire callback. Reading
    /// after acquiring a permit would leave a remove/recreate gap between the
    /// identity check and the observation; the original owner closes that gap.
    pub async fn acquire_realization_with<R: Send>(
        &self,
        session: &str,
        mut admit: impl FnMut(SessionRealizationAccess<'_>) -> R + Send,
    ) -> (
        awaken_session_realization_contract::SessionRealizationPermit,
        R,
    ) {
        let mut result = None;
        let permit = self
            .0
            .acquire_realization(session, &mut |slot| result = Some(admit(slot)))
            .await;
        (
            permit,
            result.expect("canonical permit acquisition admits its current slot"),
        )
    }

    /// Apply the canonical slot owner's exact post-I/O cleanup CAS.
    pub fn finish_realization_revocation(
        &self,
        session: &str,
        selected: &SessionRealizationRevocationSelection,
        expected_owner: &crate::environment_owner::SessionEnvironmentOwner,
        disposed: bool,
    ) -> bool {
        self.0
            .finish_realization_revocation(session, selected, expected_owner, disposed)
    }

    pub fn retain_publication(
        &self,
        session: &str,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), HostError> {
        self.0.retain_publication(session, publication)
    }

    pub fn publish_resource_requirements(
        &self,
        session: &str,
        workspace: &str,
        asserted: Option<&awaken_session_configuration_contract::resource::SessionRealizationLease>,
        compiled: crate::staged_resources::CompiledEffectiveInputs,
        versions: Vec<awaken_resource_contract::SkillVersion>,
        effect_key: String,
    ) -> Result<(), awaken_session_types::RunError> {
        self.0.publish_resource_requirements(
            session, workspace, asserted, compiled, versions, effect_key,
        )
    }

    pub fn new(source: Arc<dyn SessionRealizationSource>) -> Self {
        Self(source)
    }

    fn visit<R>(
        &self,
        session: &str,
        create: bool,
        f: impl FnOnce(SessionRealizationAccess<'_>) -> R,
    ) -> Option<R> {
        let (mut f, mut result) = (Some(f), None);
        self.0
            .visit(session, create, &mut |access| {
                result = f.take().map(|f| f(access));
            })
            .expect("installed sessions mutex poisoned");
        result
    }

    /// Change one Session, installing it empty when absent.
    pub fn update_session<R>(
        &self,
        session: &str,
        f: impl FnOnce(SessionRealizationAccess<'_>) -> R,
    ) -> R {
        self.visit(session, true, f)
            .expect("an installed Session is always visited")
    }

    /// Change one Session only if it is installed.
    pub fn modify_session<R>(
        &self,
        session: &str,
        f: impl FnOnce(SessionRealizationAccess<'_>) -> R,
    ) -> Option<R> {
        self.visit(session, false, f)
    }

    pub fn close_mcp_realization_admission(
        &self,
        session: &str,
        fence: awaken_session_mcp::McpQuiescenceAdmissionFence,
    ) -> Result<bool, crate::host_error::HostError> {
        Ok(self.update_session(session, |slot| {
            slot.realization.staging.mcp.close_admission(fence)
        })?)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn mcp_realization_admitted(&self, session: &str) -> bool {
        self.modify_session(session, |slot| {
            slot.realization.staging.mcp.admits_realization()
        })
        .unwrap_or(true)
    }

    /// Read the one current process-local projection of all frozen prompt
    /// inputs. The durable baseline/resource aggregates remain authoritative;
    /// executors call this at attempt time so late realization cannot leave a
    /// construction-time prompt snapshot behind.
    pub fn prompts(
        &self,
        session: &str,
        working_directory: Option<&str>,
        has_acp: bool,
        has_native: bool,
    ) -> Result<Vec<String>, String> {
        self.modify_session(session, |slot| {
            let mut prompts = slot.realization.staging.resources.prompts.clone();
            let delivery = slot.realization.staging.content_delivery.unwrap_or_default();
            let environment = slot.realization.environment.owner.resident();
            let path_view = match environment.as_deref() {
                Some(environment) => environment
                    .agent_tools()
                    .agent_prompt_path_view(working_directory, has_acp, has_native)
                    .map_err(str::to_owned)?,
                None => Default::default(),
            };
            prompts.extend(
                slot.realization.staging.resources
                    .resource_prompts
                    .iter()
                    .map(|prompt| prompt.render(delivery, path_view.clone())),
            );
            if let Some(baseline) = &slot.inputs.baseline {
                prompts.extend(baseline.prompts.iter()
                    .filter(|prompt| !awaken_session_configuration_contract::SessionContextSeed::is_seed_prompt(prompt))
                    .cloned());
            }
            if let Some(skill_prompt) = &slot.realization.staging.skill_prompt {
                prompts.push(skill_prompt.render(&path_view, environment.as_deref())?);
            }
            Ok(prompts)
        })
        .unwrap_or_else(|| Ok(Vec::new()))
    }

    /// Hold one Session's Environment `lifecycle` lock, creating its slot.
    pub async fn lock_lifecycle(&self, session: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lifecycle = self.update_session(session, |slot| {
            slot.realization.environment.lifecycle.clone()
        });
        lifecycle.lock_owned().await
    }

    /// Hold one Session's `resource_projection` lock, creating its slot.
    pub async fn lock_resource_projection(
        &self,
        session: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let projection = self.update_realization(session, |r| r.staging.projection.clone());
        projection.lock_owned().await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn realization_lock(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.update_session(session, |slot| slot.realization.execution.phase.clone())
    }

    /// The immutable Agent publication retained for one Session.
    pub fn published_snapshot(
        &self,
        session: &str,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        self.frozen(session).published_snapshot.clone()
    }

    /// Exact registered ownership; an unknown Thread has no inherited Scope.
    pub fn registered_workspace(&self, session: &str) -> Option<String> {
        self.frozen(session).workspace.clone()
    }

    /// The credential profile remains a projection of the frozen Environment.
    pub fn credential_realization(
        &self,
        session: &str,
    ) -> Option<awaken_credential_contract::CredentialRealizationProfile> {
        self.frozen(session)
            .environment_snapshot
            .as_ref()
            .map(|environment| environment.credential_realization.clone())
    }

    /// Read registered ownership with the selected process fallback for flat protocols.
    pub fn workspace(&self, session: &str, fallback: &str) -> String {
        self.registered_workspace(session)
            .unwrap_or_else(|| fallback.to_owned())
    }

    /// The Session's current frozen inputs, taken out of the lock as one
    /// value. An unknown Session reads as the empty projection.
    pub fn frozen(&self, session: &str) -> Arc<FrozenSessionInputs> {
        self.modify_session(session, |slot| slot.inputs.clone())
            .unwrap_or_default()
    }

    /// Read only the realization layer of one Session's slot.
    pub fn read_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&SessionRealization) -> R,
    ) -> Option<R> {
        self.modify_session(session, |slot| f(slot.realization))
    }

    /// Change only the realization layer, creating the Session's slot.
    pub fn update_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRealization) -> R,
    ) -> R {
        self.update_session(session, |slot| f(slot.realization))
    }

    /// Change only the realization layer of an existing slot.
    pub fn modify_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRealization) -> R,
    ) -> Option<R> {
        self.modify_session(session, |slot| f(slot.realization))
    }

    /// Drop the Session's run context; see [`SessionRunQuery::drop_context`].
    pub fn drop_run_context(&self, session: &str) {
        self.update_session(session, |slot| slot.run.drop_context());
    }

    /// Terminal disposal ends the run context and the Environment's Resource
    /// reconciliation together, under one lock.
    pub fn end_terminal_disposal(&self, session: &str) {
        self.update_session(session, |slot| {
            slot.run.drop_context();
            slot.realization.staging.reconciliation =
                crate::staged_resources::EnvironmentResourceReconciliation::None;
        });
    }

    /// See [`SessionRunQuery::enter_session_dispatch`].
    pub fn enter_session_dispatch(&self, session: &str) {
        self.update_session(session, |slot| slot.run.enter_session_dispatch());
    }

    /// The durable dispatch claim the Session currently serves.
    pub fn dispatch_claim(&self, session: &str) -> Option<awaken_dispatch_contract::RunClaim> {
        self.modify_session(session, |access| access.run.dispatch_claim().cloned())
            .flatten()
    }
}

#[cfg(test)]
mod tests;
