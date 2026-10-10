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

mod projection_receipt;
mod state;
pub(crate) use projection_receipt::FrozenProjectionReceipt;
pub(crate) use state::{SessionRealizationAuthority, SessionRealizationState};

/// What the realizer may learn of a Session's run layer, and the only two
/// changes it may make to it.
pub(crate) trait SessionRunQuery {
    /// A run context is currently built for the Session.
    fn holds_context(&self) -> bool;
    /// The Session's dispatch crosses the claimed Session realization boundary.
    fn session_dispatch(&self) -> bool;
    /// The durable dispatch claim the Session currently serves.
    fn dispatch_claim(&self) -> Option<&awaken_dispatch_contract::RunClaim>;
    /// The realization the run context was built on changed: drop the context
    /// so the next run rebuilds it.
    fn drop_context(&mut self);
    /// Mark the Session's dispatch as entered through the Session application.
    fn enter_session_dispatch(&mut self);
}

/// One installed Session as the realizer sees it, borrowed under the lock.
pub(crate) struct SessionRealizationAccess<'a> {
    pub(crate) frozen: &'a Arc<FrozenSessionInputs>,
    pub(crate) realization: &'a mut SessionRealizationState,
    pub(crate) run: &'a mut dyn SessionRunQuery,
}

/// Where the host keeps installed Sessions, as the realizer reaches them.
pub(crate) trait SessionRealizationSource: Send + Sync {
    /// Visit one Session under the lock; with `create`, an absent Session is
    /// installed empty first, otherwise an absent Session is not visited.
    fn visit(
        &self,
        session: &str,
        create: bool,
        visit: &mut dyn FnMut(SessionRealizationAccess<'_>),
    );
}

/// The realizer's handle on installed Sessions (see the module docs).
#[derive(Clone)]
pub(crate) struct SessionRealizations(Arc<dyn SessionRealizationSource>);

impl SessionRealizations {
    pub(crate) fn new(source: Arc<dyn SessionRealizationSource>) -> Self {
        Self(source)
    }

    fn visit<R>(
        &self,
        session: &str,
        create: bool,
        f: impl FnOnce(SessionRealizationAccess<'_>) -> R,
    ) -> Option<R> {
        let (mut f, mut result) = (Some(f), None);
        self.0.visit(session, create, &mut |access| {
            result = f.take().map(|f| f(access));
        });
        result
    }

    /// Change one Session, installing it empty when absent.
    pub(crate) fn update_session<R>(
        &self,
        session: &str,
        f: impl FnOnce(SessionRealizationAccess<'_>) -> R,
    ) -> R {
        self.visit(session, true, f)
            .expect("an installed Session is always visited")
    }

    /// Change one Session only if it is installed.
    pub(crate) fn modify_session<R>(
        &self,
        session: &str,
        f: impl FnOnce(SessionRealizationAccess<'_>) -> R,
    ) -> Option<R> {
        self.visit(session, false, f)
    }

    pub(crate) fn close_mcp_realization_admission(
        &self,
        session: &str,
        fence: awaken_session_mcp::McpQuiescenceAdmissionFence,
    ) -> Result<bool, crate::host_error::HostError> {
        self.update_session(session, |slot| slot.realization.mcp.close_admission(fence))
            .map_err(crate::host_error::HostError::from)
    }

    #[cfg(test)]
    pub(crate) fn mcp_realization_admitted(&self, session: &str) -> bool {
        self.modify_session(session, |slot| slot.realization.mcp.admits_realization())
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
            let mut prompts = slot.realization.resources.prompts.clone();
            let delivery = slot.realization.content_delivery.unwrap_or_default();
            let environment = slot.realization.environment_owner.resident();
            let path_view = match environment.as_deref() {
                Some(environment) => environment
                    .agent_tools()
                    .agent_prompt_path_view(working_directory, has_acp, has_native)
                    .map_err(str::to_owned)?,
                None => Default::default(),
            };
            prompts.extend(
                slot.realization.resources
                    .resource_prompts
                    .iter()
                    .map(|prompt| prompt.render(delivery, path_view.clone())),
            );
            if let Some(baseline) = &slot.frozen.baseline {
                prompts.extend(baseline.prompts.iter()
                    .filter(|prompt| !awaken_session_configuration_contract::SessionContextSeed::is_seed_prompt(prompt))
                    .cloned());
            }
            if let Some(skill_prompt) = &slot.realization.skill_prompt {
                prompts.push(skill_prompt.render(&path_view, environment.as_deref())?);
            }
            Ok(prompts)
        })
        .unwrap_or_else(|| Ok(Vec::new()))
    }

    /// Hold one Session's Environment `lifecycle` lock, creating its slot.
    pub(crate) async fn lock_lifecycle(&self, session: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lifecycle = self.update_session(session, |slot| slot.realization.lifecycle.clone());
        lifecycle.lock_owned().await
    }

    /// Hold one Session's `resource_projection` lock, creating its slot.
    pub(crate) async fn lock_resource_projection(
        &self,
        session: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let projection =
            self.update_session(session, |slot| slot.realization.resource_projection.clone());
        projection.lock_owned().await
    }

    #[cfg(test)]
    pub fn realization_lock(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.update_session(session, |slot| slot.realization.realization.clone())
    }

    /// The immutable Agent publication retained for one Session.
    pub(crate) fn published_snapshot(
        &self,
        session: &str,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        self.frozen(session).published_snapshot.clone()
    }

    /// The Session's current frozen inputs, taken out of the lock as one
    /// value. An unknown Session reads as the empty projection.
    pub(crate) fn frozen(&self, session: &str) -> Arc<FrozenSessionInputs> {
        self.modify_session(session, |slot| slot.frozen.clone())
            .unwrap_or_default()
    }

    /// Read only the realization layer of one Session's slot.
    pub(crate) fn read_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&SessionRealizationState) -> R,
    ) -> Option<R> {
        self.modify_session(session, |slot| f(slot.realization))
    }

    /// Change only the realization layer, creating the Session's slot.
    pub(crate) fn update_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRealizationState) -> R,
    ) -> R {
        self.update_session(session, |slot| f(slot.realization))
    }

    /// Change only the realization layer of an existing slot.
    pub(crate) fn modify_realization<R>(
        &self,
        session: &str,
        f: impl FnOnce(&mut SessionRealizationState) -> R,
    ) -> Option<R> {
        self.modify_session(session, |slot| f(slot.realization))
    }

    /// Drop the Session's run context; see [`SessionRunQuery::drop_context`].
    pub(crate) fn drop_run_context(&self, session: &str) {
        self.update_session(session, |slot| slot.run.drop_context());
    }

    /// Terminal disposal ends the run context and the Environment's Resource
    /// reconciliation together, under one lock.
    pub(crate) fn end_terminal_disposal(&self, session: &str) {
        self.update_session(session, |slot| {
            slot.run.drop_context();
            slot.realization.environment_resource_reconciliation =
                crate::staged_resources::EnvironmentResourceReconciliation::None;
        });
    }

    /// See [`SessionRunQuery::enter_session_dispatch`].
    pub(crate) fn enter_session_dispatch(&self, session: &str) {
        self.update_session(session, |slot| slot.run.enter_session_dispatch());
    }

    /// The durable dispatch claim the Session currently serves.
    pub(crate) fn dispatch_claim(
        &self,
        session: &str,
    ) -> Option<awaken_dispatch_contract::RunClaim> {
        self.modify_session(session, |access| access.run.dispatch_claim().cloned())
            .flatten()
    }
}

#[cfg(test)]
mod tests;
