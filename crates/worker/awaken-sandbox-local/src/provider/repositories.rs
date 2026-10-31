// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: Existing Repository effects retain the provider identity and one shared ownership rule; no proof claim.
//! Repository realization and publication for this provider.

use super::{LocalSandbox, pc, provision_repo_at, push_repo_to_at};
use async_trait::async_trait;

#[async_trait]
impl pc::RepositoryRealizer for LocalSandbox {
    async fn realize_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
        intent: pc::RepositoryRealizationIntent,
    ) -> Result<(), pc::SandboxError> {
        self.require_root_identity()?;
        plan.validate_mount_path()?;
        intent.validate_owned_path(plan, Some(&self.owned_paths.lock()))?;
        #[cfg(target_os = "linux")]
        if let Some(outcome) = self
            .scoped_repository(plan, None, credential, intent)
            .await?
        {
            match outcome.into_result()? {
                crate::repository_effect::Outcome::Realized => {
                    self.reserve_owned_path(&plan.mount_path);
                    return Ok(());
                }
                _ => {
                    return Err(pc::SandboxError::conflict(
                        "repository realization returned another operation's receipt",
                    ));
                }
            }
        }
        let (branch, commit) = intent.checkout_constraints(plan);
        provision_repo_at(
            &self.root,
            &plan.mount_path,
            &plan.transport_url,
            branch,
            commit,
            credential,
            intent,
        )?;
        self.reserve_owned_path(&plan.mount_path);
        Ok(())
    }

    async fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        expectation: &pc::RepositoryPublicationExpectation,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
        self.require_root_identity()
            .map_err(pc::RepositoryPublicationError::Unavailable)?;
        plan.validate_mount_path()
            .map_err(pc::RepositoryPublicationError::Unavailable)?;
        #[cfg(target_os = "linux")]
        if let Some(outcome) = self
            .scoped_repository(
                plan,
                Some(expectation),
                credential,
                pc::RepositoryRealizationIntent::Initialize,
            )
            .await
            .map_err(pc::RepositoryPublicationError::Unavailable)?
        {
            use crate::repository_effect::Outcome;
            return match outcome
                .into_result()
                .map_err(pc::RepositoryPublicationError::Unavailable)?
            {
                Outcome::Published(receipt)
                    if receipt == pc::RepositoryPublicationReceipt::new(plan, expectation) =>
                {
                    Ok(receipt)
                }
                Outcome::Rejected(rejection) => {
                    rejection.verify(expectation)?;
                    Err(pc::RepositoryPublicationError::Rejected(rejection))
                }
                _ => Err(pc::RepositoryPublicationError::Unavailable(
                    pc::SandboxError::conflict(
                        "repository publication returned another operation's receipt",
                    ),
                )),
            };
        }
        push_repo_to_at(&self.root, &plan.mount_path, plan, expectation, credential)
    }
}
