//! Repository projection adapter for a realized Session environment.

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use super::{SessionEnvironment, container_repositories};

#[async_trait]
impl pc::RepositoryRealizer for SessionEnvironment {
    async fn realize_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => {
                pc::RepositoryRealizer::realize_repository(sandbox.as_ref(), plan, credential).await
            }
            Self::Namespace { sandbox, .. } => sandbox.provision_repo(
                &plan.mount_path,
                &plan.transport_url,
                plan.initial_branch.as_deref(),
                plan.initial_commit.as_deref(),
                credential,
            ),
            Self::Container { sandbox, .. } => {
                container_repositories::provision(
                    sandbox.as_ref(),
                    &plan.mount_path,
                    &plan.transport_url,
                    plan.initial_branch.as_deref(),
                    plan.initial_commit.as_deref(),
                    credential,
                )
                .await
            }
        }
    }

    async fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        expectation: &pc::RepositoryPublicationExpectation,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<pc::RepositoryPublicationReceipt, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => {
                pc::RepositoryRealizer::publish_repository(
                    sandbox.as_ref(),
                    plan,
                    expectation,
                    credential,
                )
                .await
            }
            Self::Namespace { sandbox, .. } => sandbox.push_repo(plan, expectation, credential),
            Self::Container { sandbox, .. } => {
                container_repositories::push(sandbox.as_ref(), plan, expectation, credential).await
            }
        }
    }
}
