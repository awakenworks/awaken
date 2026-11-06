// @formal surface exclude=composition_adapter
// @formal reason: Edge-facing port for a current hosted Workspace-purpose fact.
// @formal reason: Authority stays with the selected deployment's Workspace owner.
//! Ordinary public entrypoints require a positive Workspace-purpose observation.

/// The selected deployment authority could not observe current purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspacePurposeUnavailable;

/// Observes the original hosted Workspace owner; authenticates no caller.
#[async_trait::async_trait]
pub trait OrdinaryWorkspaceAdmission: Send + Sync {
    /// `true` is positive evidence for this exact active ordinary Workspace.
    /// Private, unknown and inactive Workspaces return `false`. An unavailable
    /// authority returns an error; a previous observation cannot replace it.
    async fn is_ordinary_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<bool, WorkspacePurposeUnavailable>;
}
