// @formal surface obligation=sandbox.workspace_path_projection_is_one_logical_root_and_never_discloses_provider_host_paths
// @formal reason: Repository effects reuse the exact namespace root and shared ownership predicate; no proof claim.
//! Repository realization and publication in the namespace provider.

use super::{NamespaceSandbox, pc, provision_repo_at, push_repo_to_at, workspace_relative};

impl NamespaceSandbox {
    pub fn provision_repo(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
        intent: pc::RepositoryRealizationIntent,
    ) -> Result<(), pc::SandboxError> {
        self.require_root_identity()?;
        plan.validate_mount_path()?;
        intent.validate_owned_path(
            plan,
            Some(&self.owned_paths.lock().expect("owned paths lock poisoned")),
        )?;
        let (branch, commit) = intent.checkout_constraints(plan);
        provision_repo_at(
            &self.workspace_root(),
            workspace_relative(&plan.mount_path),
            &plan.transport_url,
            branch,
            commit,
            credential,
            intent,
        )?;
        self.reserve_owned_path(&plan.mount_path);
        Ok(())
    }

    pub fn push_repo(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        expectation: &pc::RepositoryPublicationExpectation,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
        self.require_root_identity()
            .map_err(pc::RepositoryPublicationError::Unavailable)?;
        plan.validate_mount_path()
            .map_err(pc::RepositoryPublicationError::Unavailable)?;
        push_repo_to_at(
            &self.workspace_root(),
            workspace_relative(&plan.mount_path),
            plan,
            expectation,
            credential,
        )
    }
}
