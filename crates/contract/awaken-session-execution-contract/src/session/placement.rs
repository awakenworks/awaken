// @formal surface requirement=session.lifecycle.session_state
// @formal reason: One composition selects where its Sessions are realized: a co-located Worker's realization
// @formal reason: roles, or the admission of Worker-placed Sessions' frozen inputs only. A process without a
// @formal reason: co-located Worker can never reach a realization effect. Classification, not a proof claim.
//! Where the Sessions of one process are realized.
//!
//! A process with a co-located Worker realizes its Sessions itself: it holds
//! the realization, MCP-effect, Environment and terminal-cleanup roles
//! ([`ColocatedRealizationRoles`]). A process whose Sessions only registered
//! Workers realize (a split Coordinator) holds none of them; it only admits
//! each Session's frozen inputs ([`SessionInputAdmission`]): the dispatch
//! projection its Runs are lowered over, and the layout, Skill and tool
//! validation a Session's configuration is checked by.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    EnvironmentRuntime, RealizationEffects, RealizationRuntime, RunError, TerminalCleanup,
};

/// The frozen-input admission of Worker-placed Sessions, for a process that
/// realizes no Session itself.
#[async_trait]
pub trait SessionInputAdmission: Send + Sync {
    /// Install the frozen root projection a Session's Runs are lowered over.
    async fn install_dispatch_projection(
        &self,
        thread: &str,
        projection: crate::FrozenSessionProjection,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure>;

    /// Validate a Session's sandbox layout by its structural rules.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &crate::SessionSandboxLayout,
    ) -> Result<(), RunError>;

    /// Resolve the Skill bindings a Session selects to exact pinned versions.
    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<crate::ResolvedSkillBinding>, RunError>;

    /// Replace a Session's complete tool configuration.
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: crate::SessionToolConfiguration,
    ) -> Result<(), RunError>;
}

/// The roles of a co-located Worker that realizes this process's Sessions.
#[derive(Clone)]
pub struct ColocatedRealizationRoles {
    pub mcp: Arc<dyn crate::McpAttachmentRealizer>,
    pub realization: Arc<dyn RealizationRuntime>,
    pub realization_effects: Arc<dyn RealizationEffects>,
    pub environment: Arc<dyn EnvironmentRuntime>,
    pub terminal_cleanup: Arc<dyn TerminalCleanup>,
}

/// Where this process's Sessions are realized, selected once by the
/// composition.
///
/// Decision table (operation → effect per placement):
/// | Rule | operation | co-located Worker | registered Workers only |
/// |---|---|---|---|
/// | P1 | install a dispatch projection | the realization installs it in Dispatch mode | the input admission installs it |
/// | P2 | validate a layout / resolve Skills / replace tools | the realization roles | the input admission |
/// | P3 | renew local realization leases | the realization renews its leases | nothing is leased here: none renewed |
/// | P4 | any realization effect | the co-located roles | refused: no Session is realized here |
#[derive(Clone)]
pub enum SessionPlacementRoles {
    /// A co-located Worker realizes Sessions in this process.
    Colocated(ColocatedRealizationRoles),
    /// Only registered Workers realize Sessions; this process admits their
    /// frozen inputs.
    RegisteredWorkers(Arc<dyn SessionInputAdmission>),
}

/// The refusal of a realization effect in a process that realizes no Session.
pub const NO_COLOCATED_WORKER: &str =
    "this process realizes no Session: the Session requires a co-located Worker";

impl SessionPlacementRoles {
    /// The co-located Worker's realization roles, when this process has one.
    #[must_use]
    pub fn colocated(&self) -> Option<&ColocatedRealizationRoles> {
        match self {
            Self::Colocated(roles) => Some(roles),
            Self::RegisteredWorkers(_) => None,
        }
    }

    /// The co-located Worker's realization roles, refused (P4) when this
    /// process realizes no Session.
    pub fn require_colocated(&self) -> Result<&ColocatedRealizationRoles, RunError> {
        self.colocated().ok_or_else(|| {
            RunError::unavailable_classified("session_colocated_worker_absent", NO_COLOCATED_WORKER)
        })
    }

    /// P4: the co-located realization role.
    pub fn realization(&self) -> Result<&dyn RealizationRuntime, RunError> {
        Ok(self.require_colocated()?.realization.as_ref())
    }

    /// P4: the co-located realization-effect role.
    pub fn realization_effects(&self) -> Result<&dyn RealizationEffects, RunError> {
        Ok(self.require_colocated()?.realization_effects.as_ref())
    }

    /// P1: install the frozen root projection a Session's Runs are lowered over.
    pub async fn install_dispatch_projection(
        &self,
        thread: &str,
        projection: crate::FrozenSessionProjection,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        match self {
            Self::Colocated(roles) => {
                roles
                    .realization
                    .install_session_projection(
                        thread,
                        projection,
                        crate::SessionProjectionInstallMode::Dispatch,
                    )
                    .await
            }
            Self::RegisteredWorkers(inputs) => {
                inputs.install_dispatch_projection(thread, projection).await
            }
        }
    }

    /// P2: validate a Session's sandbox layout.
    pub fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &crate::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        match self {
            Self::Colocated(roles) => roles
                .realization_effects
                .validate_session_sandbox_layout(thread, layout),
            Self::RegisteredWorkers(inputs) => {
                inputs.validate_session_sandbox_layout(thread, layout)
            }
        }
    }

    /// P2: resolve a Session's Skill bindings to exact pinned versions.
    pub async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<crate::ResolvedSkillBinding>, RunError> {
        match self {
            Self::Colocated(roles) => {
                roles
                    .environment
                    .resolve_session_skills(workspace_id, skills)
                    .await
            }
            Self::RegisteredWorkers(inputs) => {
                inputs.resolve_session_skills(workspace_id, skills).await
            }
        }
    }

    /// P2: replace a Session's complete tool configuration.
    pub async fn replace_session_tools(
        &self,
        thread: &str,
        tools: crate::SessionToolConfiguration,
    ) -> Result<(), RunError> {
        match self {
            Self::Colocated(roles) => roles.environment.replace_session_tools(thread, tools).await,
            Self::RegisteredWorkers(inputs) => inputs.replace_session_tools(thread, tools).await,
        }
    }

    /// P3: renew the realization leases this process holds.
    pub async fn renew_session_realization_projections(
        &self,
        control: &dyn crate::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, RunError> {
        match self {
            Self::Colocated(roles) => {
                roles
                    .realization
                    .renew_session_realization_projections(
                        control,
                        owner,
                        runtime_incarnation,
                        timing,
                    )
                    .await
            }
            Self::RegisteredWorkers(_) => Ok(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frozen-input admission that names itself in every answer.
    struct NamedInputs;

    #[async_trait]
    impl SessionInputAdmission for NamedInputs {
        async fn install_dispatch_projection(
            &self,
            _thread: &str,
            _projection: crate::FrozenSessionProjection,
        ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure>
        {
            Err(RunError::bad_request("input admission install").into())
        }

        fn validate_session_sandbox_layout(
            &self,
            _thread: &str,
            _layout: &crate::SessionSandboxLayout,
        ) -> Result<(), RunError> {
            Err(RunError::bad_request("input admission layout"))
        }

        async fn resolve_session_skills(
            &self,
            _workspace_id: &str,
            _skills: &[awaken_agent_contract::AgentSkillBinding],
        ) -> Result<Vec<crate::ResolvedSkillBinding>, RunError> {
            Err(RunError::bad_request("input admission skills"))
        }

        async fn replace_session_tools(
            &self,
            _thread: &str,
            _tools: crate::SessionToolConfiguration,
        ) -> Result<(), RunError> {
            Err(RunError::bad_request("input admission tools"))
        }
    }

    fn colocated() -> SessionPlacementRoles {
        let neutral = Arc::new(crate::NeutralRuntime);
        SessionPlacementRoles::Colocated(ColocatedRealizationRoles {
            mcp: Arc::new(crate::NeutralRuntime),
            realization: neutral.clone(),
            realization_effects: neutral.clone(),
            environment: neutral.clone(),
            terminal_cleanup: neutral,
        })
    }

    #[tokio::test]
    async fn placement_routes_each_operation_by_its_decision_table() {
        let registered = SessionPlacementRoles::RegisteredWorkers(Arc::new(NamedInputs));

        // P2: input operations reach the input admission only without a
        // co-located Worker, and the co-located roles otherwise.
        let tools = registered
            .replace_session_tools("session", crate::SessionToolConfiguration::default())
            .await
            .expect_err("P2 tools");
        assert_eq!(tools.message, "input admission tools", "P2 tools");
        assert_eq!(
            registered
                .resolve_session_skills("workspace", &[])
                .await
                .expect_err("P2 skills")
                .message,
            "input admission skills",
            "P2 skills"
        );
        let colocated = colocated();
        assert_eq!(
            colocated
                .resolve_session_skills("workspace", &[])
                .await
                .expect("P2 co-located skills"),
            Vec::new(),
            "P2 co-located skills"
        );
        assert_ne!(
            colocated
                .replace_session_tools("session", crate::SessionToolConfiguration::default())
                .await
                .expect_err("P2 co-located tools")
                .message,
            "input admission tools",
            "P2 co-located tools"
        );

        // P4: a realization effect needs a co-located Worker.
        assert!(colocated.require_colocated().is_ok(), "P4 co-located");
        assert!(registered.colocated().is_none(), "P4");
        let refused = registered.require_colocated().err().expect("P4 refused");
        assert_eq!(refused.code, "session_colocated_worker_absent", "P4");
        assert_eq!(refused.message, NO_COLOCATED_WORKER, "P4");
    }
}
